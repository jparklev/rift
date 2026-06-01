use rift::{Create, Error, Manager};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Request {
    database: Option<PathBuf>,
    #[serde(flatten)]
    command: Command,
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Command {
    Init {
        at: PathBuf,
    },
    Create {
        from: PathBuf,
        name: Option<String>,
        into: Option<PathBuf>,
    },
    Remove {
        at: PathBuf,
        all: Option<bool>,
    },
    List {
        of: PathBuf,
    },
    Ancestors {
        of: PathBuf,
    },
    Gc,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Value {
    Empty(()),
    Path(Option<PathBuf>),
    Paths(Vec<PathBuf>),
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Ok { value: Value },
    Error { error: Failure },
}

#[derive(Serialize)]
struct Failure {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
}

impl Failure {
    fn protocol(code: &'static str, message: String) -> Self {
        Self {
            code,
            message,
            path: None,
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        let (code, path) = match &error {
            Error::Io(_) => ("io", None),
            Error::Database(_) => ("database", None),
            Error::Walk(_) => ("walk", None),
            Error::Path(_) => ("invalid_path", None),
            Error::CowUnavailable(_) => ("cow_unavailable", None),
            Error::InitializationRequired(path) => ("initialization_required", Some(path.clone())),
            Error::WorkspaceNotInitialized(path) => {
                ("workspace_not_initialized", Some(path.clone()))
            }
            Error::MissingMarker(path) => ("missing_marker", Some(path.clone())),
            Error::UnsupportedEntry(path) => ("unsupported_entry", Some(path.clone())),
            Error::UnsafeGit(_) => ("unsafe_git", None),
            Error::NotManaged(path) => ("not_managed", Some(path.clone())),
            Error::MarkerMismatch(path) => ("marker_mismatch", Some(path.clone())),
            Error::UnknownMarker(path) => ("unknown_marker", Some(path.clone())),
            Error::AlreadyExists(path) => ("already_exists", Some(path.clone())),
            Error::MissingRift(path) => ("missing_rift", Some(path.clone())),
            Error::InsideSource(path) => ("inside_source", Some(path.clone())),
        };
        Self {
            code,
            message: error.to_string(),
            path,
        }
    }
}

fn execute(input: &str) -> Result<Value, Failure> {
    let request: Request = serde_json::from_str(input)
        .map_err(|error| Failure::protocol("invalid_request", error.to_string()))?;
    let mut manager = match request.database {
        Some(path) => Manager::open(path),
        None => Manager::open_default(),
    }
    .map_err(Failure::from)?;
    match request.command {
        Command::Init { at } => manager
            .init(at)
            .map(|_| Value::Empty(()))
            .map_err(Failure::from),
        Command::Create { from, name, into } => manager
            .create(Create { from, name, into })
            .map(|path| Value::Path(Some(path)))
            .map_err(Failure::from),
        Command::Remove { at, all } => {
            if all.unwrap_or(false) {
                manager
                    .remove_all(at)
                    .map(Value::Paths)
                    .map_err(Failure::from)
            } else {
                manager
                    .remove(at)
                    .map(|()| Value::Empty(()))
                    .map_err(Failure::from)
            }
        }
        Command::List { of } => manager.list(of).map(Value::Paths).map_err(Failure::from),
        Command::Ancestors { of } => manager
            .ancestors(of)
            .map(Value::Paths)
            .map_err(Failure::from),
        Command::Gc => manager.gc().map(Value::Paths).map_err(Failure::from),
    }
}

fn response(input: *const c_char) -> Response {
    if input.is_null() {
        return Response::Error {
            error: Failure::protocol(
                "invalid_request",
                "rift_ffi_call received a null request".into(),
            ),
        };
    }
    let input = unsafe { CStr::from_ptr(input) };
    match input.to_str() {
        Ok(input) => match execute(input) {
            Ok(value) => Response::Ok { value },
            Err(error) => Response::Error { error },
        },
        Err(error) => Response::Error {
            error: Failure::protocol("invalid_request", error.to_string()),
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rift_ffi_call(input: *const c_char) -> *mut c_char {
    let response =
        std::panic::catch_unwind(|| response(input)).unwrap_or_else(|_| Response::Error {
            error: Failure::protocol("panic", "rift FFI call panicked".into()),
        });
    let output = serde_json::to_string(&response)
        .unwrap_or_else(|error| format!("{{\"status\":\"error\",\"error\":{{\"code\":\"serialization\",\"message\":\"{error}\"}}}}"));
    CString::new(output).unwrap().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn rift_ffi_free(output: *mut c_char) {
    if !output.is_null() {
        unsafe {
            drop(CString::from_raw(output));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_are_exposed_with_codes_and_data() {
        let response = Response::Error {
            error: Error::WorkspaceNotInitialized(PathBuf::from("/tmp/app")).into(),
        };
        let response = serde_json::to_value(response).unwrap();

        assert_eq!(response["status"], "error");
        assert_eq!(response["error"]["code"], "workspace_not_initialized");
        assert_eq!(
            response["error"]["message"],
            "workspace is not initialized: /tmp/app"
        );
        assert_eq!(response["error"]["path"], "/tmp/app");
    }
}
