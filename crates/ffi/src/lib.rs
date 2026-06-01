use rift::{Create, Manager};
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
    Error { error: String },
}

fn execute(input: &str) -> Result<Value, String> {
    let request: Request = serde_json::from_str(input).map_err(|error| error.to_string())?;
    let mut manager = match request.database {
        Some(path) => Manager::open(path),
        None => Manager::open_default(),
    }
    .map_err(|error| error.to_string())?;
    match request.command {
        Command::Init { at } => manager
            .init(at)
            .map(Value::Path)
            .map_err(|error| error.to_string()),
        Command::Create { from, name, into } => manager
            .create(Create { from, name, into })
            .map(|path| Value::Path(Some(path)))
            .map_err(|error| error.to_string()),
        Command::Remove { at, all } => {
            if all.unwrap_or(false) {
                manager
                    .remove_all(at)
                    .map(Value::Paths)
                    .map_err(|error| error.to_string())
            } else {
                manager
                    .remove(at)
                    .map(|()| Value::Empty(()))
                    .map_err(|error| error.to_string())
            }
        }
        Command::List { of } => manager
            .list(of)
            .map(Value::Paths)
            .map_err(|error| error.to_string()),
        Command::Ancestors { of } => manager
            .ancestors(of)
            .map(Value::Paths)
            .map_err(|error| error.to_string()),
        Command::Gc => manager
            .gc()
            .map(Value::Paths)
            .map_err(|error| error.to_string()),
    }
}

fn response(input: *const c_char) -> Response {
    if input.is_null() {
        return Response::Error {
            error: "rift_ffi_call received a null request".into(),
        };
    }
    let input = unsafe { CStr::from_ptr(input) };
    match input.to_str() {
        Ok(input) => match execute(input) {
            Ok(value) => Response::Ok { value },
            Err(error) => Response::Error { error },
        },
        Err(error) => Response::Error {
            error: error.to_string(),
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rift_ffi_call(input: *const c_char) -> *mut c_char {
    let response =
        std::panic::catch_unwind(|| response(input)).unwrap_or_else(|_| Response::Error {
            error: "rift FFI call panicked".into(),
        });
    let output = serde_json::to_string(&response)
        .unwrap_or_else(|error| format!("{{\"status\":\"error\",\"error\":\"{error}\"}}"));
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
