/*
 * Utilities
 */
 
 use anyhow::Result;
 use log::{debug, info};
 
 use serde_json::{self, json};
 use std::process::{Command, Stdio};
 use uuid::Uuid;
 use std::fs;
 use tempfile::TempDir;
 
 use std::env;

/// Python env guard that restores the original PYTHONPATH when dropped
pub struct PythonPathGuard {
    pub module_name: String,
    pub container_path: String,

    _temp_dir: TempDir,
}

impl PythonPathGuard {
    fn new(module_name: String, temp_dir: TempDir) -> Self {
        // Get the path from temp_dir
        let container_path = temp_dir.path().to_str()
            .expect("Failed to convert temp dir path to string")
            .to_string();
        
        // Add the new path to PYTHONPATH
        if let Some(current_path) = env::var_os("PYTHONPATH") {
            // If PYTHONPATH already exists, append the temp dir
            let mut new_path = current_path.clone();
            let path_separator = if cfg!(windows) { ";" } else { ":" };
            new_path.push(path_separator);
            new_path.push(&container_path);
            env::set_var("PYTHONPATH", new_path);
        } else {
            // If PYTHONPATH doesn't exist, create it with just the temp dir
            env::set_var("PYTHONPATH", &container_path);
        }
        
        Self { module_name, container_path, _temp_dir: temp_dir }
    }
}

impl Drop for PythonPathGuard {
    fn drop(&mut self) {
        // Get the current PYTHONPATH
        if let Some(current_path) = env::var_os("PYTHONPATH") {
            let current_path_str = current_path.to_str().unwrap_or("");
            let path_separator = if cfg!(windows) { ";" } else { ":" };
            
            // Split the current path into components
            let mut components: Vec<&str> = current_path_str.split(path_separator).collect();
            
            // Remove our specific directory from the components
            components.retain(|&component| component != self.container_path);
            
            // Rejoin the components to form the new path
            let new_path = components.join(path_separator);
            
            // If there are still components left, set the new path
            // Otherwise, remove the PYTHONPATH variable
            if !new_path.is_empty() {
                env::set_var("PYTHONPATH", new_path);
            } else {
                env::remove_var("PYTHONPATH");
            }
        }
        // If PYTHONPATH doesn't exist, nothing to do
    }
}

/// Higher-level function that prepares a Python script for execution in isolation.
/// Used in our testing harness. NOTE: You must call this before any initialization of the first
/// environment, otherwise the forked process won't pick up on our updated PYTHONPATH
/// to import the mocked module. Otherwise you'll get an error during exec:
/// `No module named 'pymodule550871ccb8f44d3eae652d09468cef98'`
///
/// This function:
/// 1. Takes a Python script as input
/// 2. Creates a temporary environment
/// 3. Builds a JSON payload with all necessary information
/// 4. Handles pickling and encoding for execution isolation
///
/// Returns a tuple containing:
/// - The pickled, base64-encoded data ready for execution in isolation
/// - The PythonPathGuard object that restores the original PYTHONPATH when dropped
///   and cleans up the temporary directory when it goes out of scope
pub fn prepare_script_for_isolation(
    python_script: &str,
    func_name: &str,
) -> Result<(String, PythonPathGuard), String> {
    // Create a temporary directory for the script
    let temp_dir =
        TempDir::new().map_err(|e| format!("Failed to create temporary directory: {}", e))?;

    // Create a valid Python module name (no dashes, start with letter)
    let module_name = format!("pymodule{}", Uuid::new_v4().to_string().replace("-", ""));
    
    // Create the module directory inside the temp directory
    let module_dir = temp_dir.path().join(&module_name);
    fs::create_dir(&module_dir)
        .map_err(|e| format!("Failed to create module directory: {}", e))?;
    
    // Create __init__.py inside the module directory to make it a proper package
    let init_path = module_dir.join("__init__.py");
    fs::write(&init_path, "# Package initialization")
        .map_err(|e| format!("Failed to write __init__.py file: {}", e))?;

    // Create the script file inside the module directory (using a standard name)
    let script_file_name = "script.py";
    let script_path = module_dir.join(script_file_name);
    fs::write(&script_path, python_script)
        .map_err(|e| format!("Failed to write script to file: {}", e))?;

    // At this point our directory looks like:
    // pymodule
    // - __init__.py
    // - script.py

    // Build the payload according to the SerializedCall TypedDict format
    // The module import path is module_name.script (without the .py extension)
    let isolation_payload = json!({
        "func_module_path": format!("{}.{}", module_name, script_file_name.trim_end_matches(".py")),
        "func_name": func_name,
        "func_qualname": func_name,
        "args": serde_json::Value::Null,
    });

    // Create a simple pickle script that only handles pickling and base64 encoding
    let pickle_script = r#"
import sys
import json
import base64
import pickle

# Get the payload from command line arguments
payload_json = sys.argv[1]
payload = json.loads(payload_json)

# Pickle and base64 encode
pickled_data = base64.b64encode(pickle.dumps(payload)).decode('utf-8')

# Print the result to stdout (this is what the function returns)
print(pickled_data)
    "#;

    // Write the pickle script directly to the temp directory (not in the module)
    let pickle_script_path = temp_dir.path().join("pickle_helper.py");
    fs::write(&pickle_script_path, pickle_script)
        .map_err(|e| format!("Failed to write pickle script to temporary file: {}", e))?;

    // Serialize the payload to a JSON string
    let json_payload = isolation_payload.to_string();

    // Create a path we can use after transferring ownership of temp_dir
    let pickle_script_path_string = pickle_script_path.to_string_lossy().to_string();

    // Create the PythonPathGuard which takes ownership of temp_dir, updates PYTHONPATH,
    // and will handle cleanup when dropped
    let python_path_guard = PythonPathGuard::new(module_name, temp_dir);

    // Run the pickle script with the payload as an argument
    let child = Command::new("python")
        .arg(&pickle_script_path_string)
        .arg(&json_payload)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn Python process: {}", e))?;

    // Get the output
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to get Python process output: {}", e))?;

    // Log stderr for debugging
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        debug!("Python stderr: {}", stderr);
    }

    // Check if the process executed successfully
    if !output.status.success() {
        return Err(format!("Python pickling failed: {}", stderr));
    }

    // Parse the output (base64 encoded pickled data)
    let pickled_output = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Return both the pickled output and the PythonPathGuard which now owns the temp_dir
    info!("Successfully prepared script for isolation");
    Ok((pickled_output, python_path_guard))
}