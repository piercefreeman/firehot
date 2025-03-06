use anstream::eprintln;
use log::{debug, error, info};
use once_cell::sync::Lazy;
use owo_colors::OwoColorize;
use std::{collections::HashMap, time::Instant};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::Mutex;
use uuid::Uuid;

pub mod ast;
pub mod environment;
pub mod messages;
pub mod scripts;

// Export types from messages and scripts for public use
pub use messages::{ExitRequest, ForkRequest, Message};
use scripts::PYTHON_CALL_SCRIPT;

// Replace RUNNERS and other new collections with IMPORT_RUNNERS
static IMPORT_RUNNERS: Lazy<Mutex<HashMap<String, environment::ImportRunner>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Python module for hot reloading with isolated imports
#[pymodule]
fn firehot(_py: Python, m: &PyModule) -> PyResult<()> {
    // Initialize the logger using Builder API
    let mut builder = env_logger::Builder::from_default_env();

    // Check if FIREHOT_LOG_LEVEL is set
    match std::env::var("FIREHOT_LOG_LEVEL") {
        Ok(level) => {
            // Parse the level from the environment variable
            let log_level = match level.to_lowercase().as_str() {
                "trace" => log::LevelFilter::Trace,
                "debug" => log::LevelFilter::Debug,
                "info" => log::LevelFilter::Info,
                "warn" => log::LevelFilter::Warn,
                "error" => log::LevelFilter::Error,
                _ => {
                    // Default to info if the level is invalid
                    // Can't use warn! here as logger isn't initialized yet
                    eprintln!("Invalid log level: {}. Using info level instead.", level);
                    log::LevelFilter::Info
                }
            };
            // Set filter for just the firehot crate
            builder.filter(Some("firehot"), log_level);
        }
        Err(_) => {
            // Default to warn level if FIREHOT_LOG_LEVEL is not set
            builder.filter(Some("firehot"), log::LevelFilter::Warn);
        }
    }

    // Initialize the logger
    let _ = builder.try_init();

    info!("Initializing firehot module");

    // Environment (parent) management
    m.add_function(wrap_pyfunction!(start_import_runner, m)?)?;
    m.add_function(wrap_pyfunction!(update_environment, m)?)?;
    m.add_function(wrap_pyfunction!(stop_import_runner, m)?)?;

    // Isolated (child, post-fork) process management
    m.add_function(wrap_pyfunction!(exec_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(communicate_isolated, m)?)?;
    m.add_function(wrap_pyfunction!(stop_isolated, m)?)?;

    info!("firehot module initialization complete");
    Ok(())
}

/// Initialize and start the import runner, returning a unique identifier
#[pyfunction]
fn start_import_runner(_py: Python, project_name: &str, package_path: &str) -> PyResult<String> {
    // Generate a unique ID for this runner
    let runner_id = Uuid::new_v4().to_string();

    // Beautiful logging for starting the import runner
    eprintln!(
        "{} {} {}",
        "🔥".magenta().bold(),
        "Initializing firehot for".white().bold(),
        project_name.cyan().bold()
    );

    // Create the runner object
    info!("Creating environment with ID: {}", runner_id);
    let mut runner = environment::ImportRunner::new(project_name, package_path);

    runner.boot_main().map_err(|e| {
        error!("Failed to boot main: {}", e);
        PyRuntimeError::new_err(e)
    })?;

    // Store in global registry
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    runners.insert(runner_id.clone(), runner);

    Ok(runner_id)
}

/// Update the environment by checking for import changes and restarting if necessary
#[pyfunction]
fn update_environment(_py: Python, runner_id: &str) -> PyResult<bool> {
    // Get the ImportRunner
    info!("Updating environment for runner: {}", runner_id);
    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    let runner = runners.get_mut(runner_id).ok_or_else(|| {
        let err_msg = format!("No import runner found with ID: {}", runner_id);
        error!("{}", err_msg);
        PyRuntimeError::new_err(err_msg)
    })?;

    // Update the environment using the runner's method
    let updated = runner.update_environment().map_err(|e| {
        let err_msg = format!("Failed to update environment: {}", e);
        error!("{}", err_msg);
        PyRuntimeError::new_err(err_msg)
    })?;

    if updated {
        info!("Environment updated successfully for runner: {}", runner_id);
    } else {
        debug!("No environment updates needed for runner: {}", runner_id);
    }

    Ok(updated)
}

/// Stop the import runner with the given ID
#[pyfunction]
fn stop_import_runner(_py: Python, runner_id: &str) -> PyResult<()> {
    // Beautiful logging for stopping the import runner
    eprintln!(
        "\n{} {}\n",
        "⏹".yellow().bold(),
        format!("Stopping import environment {}", runner_id)
            .white()
            .bold()
    );

    let start_time = Instant::now();

    let mut runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.remove(runner_id) {
        // Clean up resources
        runner.stop_main().map_err(|e| {
            let err_msg = format!("Failed to stop import runner: {}", e);
            error!("{}", err_msg);
            PyRuntimeError::new_err(err_msg)
        })?;

        // Calculate and log cleanup time
        let elapsed_ms = start_time.elapsed().as_millis();
        eprintln!(
            "{} {} {} {}",
            "✓".green().bold(),
            "Import runner stopped in".white().bold(),
            elapsed_ms.to_string().yellow().bold(),
            "ms".white().bold()
        );

        Ok(())
    } else {
        let err_msg = format!("No import runner found with ID: {}", runner_id);
        error!("{}", err_msg);

        // Log the error with owo_colors
        eprintln!("\n{} {}\n", "✗".red().bold(), err_msg.white().bold());

        Err(PyRuntimeError::new_err(err_msg))
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
    debug!(
        "Executing function in isolated process for runner: {}",
        runner_id
    );

    // Create a dict to hold our function and args for pickling
    let locals = PyDict::new(py);
    locals.set_item("func", func)?;
    locals.set_item("args", args.unwrap_or_else(|| py.None()))?;

    py.run(PYTHON_CALL_SCRIPT, None, Some(locals))?;

    // Get the pickled data - now it's a string because we decoded it in Python
    let pickled_data = locals
        .get_item("pickled_data")
        .ok_or_else(|| {
            let err_msg = "Failed to pickle function and args";
            error!("{}", err_msg);
            PyRuntimeError::new_err(err_msg)
        })?
        .extract::<String>()?;

    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        // Convert Rust Result<String, String> to PyResult
        match runner.exec_isolated(&pickled_data) {
            Ok(result) => {
                debug!("Function executed successfully in isolated process");
                Ok(py.eval(&format!("'{}'", result), None, None)?)
            }
            Err(err) => {
                error!("Error executing function in isolated process: {}", err);
                Err(PyRuntimeError::new_err(err))
            }
        }
    } else {
        let err_msg = format!("No import runner found with ID: {}", runner_id);
        error!("{}", err_msg);
        Err(PyRuntimeError::new_err(err_msg))
    }
}

/// Stop an isolated process
#[pyfunction]
fn stop_isolated(_py: Python, runner_id: &str, process_uuid: &str) -> PyResult<bool> {
    info!(
        "Stopping isolated process {} for runner {}",
        process_uuid, runner_id
    );
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.stop_isolated(process_uuid).map_err(|e| {
            let err_msg = format!("Failed to stop isolated process: {}", e);
            error!("{}", err_msg);
            PyRuntimeError::new_err(err_msg)
        })
    } else {
        let err_msg = format!("No import runner found with ID: {}", runner_id);
        error!("{}", err_msg);
        Err(PyRuntimeError::new_err(err_msg))
    }
}

/// Get output from an isolated process
#[pyfunction]
fn communicate_isolated(
    _py: Python,
    runner_id: &str,
    process_uuid: &str,
) -> PyResult<Option<String>> {
    debug!(
        "Communicating with isolated process {} for runner {}",
        process_uuid, runner_id
    );
    let runners = IMPORT_RUNNERS.lock().unwrap();
    if let Some(runner) = runners.get(runner_id) {
        runner.communicate_isolated(process_uuid).map_err(|e| {
            let err_msg = format!("Failed to communicate with isolated process: {}", e);
            error!("{}", err_msg);
            PyRuntimeError::new_err(err_msg)
        })
    } else {
        let err_msg = format!("No import runner found with ID: {}", runner_id);
        error!("{}", err_msg);
        Err(PyRuntimeError::new_err(err_msg))
    }
}
