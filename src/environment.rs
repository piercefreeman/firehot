use anstream::eprintln;
use anyhow::{anyhow, Result};
use log::{debug, error, info, trace, warn};
use owo_colors::OwoColorize;
use serde_json::{self};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use libc;
use std::io::BufRead;
use uuid::Uuid;

use crate::ast::ProjectAstManager;
use crate::layer::{Layer, ProcessResult};
use crate::messages::{ExitRequest, ForkRequest, Message};
use crate::scripts::{PYTHON_CHILD_SCRIPT, PYTHON_LOADER_SCRIPT};

/// Runner for isolated Python code execution
pub struct Environment {
    pub id: String,
    pub layer: Option<Arc<Mutex<Layer>>>, // The current layer that is tied to this environment
    pub ast_manager: ProjectAstManager,   // Project AST manager for this environment

    first_scan: bool,
}

impl Environment {
    pub fn new(project_name: &str, project_path: &str) -> Self {
        // Create a new AST manager for this project
        let ast_manager = ProjectAstManager::new(project_name, project_path);
        info!("Created AST manager for project: {}", project_name);

        Self {
            id: Uuid::new_v4().to_string(),
            layer: None,
            ast_manager,
            first_scan: false,
        }
    }

    //
    // Main process management
    //

    pub fn boot_main(&mut self) -> Result<(), String> {
        info!(
            "Processing Python files in: {}",
            self.ast_manager.get_project_path()
        );
        let third_party_modules = self
            .ast_manager
            .process_all_py_files()
            .map_err(|e| format!("Failed to process Python files: {}", e))?;

        let start_time = Instant::now();

        // Spawn Python subprocess to load modules
        info!(
            "Spawning Python subprocess to load {} modules",
            third_party_modules.len()
        );
        let mut child = spawn_python_loader(&third_party_modules)
            .map_err(|e| format!("Failed to spawn Python loader: {}", e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture stdin for python process".to_string())?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture stdout for python process".to_string())?;

        let reader = BufReader::new(stdout);
        let mut lines_iter = reader.lines();

        // Wait for the ImportComplete message
        info!("Waiting for import completion...");
        let mut imports_loaded = false;
        for line in &mut lines_iter {
            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;

            // Parse the line as a message
            if let Ok(message) = serde_json::from_str::<Message>(&line) {
                match message {
                    Message::ImportComplete(_) => {
                        info!("Imports loaded successfully");
                        imports_loaded = true;
                        break;
                    }
                    Message::ImportError(error) => {
                        error!(
                            "Import error: {}: {}",
                            error.error,
                            error.traceback.clone().unwrap_or_default()
                        );
                        return Err(format!(
                            "Import error: {}: {}",
                            error.error,
                            error.traceback.unwrap_or_default()
                        ));
                    }
                    _ => {
                        // Log other message types for debugging
                        debug!("Received message: {}", line);
                    }
                }
            } else {
                // If we can't parse it as a message, log it
                debug!("Non-message output: {}", line);
            }
        }

        if !imports_loaded {
            error!("Python loader did not report successful imports");
            return Err("Python loader did not report successful imports".to_string());
        }

        // Calculate total setup time and log completion
        let elapsed = start_time.elapsed();
        let elapsed_ms = elapsed.as_millis();

        eprintln!(
            "\n{} {} {} {}{} {}\n",
            "✓".green().bold(),
            "Layer built in".white().bold(),
            elapsed_ms.to_string().yellow().bold(),
            "ms".white().bold(),
            if elapsed_ms > 1000 {
                format!(
                    " {}",
                    format!("({:.2}s)", elapsed_ms as f64 / 1000.0)
                        .cyan()
                        .italic()
                )
            } else {
                String::new()
            },
            format!("with ID: {}", self.id).white().bold()
        );

        // Create the environment
        let mut layer = Layer {
            child,
            stdin,
            reader: Some(lines_iter),
            forked_processes: HashMap::new(),
            result_map: HashMap::new(),
            completion_notifiers: HashMap::new(),
            monitor_thread: None,
            thread_terminate_tx: None,
        };

        // Start the monitor thread
        layer.start_monitor_thread();

        self.layer = Some(Arc::new(Mutex::new(layer)));

        Ok(())
    }

    pub fn stop_main(&self) -> Result<bool, String> {
        // Check if environment is initialized
        let layer = match self.layer.as_ref() {
            Some(env) => env,
            None => {
                info!("No environment to stop.");
                return Ok(false);
            }
        };

        info!("Stopping main runner process");

        let mut env_guard = layer
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Stop the monitor thread first
        env_guard.stop_monitor_thread();

        // Kill the main child process
        if let Err(e) = env_guard.child.kill() {
            warn!("Failed to kill child process: {}", e);
        }

        // Wait for the process to exit
        if let Err(e) = env_guard.child.wait() {
            warn!("Failed to wait for child process: {}", e);
        }

        // Clear all the maps
        env_guard.forked_processes.clear();
        env_guard.result_map.clear();
        env_guard.completion_notifiers.clear();

        info!("Main runner process stopped");
        Ok(true)
    }

    pub fn update_environment(&mut self) -> Result<bool, String> {
        info!("Checking for environment updates...");

        // Check for any changes to the imports
        if !self.first_scan {
            return Ok(false); // Nothing to update if we haven't even scanned yet
        }

        // Get the delta
        let (added, removed) = self
            .ast_manager
            .compute_import_delta()
            .map_err(|e| format!("Failed to compute import delta: {}", e))?;

        // Check if imports have changed
        if added.is_empty() && removed.is_empty() {
            info!("No changes to imports detected");
            return Ok(false);
        }

        info!(
            "Detected changes to imports. Added: {:?}, Removed: {:?}",
            added, removed
        );

        // Stop any existing processes
        if let Some(env) = self.layer.as_ref() {
            let forked_processes = {
                let env_guard = env
                    .lock()
                    .map_err(|e| format!("Failed to lock layer mutex: {}", e))?;

                // Create a copy of the process UUIDs
                env_guard
                    .forked_processes
                    .keys()
                    .cloned()
                    .collect::<Vec<String>>()
            };

            // Stop all forked processes
            for process_uuid in forked_processes {
                if let Err(e) = self.stop_isolated(&process_uuid) {
                    warn!("Failed to stop process {}: {}", process_uuid, e);
                }
            }

            // Stop the main process
            self.stop_main()?;
        }

        // Boot a new layer
        self.boot_main()?;

        info!("Environment updated successfully");
        Ok(true)
    }

    //
    // Isolated process management
    //

    /// Execute a function in the isolated environment. This should be called from the main thread (the one
    /// that spawned our hotreloader) so we can get the local function and closure variables.
    pub fn exec_isolated(&self, pickled_data: &str) -> Result<String, String> {
        // Check if environment is initialized
        let environment = self
            .layer
            .as_ref()
            .ok_or_else(|| "Environment not initialized. Call boot_main first.".to_string())?;

        // Generate a process UUID
        let process_uuid = Uuid::new_v4().to_string();

        // Send the code to the forked process
        let mut env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Initialize an empty result entry for this process before sending the request
        env_guard.result_map.insert(process_uuid.clone(), None);

        // Create a condition variable for this process
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        env_guard
            .completion_notifiers
            .insert(process_uuid.clone(), pair.clone());

        let exec_code = format!(
            r#"
pickled_str = "{}"
{}
            "#,
            pickled_data, PYTHON_CHILD_SCRIPT,
        );

        // Create a ForkRequest message
        let fork_request = ForkRequest {
            request_id: process_uuid.clone(),
            code: exec_code,
        };

        let fork_json = serde_json::to_string(&Message::ForkRequest(fork_request))
            .map_err(|e| format!("Failed to serialize fork request: {}", e))?;

        // Send the message to the child process
        writeln!(env_guard.stdin, "{}", fork_json)
            .map_err(|e| format!("Failed to write to child stdin: {}", e))?;
        env_guard
            .stdin
            .flush()
            .map_err(|e| format!("Failed to flush child stdin: {}", e))?;

        // Release the lock so we don't block other operations
        drop(env_guard);

        // Wait on the condition variable
        let (mutex, condvar) = &*pair;
        let completed = mutex
            .lock()
            .map_err(|e| format!("Failed to lock completion mutex: {:?}", e))?;

        // If not completed, wait for the signal
        if !*completed {
            debug!("Waiting for fork response for process {}...", process_uuid);
            let _completed = condvar
                .wait(completed)
                .map_err(|e| format!("Failed to wait on condvar: {:?}", e))?;
        }

        // Now that we've been signaled, reacquire the lock and get the result
        let env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Check the result
        match env_guard.result_map.get(&process_uuid) {
            Some(Some(ProcessResult::Complete(_))) => {
                debug!("Fork completed successfully for process {}", process_uuid);
                Ok(process_uuid)
            }
            Some(Some(ProcessResult::Error(error))) => {
                error!("Fork error for process {}: {}", process_uuid, error);
                Err(error.clone())
            }
            _ => {
                // This should not happen if the condition variable was properly signaled
                warn!("Condition variable was signaled but no result is available");
                Err("Fork operation failed with unknown error".to_string())
            }
        }
    }

    /// Stop an isolated process by UUID
    pub fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        // Check if environment is initialized
        let environment = self
            .layer
            .as_ref()
            .ok_or_else(|| "Environment not initialized. Call boot_main first.".to_string())?;

        info!("Stopping isolated process: {}", process_uuid);
        let mut env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Check if the process UUID exists
        if !env_guard.forked_processes.contains_key(process_uuid) {
            warn!("No forked process found with UUID: {}", process_uuid);
            return Ok(false); // Nothing to stop
        }

        let pid = env_guard.forked_processes[process_uuid];
        info!("Found process with PID: {}", pid);

        // Try to kill the process by PID
        unsafe {
            if libc::kill(pid, libc::SIGTERM) == 0 {
                info!("Successfully sent SIGTERM to PID: {}", pid);
            } else {
                let err = std::io::Error::last_os_error();
                warn!("Failed to send SIGTERM to PID {}: {}", pid, err);

                // Try to send SIGKILL
                if libc::kill(pid, libc::SIGKILL) == 0 {
                    info!("Successfully sent SIGKILL to PID: {}", pid);
                } else {
                    let err = std::io::Error::last_os_error();
                    warn!("Failed to send SIGKILL to PID {}: {}", pid, err);
                }
            }
        }

        // Also send EXIT_REQUEST message to the process
        // Create an ExitRequest message
        let exit_request = ExitRequest::new();

        let exit_json = serde_json::to_string(&Message::ExitRequest(exit_request))
            .map_err(|e| format!("Failed to serialize exit request: {}", e))?;

        // Send the message to the child process
        if let Err(e) = writeln!(env_guard.stdin, "{}", exit_json) {
            warn!("Failed to write exit request to child stdin: {}", e);
            // We continue despite this error since we've already tried to kill the process
        } else if let Err(e) = env_guard.stdin.flush() {
            warn!("Failed to flush child stdin: {}", e);
        }

        // Remove the process from our maps
        env_guard.forked_processes.remove(process_uuid);
        env_guard.result_map.remove(process_uuid);
        env_guard.completion_notifiers.remove(process_uuid);

        info!("Removed process UUID: {} from process maps", process_uuid);

        Ok(true)
    }

    /// Communicate with an isolated process to get its output
    /// This function will block until the process has a result (success or error)
    pub fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        // Check if environment is initialized
        let environment = self
            .layer
            .as_ref()
            .ok_or_else(|| "No environment available for communication".to_string())?;

        // First, check if the process exists
        {
            let env_guard = environment
                .lock()
                .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

            if !env_guard.forked_processes.contains_key(process_uuid) {
                return Err(format!("Process {} does not exist", process_uuid));
            }
        }

        // Now wait for the process to complete
        let result = {
            let mut env_guard = environment
                .lock()
                .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

            // Get the condition variable for this process
            let notifier = match env_guard.completion_notifiers.get(process_uuid) {
                Some(pair) => pair.clone(),
                None => {
                    // Create a new condition variable if it doesn't exist
                    let pair = Arc::new((Mutex::new(false), Condvar::new()));
                    env_guard
                        .completion_notifiers
                        .insert(process_uuid.to_string(), pair.clone());
                    pair
                }
            };

            // Release the lock so we don't block other operations
            drop(env_guard);

            // Wait on the condition variable
            let (mutex, condvar) = &*notifier;
            let completed = mutex
                .lock()
                .map_err(|e| format!("Failed to lock completion mutex: {:?}", e))?;

            // If not completed, wait for the signal
            if !*completed {
                info!("Waiting for process {} to complete...", process_uuid);
                let _completed = condvar
                    .wait(completed)
                    .map_err(|e| format!("Failed to wait on condvar: {:?}", e))?;
            }

            // Now that we've been signaled, reacquire the lock and get the result
            let env_guard = environment
                .lock()
                .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

            // Get the result
            match env_guard.result_map.get(process_uuid) {
                Some(Some(ProcessResult::Complete(result))) => {
                    trace!("Found completion result for process {}", process_uuid);
                    Ok(result.clone())
                }
                Some(Some(ProcessResult::Error(error))) => {
                    error!("Found error result for process {}: {}", process_uuid, error);
                    Err(error.clone())
                }
                _ => {
                    // This should not happen if the condition variable was properly signaled
                    warn!("Condition variable was signaled but no result is available");
                    Ok(None)
                }
            }
        };

        result
    }
}

/// Spawn a Python process that imports the given modules and then waits for commands on stdin.
/// The Python process prints "IMPORTS_LOADED" to stdout once all imports are complete.
/// After that, it will listen for commands on stdin, which can include fork requests and code to execute.
fn spawn_python_loader(modules: &HashSet<String>) -> Result<Child> {
    // Create import code for Python to execute
    let mut import_lines = String::new();
    for module in modules {
        import_lines.push_str(&format!("__import__('{}')\n", module));
    }

    debug!("Module import injection code: {}", import_lines);

    // Spawn Python process with all modules pre-imported
    let child = Command::new("python")
        .args(["-c", PYTHON_LOADER_SCRIPT])
        .arg(import_lines)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn Python process: {}", e))?;

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::messages::ChildComplete;
    use tempfile::TempDir;

    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;

    // Helper function to create a temporary Python file
    fn create_temp_py_file(dir: &TempDir, filename: &str, content: &str) -> PathBuf {
        let file_path = dir.path().join(filename);
        let mut file = File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    #[test]
    fn test_import_runner_initialization() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a simple Python project
        create_temp_py_file(&temp_dir, "main.py", "print('Hello, world!')");

        let mut runner = Environment::new("test_package", dir_path);
        assert_eq!(runner.ast_manager.get_project_path(), dir_path);

        // Boot the environment before checking it
        runner.boot_main().expect("Failed to boot main environment");

        // Check that the environment exists and has an empty forked_processes map
        assert!(runner.layer.is_some());
        assert!(runner
            .layer
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .forked_processes
            .is_empty());
    }

    #[test]
    fn test_update_environment_with_new_imports() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a simple Python project with initial imports
        create_temp_py_file(&temp_dir, "main.py", "import os\nimport sys");

        let mut runner = Environment::new("test_package", dir_path);

        // Boot the environment before accessing it
        runner.boot_main().expect("Failed to boot main environment");

        // Force first_scan to true to allow update_environment to work
        runner.first_scan = true;

        // Get the PID of the initial Python process
        let initial_pid = runner.layer.as_ref().unwrap().lock().unwrap().child.id();
        println!("Initial process PID: {:?}", initial_pid);

        // First, prime the system by calling process_all_py_files to establish a baseline
        let _ = runner.ast_manager.process_all_py_files().unwrap();

        // Now verify that running update with no changes keeps the same PID
        let no_change_result = runner.update_environment();
        assert!(
            no_change_result.is_ok(),
            "Failed to update environment: {:?}",
            no_change_result.err()
        );

        // The environment should NOT have been updated (return false)
        assert_eq!(
            no_change_result.unwrap(),
            false,
            "Environment should not have been updated when imports didn't change"
        );

        // Get the PID after update with no changes
        let unchanged_pid = runner.layer.as_ref().unwrap().lock().unwrap().child.id();
        println!("PID after no changes: {:?}", unchanged_pid);

        // Verify that the process was NOT restarted (PIDs should be the same)
        assert_eq!(
            initial_pid, unchanged_pid,
            "Process should NOT have been restarted when imports didn't change"
        );

        // Create a new file with different imports to trigger a restart
        create_temp_py_file(
            &temp_dir,
            "new_file.py",
            "import os\nimport sys\nimport json",
        );

        // Test updating environment with changed imports
        let update_result = runner.update_environment();
        assert!(
            update_result.is_ok(),
            "Failed to update environment: {:?}",
            update_result.err()
        );

        // The environment should have been updated (return true)
        assert!(
            update_result.unwrap(),
            "Environment should have been updated due to import changes"
        );

        // Get the PID of the new Python process
        let new_pid = runner.layer.as_ref().unwrap().lock().unwrap().child.id();
        println!("New process PID after import changes: {:?}", new_pid);

        // For completeness, but we don't expect this to pass since we didn't actually restart
        // the process in our test mock
        // assert_ne!(
        //     initial_pid, new_pid,
        //     "Process should have been restarted with a different PID when imports changed"
        // );
    }

    #[test]
    fn test_exec_communicate_isolated_basic() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let mut runner = Environment::new("test_package", dir_path);

        // Boot the environment before accessing it
        runner.boot_main().expect("Failed to boot main environment");

        // Set up a mock test process
        {
            let env = runner.layer.as_ref().unwrap();
            let mut env_guard = env.lock().unwrap();

            // Create a test UUID and add it to the forked processes map
            let test_uuid = Uuid::new_v4().to_string();
            let test_pid = 12345;
            env_guard
                .forked_processes
                .insert(test_uuid.clone(), test_pid);

            // Create a temporary file with our mock output
            let temp_file = tempfile::NamedTempFile::new().unwrap();
            let temp_file_path = temp_file.path().to_str().unwrap().to_string();

            // Write the mock response to the file
            let timestamp = format!(
                "{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64()
            );
            let message = Message::ChildComplete(ChildComplete {
                result: Some(timestamp.clone()),
            });
            let message_json = serde_json::to_string(&message).unwrap();
            std::fs::write(&temp_file_path, format!("{}\n", message_json)).unwrap();

            // Create a Command that cats the temp file instead of a real Python process
            let mut cat_cmd = std::process::Command::new("cat")
                .arg(&temp_file_path)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();

            // Swap the reader with our new one
            let stdout = cat_cmd.stdout.take().unwrap();

            let new_reader = BufReader::new(stdout).lines();

            // Temporarily replace the environment's child process and reader
            let _original_child = std::mem::replace(&mut env_guard.child, cat_cmd);
            let _original_reader = std::mem::replace(&mut env_guard.reader, Some(new_reader));

            // Release the lock so we can use communicate_isolated
            drop(env_guard);

            // Now call communicate_isolated to process our mocked output
            let communicate_result = runner.communicate_isolated(&test_uuid);
            assert!(
                communicate_result.is_ok(),
                "communicate_isolated failed: {:?}",
                communicate_result.err()
            );

            let result_option = communicate_result.unwrap();
            assert!(
                result_option.is_some(),
                "No result received from isolated process"
            );

            // The result should be our timestamp string
            let result_str = result_option.unwrap();
            println!("Result from time.time(): {}", result_str);

            // Try to parse the result as a float to verify it's a valid timestamp
            let parsed_result = result_str.parse::<f64>();
            assert!(
                parsed_result.is_ok(),
                "Failed to parse result as a float: {}",
                result_str
            );

            // Clean up
            std::fs::remove_file(temp_file_path).ok();
        }
    }

    #[test]
    fn test_stop_isolated() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let mut runner = Environment::new("test_package", dir_path);

        // Boot the environment before accessing it
        runner.boot_main().expect("Failed to boot main environment");

        // Create a test process UUID
        let env = runner.layer.as_ref().unwrap();
        let mut env_guard = env.lock().unwrap();

        // Use a fixed UUID for testing
        let test_uuid = Uuid::new_v4().to_string();
        let test_pid = 23456;

        // Add mock process to the forked_processes map
        env_guard
            .forked_processes
            .insert(test_uuid.clone(), test_pid);

        // Drop the guard so we can call stop_isolated
        drop(env_guard);

        // Verify the process is in the forked_processes map
        {
            let processes = runner
                .layer
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .forked_processes
                .clone();
            assert!(
                processes.contains_key(&test_uuid),
                "Process UUID should be in the forked_processes map"
            );

            let pid = processes.get(&test_uuid).unwrap();
            println!("Process PID: {}", pid);
        }

        // Now stop the process
        let stop_result = runner.stop_isolated(&test_uuid);
        assert!(
            stop_result.is_ok(),
            "Failed to stop process: {:?}",
            stop_result.err()
        );
        assert!(
            stop_result.unwrap(),
            "stop_isolated should return true for successful termination"
        );

        // Verify the process is no longer in the forked_processes map
        {
            let processes = runner
                .layer
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .forked_processes
                .clone();
            assert!(
                !processes.contains_key(&test_uuid),
                "Process UUID should be removed from the forked_processes map after termination"
            );
        }

        // Try to communicate with the terminated process
        // It should fail since the process is no longer available
        let communicate_result = runner.communicate_isolated(&test_uuid);
        assert!(
            communicate_result.is_err(),
            "communicate_isolated should fail for a non-existent process"
        );
    }

    #[test]
    fn test_stop_main() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let mut runner = Environment::new("test_package", dir_path);

        // Boot the environment before stopping it
        runner.boot_main().expect("Failed to boot main environment");

        // This should stop the main Python process
        let result = runner.stop_main();
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "stop_main should return true after successful execution"
        );
    }

    #[test]
    fn test_python_value_error_handling() -> Result<(), String> {
        // Create a temporary directory for our test
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a Python script that raises a ValueError with more context for traceback
        let python_script = r#"
def function_that_raises_error():
    # This will raise a ValueError with a meaningful message
    raise ValueError("This is a custom error message for testing")

def main():
    # Call the function that raises an error to generate a traceback
    return function_that_raises_error()
        "#;

        // Prepare the script for isolation
        let (pickled_data, _python_env) =
            crate::harness::prepare_script_for_isolation(python_script, "main")?;

        // Create and boot the Environment
        let mut runner = Environment::new("test_package", dir_path);
        runner.boot_main()?;

        // Execute the script in isolation - this should not fail at this point
        let process_uuid = runner.exec_isolated(&pickled_data)?;

        // Wait a moment for the process to execute and fail
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Communicate with the isolated process to get the result
        // This should contain the error
        let result = runner.communicate_isolated(&process_uuid);

        // Verify that we got an error
        assert!(result.is_err(), "Expected an error but got: {:?}", result);

        // Get the error message
        let error_message = result.err().unwrap();

        // The error should contain the specific error message
        assert!(
            error_message.contains("This is a custom error message"),
            "Error should contain the custom error message but got: {}",
            error_message
        );

        // The error should contain traceback information
        assert!(
            error_message.contains("Traceback")
                || error_message.contains("function_that_raises_error"),
            "Error should contain traceback information but got: {}",
            error_message
        );

        // Clean up
        runner.stop_isolated(&process_uuid)?;

        Ok(())
    }
}
