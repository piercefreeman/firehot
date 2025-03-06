use anstream::eprintln;
use anyhow::Result;
use log::{debug, error, info, trace, warn};
use owo_colors::OwoColorize;
use serde_json;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::process::Child;
use std::time::{Duration, Instant};

use libc;
use scripts::PYTHON_CHILD_SCRIPT;
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::ast::ProjectAstManager;
use crate::messages::{ExitRequest, ForkRequest, Message};
use crate::scripts;

/// Runner for isolated Python code execution
pub struct ImportRunner {
    pub id: String,
    pub child: Arc<Mutex<Child>>,
    pub stdin: Arc<Mutex<std::process::ChildStdin>>,
    pub reader: Arc<Mutex<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
    pub forked_processes: Arc<Mutex<HashMap<String, i32>>>, // Map of UUID to PID
    pub ast_manager: ProjectAstManager, // Project AST manager for this environment

    first_scan: bool,
}

impl ImportRunner {
    pub fn new(project_name: &str, project_path: &str) -> Self {
        // Create a new AST manager for this project
        let mut ast_manager = ast::ProjectAstManager::new(project_name, package_path);
        info!("Created AST manager for project: {}", project_name);

        Self {
            id: Uuid::new_v4().to_string(),
            ast_manager,
            first_scan: false,
        }
    }

    pub fn boot_main(&self) -> Result<(), String> {
        if !self.first_scan {
            // Process Python files to get initial imports. This baseline warms the cache by building
            // the index of file shas and their imports.
            info!("Processing Python files in: {}", package_path);
            let third_party_modules = ast_manager.process_all_py_files().map_err(|e| {
                PyRuntimeError::new_err(format!("Failed to process Python files: {}", e))
            })?;
            self.first_scan = true;
        }

        let start_time = Instant::now();

        // Spawn Python subprocess to load modules
        info!(
            "Spawning Python subprocess to load {} modules",
            third_party_modules.len()
        );
        let mut child = spawn_python_loader(&third_party_modules).map_err(|e| {
            PyRuntimeError::new_err(format!("Failed to spawn Python loader: {}", e))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("Failed to capture stdin for python process"))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            PyRuntimeError::new_err("Failed to capture stdout for python process")
        })?;

        let reader = BufReader::new(stdout);
        let mut lines_iter = reader.lines();

        // Wait for the ImportComplete message
        info!("Waiting for import completion...");
        let mut imports_loaded = false;
        for line in &mut lines_iter {
            let line =
                line.map_err(|e| PyRuntimeError::new_err(format!("Failed to read line: {}", e)))?;

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
                        return Err(PyRuntimeError::new_err(format!(
                            "Import error: {}: {}",
                            error.error,
                            error.traceback.unwrap_or_default()
                        )));
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
            return Err(PyRuntimeError::new_err(
                "Python loader did not report successful imports",
            ));
        }

        // Calculate total setup time and log completion
        let elapsed = start_time.elapsed();
        let elapsed_ms = elapsed.as_millis();

        eprintln!(
            "\n{} {} {} {}{} {}\n",
            "✓".green().bold(),
            "Import environment booted in".white().bold(),
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
            format!("with ID: {}", runner_id).white().bold()
        );
    }

    /// Execute a function in the isolated environment. This should be called from the main thread (the one
    /// that spawned our hotreloader) so we can get the local function and closure variables.
    pub fn exec_isolated(&self, pickled_data: &str) -> Result<String, String> {
        debug!("Executing function in isolated environment");
        // Create the Python execution code that will unpickle and run the function
        // Errors don't seem to get caught in the parent process, so we need to log them here
        let exec_code = format!(
            r#"
pickled_str = "{}"

{}
            "#,
            pickled_data, PYTHON_CHILD_SCRIPT,
        );

        // Send the code to the forked process
        let mut stdin_guard = self
            .stdin
            .lock()
            .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;

        // Create a ForkRequest message
        let fork_request = Message::ForkRequest(ForkRequest::new(exec_code));
        let serialized = serde_json::to_string(&fork_request)
            .map_err(|e| format!("Failed to serialize message: {}", e))?;

        debug!("Sending fork request message");
        writeln!(stdin_guard, "{}", serialized)
            .map_err(|e| format!("Failed to write to child process: {}", e))?;

        drop(stdin_guard);

        // Wait for response
        let mut reader_guard = self
            .reader
            .lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;

        for line in &mut *reader_guard {
            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;

            // Parse the line as a message
            if let Ok(message) = serde_json::from_str::<Message>(&line) {
                debug!("Received message: {:?}", message);

                match message {
                    Message::ForkResponse(response) => {
                        // Handle fork response message
                        let fork_pid = response.child_pid;
                        info!("Received fork response with PID: {}", fork_pid);

                        // Generate a UUID for this process
                        let process_uuid = Uuid::new_v4().to_string();

                        // Store the PID with its UUID
                        let mut forked_processes = self
                            .forked_processes
                            .lock()
                            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
                        forked_processes.insert(process_uuid.clone(), fork_pid);
                        drop(forked_processes);

                        // Wait a bit for the fork to complete to avoid race conditions
                        std::thread::sleep(Duration::from_millis(100));

                        // Return the UUID
                        info!("Process UUID created: {}", process_uuid);
                        return Ok(process_uuid);
                    }
                    Message::ChildError(error) => {
                        // Handle child error message
                        error!("Function execution failed: {}", error.error);
                        return Err(format!("Function execution failed: {}", error.error));
                    }
                    Message::UnknownError(error) => {
                        // Handle unknown error message
                        error!("Process error: {}", error.error);
                        return Err(format!("Process error: {}", error.error));
                    }
                    _ => {
                        // Log unhandled message types
                        debug!("Unhandled message type: {:?}", message.name());
                        continue;
                    }
                }
            } else {
                // If we can't parse it as a message, log and continue
                debug!("[python stdout]: {}", line);
            }
        }

        error!("Unexpected end of output from child process");
        Err("Unexpected end of output from child process".to_string())
    }

    /// Stop an isolated process by UUID
    pub fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        info!("Stopping isolated process: {}", process_uuid);
        let mut forked_processes = self
            .forked_processes
            .lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;

        if let Some(pid) = forked_processes.get(process_uuid) {
            // Send EXIT_REQUEST message to the process
            let mut stdin_guard = self
                .stdin
                .lock()
                .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;

            // Create an ExitRequest message
            let exit_request = ExitRequest::new();
            let serialized = serde_json::to_string(&exit_request)
                .map_err(|e| format!("Failed to serialize message: {}", e))?;
            debug!("Sending exit request to process: {}", process_uuid);
            writeln!(stdin_guard, "{}", serialized)
                .map_err(|e| format!("Failed to write exit request: {}", e))?;
            drop(stdin_guard);

            // Give the process a chance to exit gracefully
            std::thread::sleep(Duration::from_millis(100));

            // If it's still running, terminate it forcefully
            unsafe {
                // Use libc::kill to terminate the process
                if libc::kill(*pid, libc::SIGTERM) != 0 {
                    let err_msg = format!(
                        "Failed to terminate process: {}",
                        std::io::Error::last_os_error()
                    );
                    error!("{}", err_msg);
                    return Err(err_msg);
                }
            }

            // Remove the process from the map
            forked_processes.remove(process_uuid);
            info!("Isolated process {} stopped successfully", process_uuid);
            Ok(true)
        } else {
            // This process doesn't own the UUID
            warn!("Process UUID not found: {}", process_uuid);
            Ok(false)
        }
    }

    /// Communicate with an isolated process to get its output
    pub fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        debug!("Communicating with isolated process: {}", process_uuid);
        let forked_processes = self
            .forked_processes
            .lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;

        if !forked_processes.contains_key(process_uuid) {
            // This process doesn't own the UUID
            warn!("Process UUID not found for communication: {}", process_uuid);
            return Ok(None);
        }
        drop(forked_processes);

        // Read from the reader
        let mut reader_guard = self
            .reader
            .lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;

        // Check for messages from the process
        for _ in 0..1000 {
            // Limit to avoid infinite loop
            match reader_guard.next() {
                Some(Ok(line)) => {
                    // Parse the line as a message
                    if let Ok(message) = serde_json::from_str::<Message>(&line) {
                        match message {
                            Message::ChildComplete(complete) => {
                                // If we have a result, return it
                                if let Some(result) = complete.result {
                                    debug!("Received result from isolated process");
                                    return Ok(Some(result));
                                }
                            }
                            Message::ChildError(error) => {
                                // Return error message as output
                                error!("Error from isolated process: {}", error.error);
                                return Ok(Some(format!("Error: {}", error.error)));
                            }
                            _ => {
                                // For other message types, add them to the output
                                let json = serde_json::to_string(&message)
                                    .map_err(|e| format!("Failed to serialize message: {}", e))?;
                                debug!("Unhandled message type from isolated process: {}", json);
                            }
                        }
                    } else {
                        // Log unrecognized output but don't add it to the result
                        debug!("[python stdout]: {}", line);
                    }
                }
                Some(Err(e)) => {
                    let err_msg = format!("Error reading output: {}", e);
                    error!("{}", err_msg);
                    return Err(err_msg);
                }
                None => break, // No more output
            }
        }

        trace!("No result received from isolated process");
        Ok(None)
    }

    pub fn stop_main(&self) -> Result<bool, String> {
        info!("Stopping main runner process");
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        info!("Main runner process stopped");
        Ok(true)
    }

    /// Update the runner by checking for changes in imports and restarting if necessary
    pub fn update_environment(&mut self) -> Result<bool, String> {
        let (added, removed) = self
            .ast_manager
            .compute_import_delta()
            .map_err(|e| format!("Failed to compute import delta: {}", e))?;

        debug!("Added imports: {:?}", added);
        debug!("Removed imports: {:?}", removed);

        // Check if there are any changes in imports
        if !added.is_empty() || !removed.is_empty() {
            info!("Detected changes in imports:");
            if !added.is_empty() {
                info!("Added imports: {:?}", added);
            }
            if !removed.is_empty() {
                info!("Removed imports: {:?}", removed);
            }

            // Attempt to stop any existing forked processes
            let forked_processes = self.forked_processes.lock().unwrap();

            // Copy the keys to avoid borrowing issues
            let process_uuids: Vec<String> = forked_processes.keys().cloned().collect();
            drop(forked_processes);

            // Stop each process
            for process_uuid in process_uuids {
                if let Err(e) = self.stop_isolated(&process_uuid) {
                    error!("Failed to stop process {}: {}", process_uuid, e);
                }
            }

            // Stop the main process
            if let Err(e) = self.stop_main() {
                error!("Failed to stop main process: {}", e);
            }

            // Boot the main process again
            self.boot_main()
                .map_err(|e| format!("Failed to reboot main: {}", e))?;

            // Return true to indicate that the environment was updated
            Ok(true)
        } else {
            debug!("No changes in imports detected");
            Ok(false)
        }
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

    use crate::scripts::PYTHON_LOADER_SCRIPT;
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    // Helper function to create a temporary Python file
    fn create_temp_py_file(dir: &TempDir, filename: &str, content: &str) -> PathBuf {
        let file_path = dir.path().join(filename);
        let mut file = File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    // Helper to create a mock ImportRunner with basic functionality
    fn create_mock_import_runner(project_dir: &str) -> Result<ImportRunner, String> {
        // Create a minimal Python process that can handle basic messages
        let mut python_cmd = Command::new("python")
            .args(["-c", PYTHON_LOADER_SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn Python process: {}", e))?;

        let stdin = python_cmd
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture stdin".to_string())?;

        let stdout = python_cmd
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture stdout".to_string())?;

        let reader = BufReader::new(stdout).lines();

        // Use a default package name for tests
        let ast_manager = ProjectAstManager::new("test_package", project_dir);

        let runner = ImportRunner {
            id: Uuid::new_v4().to_string(),
            child: Arc::new(Mutex::new(python_cmd)),
            stdin: Arc::new(Mutex::new(stdin)),
            reader: Arc::new(Mutex::new(reader)),
            forked_processes: Arc::new(Mutex::new(HashMap::new())),
            ast_manager,
        };

        Ok(runner)
    }

    #[test]
    fn test_import_runner_initialization() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a simple Python project
        create_temp_py_file(&temp_dir, "main.py", "print('Hello, world!')");

        let runner_result = create_mock_import_runner(dir_path);
        assert!(
            runner_result.is_ok(),
            "Failed to create ImportRunner: {:?}",
            runner_result.err()
        );

        let runner = runner_result.unwrap();
        assert_eq!(runner.ast_manager.get_project_path(), dir_path);
        assert!(runner.forked_processes.lock().unwrap().is_empty());
    }

    #[test]
    fn test_update_environment_with_new_imports() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a simple Python project with initial imports
        create_temp_py_file(&temp_dir, "main.py", "import os\nimport sys");

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let mut runner = runner_result.unwrap();

        // Get the PID of the initial Python process
        let initial_pid = runner.child.lock().unwrap().id();
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
        let unchanged_pid = runner.child.lock().unwrap().id();
        println!("PID after no changes: {:?}", unchanged_pid);

        // Verify that the process was NOT restarted (PIDs should be the same)
        assert_eq!(
            initial_pid, unchanged_pid,
            "Process should NOT have been restarted when imports didn't change"
        );

        // Now update the file with different imports to trigger a restart
        create_temp_py_file(&temp_dir, "main.py", "import os\nimport sys\nimport json");

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
        let new_pid = runner.child.lock().unwrap().id();
        println!("New process PID after import changes: {:?}", new_pid);

        // Verify that the process was restarted (PIDs should be different)
        assert_ne!(
            initial_pid, new_pid,
            "Process should have been restarted with a different PID when imports changed"
        );
    }

    #[test]
    fn test_exec_communicate_isolated_basic() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // Use real pickled data for time.time() with zero args
        // echo 'import base64; import pickle; import time; payload = {"func_module_path": "time", "func_name": "time", "func_qualname": "time", "args": tuple()}; pickled_data = base64.b64encode(pickle.dumps(payload)).decode("utf-8"); print(f"Pickled data for time.time() with zero args:\\n{pickled_data}")' > /tmp/generate_pickle.py && python /tmp/generate_pickle.py
        let pickled_data = "gASVRwAAAAAAAAB9lCiMEGZ1bmNfbW9kdWxlX3BhdGiUjAR0aW1llIwJZnVuY19uYW1llGgCjA1mdW5jX3F1YWxuYW1llGgCjARhcmdzlCl1Lg==";

        // Execute the time.time() function in the isolated Python process
        // This returns a UUID for the process, not the actual result
        let exec_result = runner.exec_isolated(pickled_data);
        assert!(
            exec_result.is_ok(),
            "exec_isolated failed: {:?}",
            exec_result.err()
        );

        let process_uuid = exec_result.unwrap();
        println!("Process UUID: {}", process_uuid);

        // Give some time for the child process to complete
        std::thread::sleep(Duration::from_millis(500));

        // Now use communicate_isolated to get the actual result
        let communicate_result = runner.communicate_isolated(&process_uuid);
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

        // The result should be a string containing a floating-point timestamp
        let result_str = result_option.unwrap();
        println!("Result from time.time(): {}", result_str);

        // Try to parse the result as a float to verify it's a valid timestamp
        // Note: We're not checking the actual value as it will change each time
        let parsed_result = result_str.parse::<f64>();
        assert!(
            parsed_result.is_ok(),
            "Failed to parse result as a float: {}",
            result_str
        );
    }

    #[test]
    fn test_stop_isolated() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // Create a pickled data that will make Python sleep for 10 seconds
        // This will create a long-running process that we can terminate
        // echo 'import base64; import pickle; import time; payload = {"func_module_path": "time", "func_name": "sleep", "func_qualname": "sleep", "args": (10,)}; pickled_data = base64.b64encode(pickle.dumps(payload)).decode("utf-8"); print(f"Pickled data for time.sleep(10):\\n{pickled_data}")' > /tmp/generate_sleep_pickle.py && python /tmp/generate_sleep_pickle.py
        let sleep_pickled_data = "gASVUAAAAAAAAAB9lCiMEGZ1bmNfbW9kdWxlX3BhdGiUjAR0aW1llIwJZnVuY19uYW1llIwFc2xlZXCUjA1mdW5jX3F1YWxuYW1llGgEjARhcmdzlEsKhZR1Lg==";

        // Start the sleep process
        let exec_result = runner.exec_isolated(sleep_pickled_data);
        assert!(
            exec_result.is_ok(),
            "Failed to start sleep process: {:?}",
            exec_result.err()
        );

        let process_uuid = exec_result.unwrap();
        println!("Started sleep process with UUID: {}", process_uuid);

        // Verify the process is in the forked_processes map
        {
            let processes = runner.forked_processes.lock().unwrap();
            assert!(
                processes.contains_key(&process_uuid),
                "Process UUID should be in the forked_processes map"
            );

            let pid = processes.get(&process_uuid).unwrap();
            println!("Process PID: {}", pid);
        }

        // Now stop the process
        let stop_result = runner.stop_isolated(&process_uuid);
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
            let processes = runner.forked_processes.lock().unwrap();
            assert!(
                !processes.contains_key(&process_uuid),
                "Process UUID should be removed from the forked_processes map after termination"
            );
        }

        // Try to communicate with the terminated process
        // It should return None since the process is no longer available
        let communicate_result = runner.communicate_isolated(&process_uuid);
        assert!(
            communicate_result.is_ok(),
            "communicate_isolated failed: {:?}",
            communicate_result.err()
        );
        assert!(
            communicate_result.unwrap().is_none(),
            "communicate_isolated should return None for a terminated process"
        );
    }

    #[test]
    fn test_stop_main() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // This should stop the main Python process
        let result = runner.stop_main();
        assert!(result.is_ok());

        // Since the process may terminate quickly, we can only verify the function returns success
        assert!(
            result.unwrap(),
            "stop_main should return true after successful execution"
        );
    }
}
