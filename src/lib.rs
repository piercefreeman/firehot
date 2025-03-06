use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub mod ast;
pub mod environment;
pub mod messages;
pub mod scripts;

// Export types from messages and scripts for public use
pub use messages::{ExitRequest, ForkRequest, Message};
use scripts::{PYTHON_CALL_SCRIPT, PYTHON_LOADER_SCRIPT};

// Replace RUNNERS and other new collections with IMPORT_RUNNERS
static IMPORT_RUNNERS: Lazy<Mutex<HashMap<String, environment::ImportRunner>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Spawn a Python process that imports the given modules and then waits for commands on stdin.
/// The Python process prints "IMPORTS_LOADED" to stdout once all imports are complete.
/// After that, it will listen for commands on stdin, which can include fork requests and code to execute.
fn spawn_python_loader(modules: &HashSet<String>) -> Result<Child> {
    // Create import code for Python to execute
    let mut import_lines = String::new();
    for module in modules {
        import_lines.push_str(&format!("__import__('{}')\n", module));
    }

    println!("Import lines: {}", import_lines);

    // Write the embedded loader script to a file in the /tmp directory
    println!("Writing Python loader script to /tmp directory");
    let temp_path =
        std::path::Path::new("/tmp").join(format!("hotreload_loader_{}.py", std::process::id()));
    std::fs::write(&temp_path, PYTHON_LOADER_SCRIPT.as_bytes())
        .map_err(|e| anyhow!("Failed to write Python loader script to /tmp file: {}", e))?;

    // Launch the Python process with the loader script
    let child = Command::new("python")
        .arg(&temp_path)
        .arg(import_lines)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn python process: {}", e))?;

    Ok(child)
}

/// Python module for hot reloading with isolated imports
#[pymodule]
fn hotreload(_py: Python, m: &PyModule) -> PyResult<()> {
    // Register the module functions
    m.add_function(wrap_pyfunction!(start_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(stop_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(exec_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(stop_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(communicate_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(compute_import_delta, m)?)?;
    m.add_function(wrap_pyfunction!(update_environment, m)?)?;

    Ok(())
}

/// Initialize and start the import runner, returning a unique identifier
#[pyfunction]
fn start_import_runner(_py: Python, package_path: &str) -> PyResult<String> {
    // Generate a unique ID for this runner
    let runner_id = Uuid::new_v4().to_string();

    // Create a new AST manager for this project
    let mut ast_manager = ast::ProjectAstManager::new(package_path);

    // Process Python files to get initial imports
    let third_party_modules = ast_manager
        .process_all_py_files()
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to process Python files: {}", e)))?;

    let _package_name = ast_manager
        .get_package_name()
        .ok_or_else(|| PyRuntimeError::new_err("Could not determine package name"))?;

    // Spawn Python subprocess to load modules
    let mut child = spawn_python_loader(&third_party_modules)
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to spawn Python loader: {}", e)))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdin for python process"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdout for python process"))?;

    let reader = BufReader::new(stdout);
    let mut lines_iter = reader.lines();

    // Wait for the ImportComplete message
    let mut imports_loaded = false;
    for line in &mut lines_iter {
        let line =
            line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;

        // Parse the line as a message
        if let Ok(message) = serde_json::from_str::<Message>(&line) {
            match message {
                Message::ImportComplete(_) => {
                    imports_loaded = true;
                    break;
                }
                Message::ImportError(error) => {
                    return Err(PyRuntimeError::new_err(format!(
                        "Import error: {}: {}",
                        error.error,
                        error.traceback.unwrap_or_default()
                    )));
                }
                _ => {
                    // Print other message types for debugging
                    println!("Received message: {}", line);
                }
            }
        } else {
            // If we can't parse it as a message, log it
            println!("Non-message output: {}", line);
        }
    }

    if !imports_loaded {
        return Err(PyRuntimeError::new_err(
            "Python loader did not report successful imports",
        ));
    }

    // Create the runner object
    let runner = environment::ImportRunner {
        id: runner_id.clone(),
        child: Arc::new(Mutex::new(child)),
        stdin: Arc::new(Mutex::new(stdin)),
        reader: Arc::new(Mutex::new(lines_iter)),
        forked_processes: Arc::new(Mutex::new(HashMap::new())),
        ast_manager,
    };

    // Store in global registry
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    runners.insert(runner_id.clone(), runner);

    Ok(runner_id)
}

/// Compute and return the delta of imports since the last check
#[pyfunction]
fn compute_import_delta(_py: Python, runner_id: &str) -> PyResult<(Vec<String>, Vec<String>)> {
    let mut runners = IMPORT_RUNNERS.lock().unwrap();

    let runner = runners.get_mut(runner_id).ok_or_else(|| {
        PyRuntimeError::new_err(format!("No import runner found for ID: {}", runner_id))
    })?;

    let (added, removed) = runner
        .ast_manager
        .compute_import_delta()
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to compute import delta: {}", e)))?;

    // Convert HashSet to Vec for PyO3
    let added_vec: Vec<String> = added.into_iter().collect();
    let removed_vec: Vec<String> = removed.into_iter().collect();

    Ok((added_vec, removed_vec))
}

/// Update the environment by checking for import changes and restarting if necessary
#[pyfunction]
fn update_environment(_py: Python, runner_id: &str) -> PyResult<bool> {
    // Get the ImportRunner
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    let runner = runners.get_mut(runner_id).ok_or_else(|| {
        PyRuntimeError::new_err(format!("No import runner found with ID: {}", runner_id))
    })?;

    // Update the environment using the runner's method
    let updated = runner
        .update_environment()
        .map_err(|e| PyRuntimeError::new_err(format!("Failed to update environment: {}", e)))?;

    Ok(updated)
}

/// Stop the import runner with the given ID
#[pyfunction]
fn stop_import_runner(_py: Python, runner_id: &str) -> PyResult<()> {
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.remove(runner_id) {
        // Clean up resources
        runner
            .stop_main()
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to stop import runner: {}", e)))?;

        Ok(())
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Execute a Python function in an isolated process
#[pyfunction]
fn exec_isolated<'py>(
    py: Python<'py>,
    runner_id: &str,
    func: PyObject,
    args: Option<PyObject>,
) -> PyResult<&'py PyAny> {
    // Create a dict to hold our function and args for pickling
    let locals = PyDict::new(py);
    locals.set_item("func", func)?;
    locals.set_item("args", args.unwrap_or_else(|| py.None()))?;

    py.run(PYTHON_CALL_SCRIPT, None, Some(locals))?;

    // Get the pickled data - now it's a string because we decoded it in Python
    let pickled_data = locals
        .get_item("pickled_data")
        .ok_or_else(|| PyRuntimeError::new_err("Failed to pickle function and args"))?
        .extract::<String>()?;

    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        // Convert Rust Result<String, String> to PyResult
        match runner.exec_isolated(&pickled_data) {
            Ok(result) => Ok(py.eval(&format!("'{}'", result), None, None)?),
            Err(err) => Err(PyRuntimeError::new_err(err)),
        }
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Stop an isolated process
#[pyfunction]
fn stop_isolated(_py: Python, runner_id: &str, process_uuid: &str) -> PyResult<bool> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner
            .stop_isolated(process_uuid)
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to stop isolated process: {}", e)))
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}

/// Get output from an isolated process
#[pyfunction]
fn communicate_isolated(
    _py: Python,
    runner_id: &str,
    process_uuid: &str,
) -> PyResult<Option<String>> {
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.communicate_isolated(process_uuid).map_err(|e| {
            PyRuntimeError::new_err(format!(
                "Failed to communicate with isolated process: {}",
                e
            ))
        })
    } else {
        Err(PyRuntimeError::new_err(format!(
            "No import runner found with ID: {}",
            runner_id
        )))
    }
}
