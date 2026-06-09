use once_cell::sync::Lazy;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

static PYTHON_EXECUTABLE: Lazy<OsString> = Lazy::new(resolve_python_executable);
static EXTRA_PYTHONPATHS: Lazy<Mutex<Vec<PathBuf>>> = Lazy::new(|| Mutex::new(Vec::new()));

pub fn python_command() -> Command {
    Command::new(&*PYTHON_EXECUTABLE)
}

pub fn add_python_path(path: PathBuf) {
    let mut paths = EXTRA_PYTHONPATHS.lock().unwrap();
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

pub fn remove_python_path(path: &Path) {
    let mut paths = EXTRA_PYTHONPATHS.lock().unwrap();
    paths.retain(|existing| existing != path);
}

pub fn extra_python_paths() -> Vec<PathBuf> {
    EXTRA_PYTHONPATHS.lock().unwrap().clone()
}

fn resolve_python_executable() -> OsString {
    if let Some(python) = std::env::var_os("PYTHON").filter(|value| !value.is_empty()) {
        return python;
    }

    for candidate in ["python", "python3"] {
        if let Ok(status) = Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            && status.success()
        {
            return candidate.into();
        }
    }

    "python".into()
}
