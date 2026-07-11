use rift::{CopyMode, Create, CreateOptions, Error, HookMode, Manager};
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
        #[serde(rename = "copyAll")]
        copy_all: Option<bool>,
        hooks: Option<bool>,
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
    Status {
        of: PathBuf,
    },
    Gc,
}

#[derive(Serialize)]
struct Workspace {
    path: PathBuf,
    id: String,
    parent: Option<PathBuf>,
}

impl From<rift::Workspace> for Workspace {
    fn from(workspace: rift::Workspace) -> Self {
        Self {
            path: workspace.path,
            id: workspace.id,
            parent: workspace.parent,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Value {
    Empty(()),
    Path(Option<PathBuf>),
    Paths(Vec<PathBuf>),
    Workspace(Workspace),
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
            Error::UnsafeMarker(path) => ("unsafe_marker", Some(path.clone())),
            Error::UnsupportedEntry(path) => ("unsupported_entry", Some(path.clone())),
            Error::UnsafeGit(_) => ("unsafe_git", None),
            Error::NotManaged(path) => ("not_managed", Some(path.clone())),
            Error::MarkerMismatch(path) => ("marker_mismatch", Some(path.clone())),
            Error::UnknownMarker(path) => ("unknown_marker", Some(path.clone())),
            Error::AlreadyExists(path) => ("already_exists", Some(path.clone())),
            Error::MissingRift(path) => ("missing_rift", Some(path.clone())),
            Error::DanglingParent { workspace, .. } => ("dangling_parent", Some(workspace.clone())),
            Error::InsideSource(path) => ("inside_source", Some(path.clone())),
            Error::InvalidConfig { path, .. } => ("invalid_config", Some(path.clone())),
            Error::HookFailed { path, .. } => ("hook_failed", Some(path.clone())),
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
    let mut manager = request
        .database
        .map_or_else(Manager::open_default, Manager::open)
        .map_err(Failure::from)?;
    match request.command {
        Command::Init { at } => manager
            .init(at)
            .map(|_| Value::Empty(()))
            .map_err(Failure::from),
        Command::Create {
            from,
            name,
            into,
            copy_all,
            hooks,
        } => manager
            .create_with_options(
                Create::new(from).with_name(name).with_storage(into),
                CreateOptions::default()
                    .copy_mode(if copy_all.unwrap_or(false) {
                        CopyMode::All
                    } else {
                        CopyMode::Filtered
                    })
                    .hook_mode(if hooks.unwrap_or(true) {
                        HookMode::Run
                    } else {
                        HookMode::Skip
                    }),
            )
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
        Command::Status { of } => manager
            .describe(of)
            .map(Workspace::from)
            .map(Value::Workspace)
            .map_err(Failure::from),
        Command::Gc => manager.gc().map(Value::Paths).map_err(Failure::from),
    }
}

unsafe fn response(input: *const c_char) -> Response {
    if input.is_null() {
        return Response::Error {
            error: Failure::protocol(
                "invalid_request",
                "rift_ffi_call received a null request".into(),
            ),
        };
    }
    // SAFETY: null was checked above. The caller promises any non-null input
    // points to a valid null-terminated request buffer for this call.
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

/// # Safety
///
/// If `input` is non-null, it must point to a valid null-terminated byte
/// buffer for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_ffi_call(input: *const c_char) -> *mut c_char {
    let response = std::panic::catch_unwind(|| {
        // SAFETY: forwarded from `rift_ffi_call`'s caller contract.
        unsafe { response(input) }
    })
    .unwrap_or_else(|_| Response::Error {
        error: Failure::protocol("panic", "rift FFI call panicked".into()),
    });
    let output = serde_json::to_string(&response).unwrap_or_else(|_| {
        r#"{"status":"error","error":{"code":"serialization","message":"failed to serialize response"}}"#
            .to_owned()
    });
    response_string_into_raw(output)
}

fn response_string_into_raw(output: String) -> *mut c_char {
    match CString::new(output) {
        Ok(output) => output.into_raw(),
        Err(_) => c"{\"status\":\"error\",\"error\":{\"code\":\"serialization\",\"message\":\"response contained an interior null byte\"}}"
            .to_owned()
            .into_raw(),
    }
}

/// # Safety
///
/// `output` must be a pointer previously returned by `rift_ffi_call` that has
/// not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_ffi_free(output: *mut c_char) {
    if !output.is_null() {
        // SAFETY: the caller promises `output` came from `CString::into_raw`
        // in `rift_ffi_call`, and this function takes back ownership once.
        unsafe {
            drop(CString::from_raw(output));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffi_json(request: serde_json::Value) -> serde_json::Value {
        let input = CString::new(request.to_string()).unwrap();
        // SAFETY: `input` is a valid, null-terminated request buffer for this call.
        let output = unsafe { rift_ffi_call(input.as_ptr()) };
        assert!(!output.is_null());
        // SAFETY: `rift_ffi_call` returned a valid C string pointer, which remains
        // allocated until `rift_ffi_free` is called below.
        let response = unsafe { CStr::from_ptr(output).to_str().unwrap().to_owned() };
        // SAFETY: `output` came from `rift_ffi_call` and is freed exactly once.
        unsafe {
            rift_ffi_free(output);
        }
        serde_json::from_str(&response).unwrap()
    }

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

        let unsafe_marker = serde_json::to_value(Response::Error {
            error: Error::UnsafeMarker(PathBuf::from("/tmp/app/.rift")).into(),
        })
        .unwrap();
        assert_eq!(unsafe_marker["error"]["code"], "unsafe_marker");
        assert_eq!(unsafe_marker["error"]["path"], "/tmp/app/.rift");

        let dangling_parent = serde_json::to_value(Response::Error {
            error: Error::DanglingParent {
                workspace: PathBuf::from("/tmp/app/.rifts/child"),
                parent_id: "PARENTID".into(),
            }
            .into(),
        })
        .unwrap();
        assert_eq!(dangling_parent["error"]["code"], "dangling_parent");
        assert_eq!(dangling_parent["error"]["path"], "/tmp/app/.rifts/child");
    }

    #[test]
    fn new_create_options_are_accepted_by_the_protocol() {
        let request = serde_json::from_str::<Request>(
            r#"{
                "command": "create",
                "from": "/tmp/app",
                "name": "child",
                "into": null,
                "copyAll": true,
                "hooks": false
            }"#,
        )
        .unwrap();

        assert!(matches!(
            request.command,
            Command::Create {
                copy_all: Some(true),
                hooks: Some(false),
                ..
            }
        ));
    }

    #[test]
    fn status_describes_an_initialized_workspace_through_the_protocol_when_supported() {
        let fixture = std::env::temp_dir().join(format!(
            "rift-ffi-status-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = fixture.join("app");
        let database = fixture.join("rift.db");
        std::fs::create_dir_all(&workspace).unwrap();

        let init = serde_json::json!({
            "database": database,
            "command": "init",
            "at": workspace,
        });
        let init = ffi_json(init);
        if init["status"] == "error" {
            assert_eq!(init["error"]["code"], "cow_unavailable");
            std::fs::remove_dir_all(fixture).unwrap();
            return;
        }
        assert_eq!(init["status"], "ok");
        assert_eq!(init["value"], serde_json::Value::Null);

        let status = serde_json::json!({
            "database": database,
            "command": "status",
            "of": workspace,
        });
        let response = ffi_json(status);
        assert_eq!(response["status"], "ok");
        assert_eq!(
            response["value"]["path"],
            std::fs::canonicalize(&workspace)
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert!(
            response["value"]["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(response["value"]["parent"], serde_json::Value::Null);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn hook_and_config_errors_are_exposed_with_codes_and_paths() {
        let config = serde_json::to_value(Response::Error {
            error: Error::InvalidConfig {
                path: PathBuf::from("/tmp/app/.rift.toml"),
                message: "bad".into(),
            }
            .into(),
        })
        .unwrap();
        let hook = serde_json::to_value(Response::Error {
            error: Error::HookFailed {
                path: PathBuf::from("/tmp/app"),
                command: "exit 1".into(),
                message: "exited with 1".into(),
            }
            .into(),
        })
        .unwrap();

        assert_eq!(config["error"]["code"], "invalid_config");
        assert_eq!(config["error"]["path"], "/tmp/app/.rift.toml");
        assert_eq!(hook["error"]["code"], "hook_failed");
        assert_eq!(hook["error"]["path"], "/tmp/app");
    }

    #[test]
    fn ffi_response_allocation_handles_interior_nulls() {
        let output = response_string_into_raw("bad\0json".into());
        // SAFETY: `response_string_into_raw` returns a valid C string pointer
        // that remains allocated until `rift_ffi_free` takes it back below.
        let response = unsafe { CStr::from_ptr(output).to_string_lossy().into_owned() };

        assert!(response.contains("interior null byte"));

        // SAFETY: `output` came from `response_string_into_raw` and has not
        // been freed yet.
        unsafe {
            rift_ffi_free(output);
        }
    }
}
