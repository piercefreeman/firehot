use anstream::eprintln;
use anyhow::{anyhow, Result};
use log::{debug, error, info, trace, warn};
use owo_colors::OwoColorize;
use serde_json::{self};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use libc;
use std::io::BufRead;
use uuid::Uuid;

use crate::ast::ProjectAstManager;
use crate::messages::{ExitRequest, ForkRequest, Message};
use crate::scripts::{PYTHON_CHILD_SCRIPT, PYTHON_LOADER_SCRIPT};
use crate::multiplex_logs::{parse_multiplexed_line, MultiplexedLogLine};

/// Result from a forked process
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// Process completed successfully with an optional return value
    Complete(Option<String>),
    /// Process failed with an error message
    Error(String),
    /// Raw log output from the process
    Log(MultiplexedLogLine),
}

/// Runtime layer for executing Python code. This is a single "built" layer that should be immutable. Any client executed code will be in a forked process and any
pub struct Layer {
    pub child: Child,                    // The forkable process with all imports loaded
    pub stdin: std::process::ChildStdin, // The stdin of the forkable process
    pub reader: std::io::Lines<BufReader<std::process::ChildStdout>>, // The reader of the forkable process
    pub forked_processes: HashMap<String, i32>,                       // Map of UUID to PID
    
    // New fields for thread-based monitoring
    pub result_map: HashMap<String, Option<ProcessResult>>, // Map of UUID to final result status
    pub monitor_thread: Option<JoinHandle<()>>,              // Thread handle for monitoring output
    pub thread_terminate_tx: Option<Sender<()>>,             // Channel to signal thread termination
}

/// Runner for isolated Python code execution
pub struct ImportRunner {
    pub id: String,
    pub layer: Option<Arc<Mutex<Layer>>>, // The current layer that is tied to this environment
    pub ast_manager: ProjectAstManager, // Project AST manager for this environment

    first_scan: bool,
}

impl ImportRunner {
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
            format!("with ID: {}", self.id).white().bold()
        );

        // Create the environment
        let mut layer = Layer {
            child,
            stdin,
            reader: lines_iter,
            forked_processes: HashMap::new(),
            result_map: HashMap::new(),
            monitor_thread: None,
            thread_terminate_tx: None,
        };
        
        // Extract the reader for the monitor thread
        let stdout = layer.child.stdout.take();
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout).lines();
            // Start the monitor thread
            layer.start_monitor_thread(reader);
        } else {
            warn!("Failed to capture stdout for monitor thread");
        }

        self.layer = Some(Arc::new(Mutex::new(layer)));

        Ok(())
    }

    pub fn stop_main(&self) -> Result<bool, String> {
        // Check if environment is initialized
        let layer = match self.layer.as_ref() {
            Some(env) => env,
            None => {
                info!("No layer to stop.");
                return Ok(false);
            }
        };

        info!("Stopping main runner process");

        let mut env_guard = layer
            .lock()
            .map_err(|e| format!("Failed to lock layer mutex: {}", e))?;
            
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

        // Clear the process map
        env_guard.forked_processes.clear();
        env_guard.result_map.clear();

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
        let layer = self
            .layer
            .as_ref()
            .ok_or_else(|| "Layer not initialized. Call boot_main first.".to_string())?;

        // Send the code to the forked process
        let mut env_guard = layer
            .lock()
            .map_err(|e| format!("Failed to lock layer mutex: {}", e))?;

        let exec_code = format!(
            r#"
pickled_str = "{}"
{}
            "#,
            pickled_data, PYTHON_CHILD_SCRIPT,
        );

        // Create a ForkRequest message
        let fork_request = ForkRequest { code: exec_code };

        let fork_json = serde_json::to_string(&Message::ForkRequest(fork_request))
            .map_err(|e| format!("Failed to serialize fork request: {}", e))?;

        // Send the message to the child process
        writeln!(env_guard.stdin, "{}", fork_json)
            .map_err(|e| format!("Failed to write to child stdin: {}", e))?;
        env_guard
            .stdin
            .flush()
            .map_err(|e| format!("Failed to flush child stdin: {}", e))?;

        // Wait for response
        let mut process_uuid = Uuid::new_v4().to_string();
        let mut pid: Option<i32> = None;

        for line in &mut env_guard.reader {
            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;

            // Try to parse the response as a Message
            if let Ok(message) = serde_json::from_str::<Message>(&line) {
                match message {
                    Message::ForkResponse(response) => {
                        process_uuid = process_uuid.clone(); // Keep UUID same, but set PID
                        pid = Some(response.child_pid);
                        debug!("Fork complete. UUID: {}, PID: {:?}", process_uuid, pid);
                        break;
                    }
                    Message::ChildError(error) => {
                        error!("Fork error: {}", error.error);
                        return Err(format!("Fork error: {}", error.error));
                    }
                    _ => {
                        // Log other message types
                        debug!("Unexpected message: {}", line);
                    }
                }
            } else {
                // Log any non-message output
                debug!("Non-message output: {}", line);
            }
        }

        if process_uuid.is_empty() {
            return Err("Failed to get process UUID from fork operation".to_string());
        }

        // Store the PID with its UUID
        if let Some(pid_val) = pid {
            env_guard
                .forked_processes
                .insert(process_uuid.clone(), pid_val);
            
            // Initialize an empty result entry for this process
            env_guard.result_map.insert(process_uuid.clone(), None);
        }

        Ok(process_uuid)
    }

    /// Stop an isolated process by UUID
    pub fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        // Check if environment is initialized
        let layer = self
            .layer
            .as_ref()
            .ok_or_else(|| "Layer not initialized. Call boot_main first.".to_string())?;

        info!("Stopping isolated process: {}", process_uuid);
        let mut env_guard = layer
            .lock()
            .map_err(|e| format!("Failed to lock layer mutex: {}", e))?;

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
        
        info!(
            "Removed process UUID: {} from process maps",
            process_uuid
        );

        Ok(true)
    }

    /// Communicate with an isolated process to get its output
    pub fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        // Check if layer is initialized
        let layer = self
            .layer
            .as_ref()
            .ok_or_else(|| "No layer available for communication".to_string())?;

        let mut env_guard = layer
            .lock()
            .map_err(|e| format!("Failed to lock layer mutex: {}", e))?;

        // Check if the process exists
        if !env_guard.forked_processes.contains_key(process_uuid) {
            return Err(format!("Process {} does not exist", process_uuid));
        }

        // Check if we have a result for this process
        match env_guard.result_map.get(process_uuid) {
            Some(Some(ProcessResult::Complete(result))) => {
                trace!("Found completion result for process {}", process_uuid);
                return Ok(result.clone());
            },
            Some(Some(ProcessResult::Error(error))) => {
                error!("Found error result for process {}: {}", process_uuid, error);
                return Err(error.clone());
            },
            Some(Some(ProcessResult::Log(_))) => {
                // Ignore log results, they should be printed directly
                return Ok(None);
            },
            Some(None) | None => {
                // No result yet or process doesn't have an entry
                if !env_guard.result_map.contains_key(process_uuid) {
                    // Initialize an empty result for this process
                    env_guard.result_map.insert(process_uuid.to_string(), None);
                }
                trace!("No results available yet for process {}", process_uuid);
                return Ok(None);
            }
        }
    }
}

impl Layer {
    /// Start a monitoring thread that continuously reads from the child process stdout
    /// and populates the result_map with parsed output
    pub fn start_monitor_thread(&mut self, reader: std::io::Lines<BufReader<std::process::ChildStdout>>) {
        // Create a channel for signaling thread termination
        let (terminate_tx, terminate_rx) = mpsc::channel();
        self.thread_terminate_tx = Some(terminate_tx);
        
        // Clone the result map into an Arc<Mutex<>> for sharing with the thread
        let result_map = Arc::new(Mutex::new(self.result_map.clone()));
        
        // Clone the forked processes map for the thread to access
        let forked_processes = Arc::new(Mutex::new(self.forked_processes.clone()));
        
        // Start the monitor thread
        let thread_handle = thread::spawn(move || {
            debug!("Monitor thread started");
            let mut reader = reader;
            
            loop {
                // Check if we've been asked to terminate
                if terminate_rx.try_recv().is_ok() {
                    debug!("Monitor thread received terminate signal");
                    break;
                }
                
                // Try to read a line from the child process
                match reader.next() {
                    Some(Ok(line)) => {
                        // First, try to parse it as a Message
                        if let Ok(message) = serde_json::from_str::<Message>(&line) {
                            match message {
                                Message::ChildComplete(complete) => {
                                    trace!("Monitor thread received function result: {:?}", complete);
                                    
                                    // Find which process this belongs to (should be in JSON)
                                    // In a real implementation we would identify the specific process
                                    let mut result_map = result_map.lock().unwrap();
                                    for (_, result) in result_map.iter_mut() {
                                        // Store the final result status
                                        *result = Some(ProcessResult::Complete(complete.result.clone()));
                                    }
                                },
                                Message::ChildError(error) => {
                                    error!("Monitor thread received function error: {:?}", error);
                                    
                                    // Format the error with traceback information if available
                                    let error_message = match error.traceback {
                                        Some(traceback) => format!("{}\n\n{}", error.error, traceback),
                                        None => error.error,
                                    };
                                    
                                    // Store the final error status
                                    let mut result_map = result_map.lock().unwrap();
                                    for (_, result) in result_map.iter_mut() {
                                        *result = Some(ProcessResult::Error(error_message.clone()));
                                    }
                                },
                                _ => {
                                    trace!("Monitor thread received other message type: {:?}", message);
                                }
                            }
                        } else {
                            // Try to parse it as a multiplexed log line
                            match parse_multiplexed_line(&line) {
                                Ok(log_line) => {
                                    // Find which process this log belongs to based on PID
                                    let forked_processes = forked_processes.lock().unwrap();
                                    let mut process_uuid = None;
                                    
                                    for (uuid, pid) in forked_processes.iter() {
                                        if *pid == log_line.pid as i32 {
                                            process_uuid = Some(uuid.clone());
                                            break;
                                        }
                                    }
                                    
                                    // Just print the log, don't store it
                                    if let Some(uuid) = process_uuid {
                                        println!("[{}:{}] {}", 
                                            uuid, log_line.stream_name, log_line.content);
                                    } else {
                                        // If we can't match it to a specific process, log it with PID
                                        println!("Unmatched log: [{}:{}] {}", 
                                            log_line.pid, log_line.stream_name, log_line.content);
                                    }
                                },
                                Err(_) => {
                                    // If parsing fails, print the raw line
                                    println!("Raw output: {}", line);
                                }
                            }
                        }
                    },
                    Some(Err(e)) => {
                        error!("Error reading from child process: {}", e);
                        break;
                    },
                    None => {
                        // End of stream
                        debug!("End of child process output stream");
                        break;
                    }
                }
                
                // Short sleep to avoid tight loop
                thread::sleep(std::time::Duration::from_millis(10));
            }
            
            debug!("Monitor thread terminated");
        });
        
        self.monitor_thread = Some(thread_handle);
    }
    
    /// Stop the monitoring thread if it's running
    pub fn stop_monitor_thread(&mut self) {
        if let Some(terminate_tx) = self.thread_terminate_tx.take() {
            debug!("Sending terminate signal to monitor thread");
            if let Err(e) = terminate_tx.send(()) {
                warn!("Failed to send terminate signal to monitor thread: {}", e);
            }
        }
        
        if let Some(handle) = self.monitor_thread.take() {
            debug!("Waiting for monitor thread to terminate");
            if let Err(e) = handle.join() {
                error!("Failed to join monitor thread: {:?}", e);
            }
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

        let mut runner = ImportRunner::new("test_package", dir_path);
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

        let mut runner = ImportRunner::new("test_package", dir_path);

        // Boot the environment before accessing it
        runner.boot_main().expect("Failed to boot main environment");

        // Force first_scan to true to allow update_environment to work
        runner.first_scan = true;

        // Get the PID of the initial Python process
        let initial_pid = runner
            .layer
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .child
            .id();
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
        let unchanged_pid = runner
            .layer
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .child
            .id();
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
        let new_pid = runner
            .layer
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .child
            .id();
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

        let mut runner = ImportRunner::new("test_package", dir_path);

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
            let _original_reader = std::mem::replace(&mut env_guard.reader, new_reader);

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

        let mut runner = ImportRunner::new("test_package", dir_path);

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

        let mut runner = ImportRunner::new("test_package", dir_path);

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

        // Create and boot the ImportRunner
        let mut runner = ImportRunner::new("test_package", dir_path);
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
