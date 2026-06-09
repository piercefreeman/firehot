/*
 * Utilities
 */

use anyhow::Result;
use log::{debug, info};

use serde_json::{self, json};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::TempDir;
use uuid::Uuid;

/// Keeps a temporary Python module alive for the duration of a test.
pub struct PythonPathGuard {
    pub module_name: String,
    pub container_path: String,

    _temp_dir: TempDir,
}

impl PythonPathGuard {
    fn new(module_name: String, temp_dir: TempDir) -> Self {
        let container_path = temp_dir
            .path()
            .to_str()
            .expect("Failed to convert temp dir path to string")
            .to_string();
        crate::python::add_python_path(PathBuf::from(&container_path));

        Self {
            module_name,
            container_path,
            _temp_dir: temp_dir,
        }
    }
}

impl Drop for PythonPathGuard {
    fn drop(&mut self) {
        crate::python::remove_python_path(PathBuf::from(&self.container_path).as_path());
    }
}

/// Higher-level function that prepares a Python script for execution in isolation.
/// Used in our testing harness. NOTE: You must call this before any initialization of the first
/// environment, otherwise the forked process won't see the generated module path.
/// Otherwise you'll get an error during exec:
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
/// - The PythonPathGuard object that keeps the temporary module directory alive
///   and registers it for spawned Python child processes until the test finishes
pub fn prepare_script_for_isolation(
    python_script: &str,
    func_name: &str,
) -> Result<(String, PythonPathGuard), String> {
    // Create a temporary directory for the script
    let temp_dir =
        TempDir::new().map_err(|e| format!("Failed to create temporary directory: {e}"))?;

    // Create a valid Python module name (no dashes, start with letter)
    let module_name = format!("pymodule{}", Uuid::new_v4().to_string().replace("-", ""));

    // Create the module directory inside the temp directory
    let module_dir = temp_dir.path().join(&module_name);
    fs::create_dir(&module_dir).map_err(|e| format!("Failed to create module directory: {e}"))?;

    // Create __init__.py inside the module directory to make it a proper package
    let init_path = module_dir.join("__init__.py");
    fs::write(&init_path, "# Package initialization")
        .map_err(|e| format!("Failed to write __init__.py file: {e}"))?;

    // Create the script file inside the module directory (using a standard name)
    let script_file_name = "script.py";
    let script_path = module_dir.join(script_file_name);
    fs::write(&script_path, python_script)
        .map_err(|e| format!("Failed to write script to file: {e}"))?;

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
        .map_err(|e| format!("Failed to write pickle script to temporary file: {e}"))?;

    // Serialize the payload to a JSON string
    let json_payload = isolation_payload.to_string();

    // Create a path we can use after transferring ownership of temp_dir
    let pickle_script_path_string = pickle_script_path.to_string_lossy().to_string();

    // Keep the temporary module directory alive until the caller finishes using it.
    let python_path_guard = PythonPathGuard::new(module_name, temp_dir);

    // Run the pickle script with the payload as an argument
    let child = crate::python::python_command()
        .arg(&pickle_script_path_string)
        .arg(&json_payload)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn Python process: {e}"))?;

    // Get the output
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to get Python process output: {e}"))?;

    // Log stderr for debugging
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        debug!("Python stderr: {}", stderr);
    }

    // Check if the process executed successfully
    if !output.status.success() {
        return Err(format!("Python pickling failed: {stderr}"));
    }

    // Parse the output (base64 encoded pickled data)
    let pickled_output = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Return both the pickled output and the PythonPathGuard which now owns the temp_dir
    info!("Successfully prepared script for isolation");
    Ok((pickled_output, python_path_guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::Environment;
    use base64;
    use base64::Engine;

    #[test]
    fn test_prepare_script_for_isolation() -> Result<(), String> {
        // Create a sample Python script
        let python_script = r#"
def greet(name):
    return f"Hello, {name}!"

def main():
    result = greet("World")
    print(result)
    return result
        "#;

        // Prepare the script for isolation
        let (pickled_data, _python_env) = prepare_script_for_isolation(python_script, "main")?;

        // Verify that we got some pickled data back
        assert!(!pickled_data.is_empty());
        assert!(pickled_data.len() > 20); // A reasonable base64 string length

        // Verify the pickled data is valid base64
        let _decoded = base64::engine::general_purpose::STANDARD
            .decode(pickled_data)
            .map_err(|e| format!("Invalid base64: {e}"))?;

        Ok(())
    }

    #[test]
    fn test_prepare_and_exec_isolation() -> Result<(), String> {
        // Create a sample Python script
        let python_script = r#"
def greet(name):
    return f"Hello, {name}!"

def main():
    result = greet("World")
    return result
        "#;

        // Prepare the script for isolation
        // Keep the temp_dir in scope until the end of the test
        let (pickled_data, python_env) = prepare_script_for_isolation(python_script, "main")?;

        // Create a mock Environment
        let mut runner = Environment::new("test_package", &python_env.container_path, None);

        // Boot the environment
        runner.boot_main()?;

        // Execute the script in isolation
        let process_uuid = runner.exec_isolated(&pickled_data, "test_script")?;

        // Verify the result - should be a valid UUID string
        assert!(!process_uuid.is_empty());

        // Wait for a moment to let the isolated process execute
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Communicate with the isolated process to get the result
        let process_result = runner.communicate_isolated(&process_uuid)?;

        // The result should be "Hello, World!"
        assert_eq!(process_result, Some("Hello, World!".to_string()));

        // Stop the isolated process
        runner.stop_isolated(&process_uuid)?;

        // Stop the main environment
        runner.stop_main()?;

        Ok(())
    }
}
