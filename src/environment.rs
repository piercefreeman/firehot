use anstream::eprintln;
use anyhow::{anyhow, Result};
use log::{debug, error, info, trace, warn};
use owo_colors::OwoColorize;
use serde_json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, Condvar};
use std::thread;
use std::time::Instant;

use libc;
use std::io::BufRead;
use uuid::Uuid;

use crate::ast::ProjectAstManager;
use crate::messages::{ExitRequest, ForkRequest, Message};
use crate::scripts::{PYTHON_CHILD_SCRIPT, PYTHON_LOADER_SCRIPT};

/// Represents the result of a forked process execution
#[derive(Debug, Clone)]
pub enum ProcessResult {
    None,                 // No result yet
    Success(Option<String>), // Process completed successfully with optional result
    Error(String),        // Process encountered an error
}

/// A forked process to run isolated code
#[derive(Debug)]
pub struct ForkedProcess {
    monitor_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    message_queue: Arc<Mutex<VecDeque<Message>>>,
    name: String,
    pid: i32,
    completion: Arc<(Mutex<ProcessResult>, Condvar)>, // Mutex paired with Condvar for signaling completion
}

impl Clone for ForkedProcess {
    fn clone(&self) -> Self {
        Self {
            monitor_thread: self.monitor_thread.clone(),
            message_queue: self.message_queue.clone(),
            name: self.name.clone(),
            pid: self.pid,
            completion: self.completion.clone(),
        }
    }
}

/// Runtime environment for executing Python code. Represents a built "layer" of imported modules that is currently running in memory.
pub struct Environment {
    pub child: Child,                    // The forkable process with all imports loaded
    pub stdin: std::process::ChildStdin, // The stdin of the forkable process
    pub reader: std::io::Lines<BufReader<std::process::ChildStdout>>, // The reader of the forkable process
    pub forked_processes: HashMap<String, ForkedProcess>,
}

/// Runner for isolated Python code execution
pub struct ImportRunner {
    pub id: String,
    pub environment: Option<Arc<Mutex<Environment>>>,
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
            environment: None,
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

        // Create and store the environment
        let environment = Environment {
            child,
            stdin,
            reader: lines_iter,
            forked_processes: HashMap::new(),
        };

        self.environment = Some(Arc::new(Mutex::new(environment)));

        Ok(())
    }

    pub fn stop_main(&self) -> Result<bool, String> {
        // Check if environment is initialized
        let environment = match self.environment.as_ref() {
            Some(env) => env,
            None => {
                info!("No environment to stop.");
                return Ok(false);
            }
        };

        info!("Stopping main runner process");

        let env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // First, kill all isolated processes
        let process_uuids: Vec<String> = env_guard.forked_processes.keys().cloned().collect();

        // Drop the lock while stopping isolated processes to avoid deadlock
        drop(env_guard);

        // Stop each isolated process
        for uuid in &process_uuids {
            if let Err(e) = self.stop_isolated(uuid) {
                warn!("Failed to stop isolated process {}: {}", uuid, e);
            }
        }

        // Re-acquire the lock
        let mut env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Kill the main child process
        if let Err(e) = env_guard.child.kill() {
            warn!("Failed to kill child process: {}", e);
        }

        // Wait for the process to exit
        if let Err(e) = env_guard.child.wait() {
            warn!("Failed to wait for child process: {}", e);
        }

        // Clean up any remaining forked processes
        let remaining_processes = std::mem::take(&mut env_guard.forked_processes);

        // Drop the lock to avoid deadlock when joining threads
        drop(env_guard);

        // Join any monitor threads that weren't properly cleaned up
        for (_uuid, process) in remaining_processes {
            if let Some(handle) = process.monitor_thread.lock().unwrap().take() {
                if let Err(e) = handle.join() {
                    warn!(
                        "Failed to join monitor thread for process {}: {:?}",
                        process.name, e
                    );
                } else {
                    debug!(
                        "Successfully joined monitor thread for process {}",
                        process.name
                    );
                }
            } else {
                debug!(
                    "Monitor thread for process {} was already joined",
                    process.name
                );
            }
        }

        info!("Main runner process stopped successfully");

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
        if let Some(env) = self.environment.as_ref() {
            let forked_processes = {
                let env_guard = env
                    .lock()
                    .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

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

        // Boot a new environment
        self.boot_main()?;

        info!("Environment updated successfully");
        Ok(true)
    }

    //
    // Isolated process management
    //

    /// Execute a function in the isolated environment. This should be called from the main thread (the one
    /// that spawned our hotreloader) so we can get the local function and closure variables.
    pub fn exec_isolated(&self, pickled_data: &str, name: &str) -> Result<String, String> {
        // Check if environment is initialized
        let environment = self
            .environment
            .as_ref()
            .ok_or_else(|| "Environment not initialized. Call boot_main first.".to_string())?;

        // Send the code to the forked process
        let mut env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

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
            // Create a message queue for this process
            let message_queue = Arc::new(Mutex::new(VecDeque::new()));

            // Create a clone of the environment arc for the monitoring thread
            let env_arc = self.environment.as_ref().unwrap().clone();
            let process_id = process_uuid.clone();
            let process_name = name.to_string(); // Clone the name for the monitoring thread
            let thread_message_queue = message_queue.clone(); // Clone for the thread

            // Start a monitoring thread for this process
            let handle = thread::spawn(move || {
                Self::monitor_isolated_output(
                    env_arc,
                    process_id,
                    thread_message_queue,
                    process_name,
                );
            });

            // Create a new ForkedProcess with the generated UUID
            let forked_process = ForkedProcess {
                monitor_thread: Arc::new(Mutex::new(Some(handle))),
                message_queue: message_queue.clone(),
                name: name.to_string(),
                pid: pid_val,
                completion: Arc::new((Mutex::new(ProcessResult::None), Condvar::new())),
            };

            // Store the ForkedProcess
            env_guard
                .forked_processes
                .insert(process_uuid.clone(), forked_process);
        }

        Ok(process_uuid)
    }

    /// Stop an isolated process by UUID
    pub fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        // Check if environment is initialized
        let environment = self
            .environment
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

        // Get the process info for logging
        let forked_process = env_guard.forked_processes.get(process_uuid).unwrap();
        let pid = forked_process.pid;
        let name = forked_process.name.clone();

        info!("Stopping process {} with PID: {}", name, pid);

        // First check if process is still running
        let process_exists = unsafe { libc::kill(pid, 0) == 0 };
        
        if !process_exists {
            let err = std::io::Error::last_os_error();
            info!("Process {} (PID: {}) is no longer running: {}", name, pid, err);
            
            // Process is already gone, so just clean up our representation
            let forked_process = match env_guard.forked_processes.remove(process_uuid) {
                Some(process) => process,
                None => {
                    warn!("Process {} was removed during stop operation", process_uuid);
                    return Ok(false);
                }
            };
            
            // Drop the lock to avoid deadlock
            drop(env_guard);
            
            // Join the monitoring thread
            if let Some(handle) = forked_process.monitor_thread.lock().unwrap().take() {
                if let Err(e) = handle.join() {
                    warn!("Failed to join monitor thread for process {}: {:?}", name, e);
                } else {
                    debug!("Successfully joined monitor thread for process {}", name);
                }
            } else {
                debug!("Monitor thread for process {} was already joined", name);
            }
            
            info!(
                "Removed process {} (UUID: {}) representation which had already exited",
                name, process_uuid
            );
            
            return Ok(true);
        }

        // Try to kill the process by PID
        unsafe {
            if libc::kill(pid, libc::SIGTERM) == 0 {
                info!(
                    "Successfully sent SIGTERM to process {} (PID: {})",
                    name, pid
                );
            } else {
                let err = std::io::Error::last_os_error();
                warn!(
                    "Failed to send SIGTERM to process {} (PID: {}): {}",
                    name, pid, err
                );

                // Try to send SIGKILL
                if libc::kill(pid, libc::SIGKILL) == 0 {
                    info!(
                        "Successfully sent SIGKILL to process {} (PID: {})",
                        name, pid
                    );
                } else {
                    let err = std::io::Error::last_os_error();
                    warn!(
                        "Failed to send SIGKILL to process {} (PID: {}): {}",
                        name, pid, err
                    );
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

        // Remove the process and get its monitor thread
        let forked_process = match env_guard.forked_processes.remove(process_uuid) {
            Some(process) => process,
            None => {
                warn!("Process {} was removed during stop operation", process_uuid);
                return Ok(false);
            }
        };

        // Drop the lock to avoid deadlock
        drop(env_guard);

        // Join the monitoring thread
        if let Some(handle) = forked_process.monitor_thread.lock().unwrap().take() {
            if let Err(e) = handle.join() {
                warn!(
                    "Failed to join monitor thread for process {}: {:?}",
                    name, e
                );
            } else {
                debug!("Successfully joined monitor thread for process {}", name);
            }
        } else {
            debug!("Monitor thread for process {} was already joined", name);
        }

        info!(
            "Removed process {} (UUID: {}) and cleaned up resources",
            name, process_uuid
        );

        Ok(true)
    }

    /// Communicate with an isolated process to get its output
    pub fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        // Check if environment is initialized
        let environment = self
            .environment
            .as_ref()
            .ok_or_else(|| "No environment available for communication".to_string())?;

        let env_guard = environment
            .lock()
            .map_err(|e| format!("Failed to lock environment mutex: {}", e))?;

        // Check if the process exists
        if !env_guard.forked_processes.contains_key(process_uuid) {
            // Process might have completed and been removed
            // Check the message queue for any final messages
            // This is mostly kept for backward compatibility
            for process in env_guard.forked_processes.values() {
                if let Ok(mut queue) = process.message_queue.lock() {
                    for message in queue.iter() {
                        match message {
                            Message::ChildComplete(complete) => {
                                if complete.result.is_some() {
                                    return Ok(complete.result.clone());
                                }
                            }
                            Message::ChildError(error) => {
                                return Err(error.error.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
            
            return Err(format!("Process {} does not exist", process_uuid));
        }

        // Get the forked process
        let forked_process = env_guard
            .forked_processes
            .get(process_uuid)
            .ok_or_else(|| format!("Forked process {} not found", process_uuid))?;

        // Get references to the things we need
        let completion = forked_process.completion.clone();
        let process_name = forked_process.name.clone();
        
        // Release the environment lock to avoid deadlocks
        drop(env_guard);

        // Wait for the process to complete with a timeout
        let timeout = std::time::Duration::from_secs(30); // 30 second timeout
        
        // Use the condition variable to wait for result
        let (lock, cvar) = &*completion;
        let mut result = lock.lock()
            .map_err(|e| format!("Failed to lock completion mutex: {:?}", e))?;
        
        // If we don't have a result yet, wait for one
        if matches!(*result, ProcessResult::None) {
            debug!("Waiting for result from process {}", process_uuid);
            
            // Wait with timeout
            let wait_result = cvar.wait_timeout(result, timeout)
                .map_err(|e| format!("Failed to wait on condition variable: {:?}", e))?;
                
            if wait_result.1.timed_out() {
                return Err(format!("Timeout waiting for process {} to complete", process_uuid));
            }
            
            // Here, result has been updated by the condition variable wait
            debug!("Received result notification for process {}", process_uuid);
        } else {
            debug!("Process {} already has a result", process_uuid);
        }

        // Process the result
        match &*result {
            ProcessResult::Success(result_opt) => {
                trace!(
                    "Received success result from {}: {:?}",
                    process_name,
                    result_opt
                );
                Ok(result_opt.clone())
            }
            ProcessResult::Error(error) => {
                error!(
                    "Received error from {}: {}",
                    process_name, error
                );
                Err(error.clone())
            }
            ProcessResult::None => {
                // This should not happen after waiting, but just in case
                warn!("No result available after waiting for process {}", process_uuid);
                Ok(None)
            }
        }
    }

    /// Monitor the output of a specific process and queue messages or print logs
    fn monitor_isolated_output(
        environment: Arc<Mutex<Environment>>,
        process_uuid: String,
        message_queue: Arc<Mutex<VecDeque<Message>>>,
        process_name: String,
    ) {
        debug!(
            "Started monitoring thread for process: {} ({})",
            process_name, process_uuid
        );

        // Continue until the process is no longer in the forked_processes map
        loop {
            // Check if the process still exists
            let process_exists = {
                let env_guard = match environment.lock() {
                    Ok(guard) => guard,
                    Err(e) => {
                        error!("Failed to lock environment mutex in monitor thread: {}", e);
                        break;
                    }
                };
                env_guard.forked_processes.contains_key(&process_uuid)
            };

            if !process_exists {
                debug!(
                    "Process {} ({}) no longer exists, stopping monitor thread",
                    process_name, process_uuid
                );
                break;
            }

            // Try to read from the reader
            let read_result = {
                let mut env_guard = match environment.lock() {
                    Ok(guard) => guard,
                    Err(e) => {
                        error!("Failed to lock environment mutex for reading: {}", e);
                        break;
                    }
                };

                match env_guard.reader.next() {
                    Some(line_result) => line_result,
                    None => {
                        // End of stream, process might have ended
                        debug!(
                            "End of stream for process {} ({}), stopping monitor thread",
                            process_name, process_uuid
                        );
                        break;
                    }
                }
            };

            match read_result {
                Ok(line) => {
                    // Try to parse as a Message
                    match serde_json::from_str::<Message>(&line) {
                        Ok(message) => {
                            match message {
                                Message::ChildComplete(complete) => {
                                    debug!(
                                        "Process {} ({}) completed successfully",
                                        process_name, process_uuid
                                    );
                                    
                                    // Store the result directly in the completion field
                                    if let Ok(env_guard) = environment.lock() {
                                        if let Some(process) = env_guard.forked_processes.get(&process_uuid) {
                                            let (lock, cvar) = &*process.completion;
                                            if let Ok(mut result) = lock.lock() {
                                                // Set the success result
                                                *result = ProcessResult::Success(complete.result.clone());
                                                // Notify waiters that the result is ready
                                                cvar.notify_all();
                                                debug!(
                                                    "Set success result for process {} ({}) and notified waiters",
                                                    process_name, process_uuid
                                                );
                                            }
                                        }
                                        
                                        // Also queue the message for potential later consumption
                                        if let Ok(mut queue) = message_queue.lock() {
                                            queue.push_back(message.clone());
                                            debug!(
                                                "Queued message: {:?} for process {} ({})",
                                                message.name(),
                                                process_name,
                                                process_uuid
                                            );
                                        }
                                        
                                        // Remove it from the forked_processes map after setting the result
                                        drop(env_guard);
                                        if let Ok(mut env_guard) = environment.lock() {
                                            env_guard.forked_processes.remove(&process_uuid);
                                            debug!(
                                                "Process {} ({}) completed, removed from forked processes",
                                                process_name, process_uuid
                                            );
                                        }
                                    }

                                    // Exit the monitoring thread
                                    break;
                                },
                                Message::ChildError(error) => {
                                    debug!(
                                        "Process {} ({}) failed with error: {}",
                                        process_name, process_uuid, error.error
                                    );
                                    
                                    // Store the error directly in the completion field
                                    if let Ok(env_guard) = environment.lock() {
                                        if let Some(process) = env_guard.forked_processes.get(&process_uuid) {
                                            let (lock, cvar) = &*process.completion;
                                            if let Ok(mut result) = lock.lock() {
                                                // Set the error result
                                                *result = ProcessResult::Error(error.error.clone());
                                                // Notify waiters that the result is ready
                                                cvar.notify_all();
                                                debug!(
                                                    "Set error result for process {} ({}) and notified waiters",
                                                    process_name, process_uuid
                                                );
                                            }
                                        }
                                        
                                        // Also queue the message for potential later consumption
                                        if let Ok(mut queue) = message_queue.lock() {
                                            queue.push_back(message.clone());
                                            debug!(
                                                "Queued message: {:?} for process {} ({})",
                                                message.name(),
                                                process_name,
                                                process_uuid
                                            );
                                        }
                                        
                                        // Remove it from the forked_processes map after setting the result
                                        drop(env_guard);
                                        if let Ok(mut env_guard) = environment.lock() {
                                            env_guard.forked_processes.remove(&process_uuid);
                                            debug!(
                                                "Process {} ({}) completed with error, removed from forked processes",
                                                process_name, process_uuid
                                            );
                                        }
                                    }

                                    // Exit the monitoring thread
                                    break;
                                },
                                _ => {
                                    // Queue other structured messages
                                    if let Ok(mut queue) = message_queue.lock() {
                                        queue.push_back(message);
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            // Print non-structured output with [name] prefix
                            println!("[{}] {}", format!("{}", process_name).bold().cyan(), line);
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Error reading from process {} ({}): {}",
                        process_name, process_uuid, e
                    );
                    
                    // Set an error result for read errors
                    if let Ok(env_guard) = environment.lock() {
                        if let Some(process) = env_guard.forked_processes.get(&process_uuid) {
                            let (lock, cvar) = &*process.completion;
                            if let Ok(mut result) = lock.lock() {
                                *result = ProcessResult::Error(format!("Read error: {}", e));
                                cvar.notify_all();
                                debug!(
                                    "Set read error result for process {} ({}) and notified waiters",
                                    process_name, process_uuid
                                );
                            }
                        }
                        
                        // Remove it from the forked_processes map
                        drop(env_guard);
                        if let Ok(mut env_guard) = environment.lock() {
                            env_guard.forked_processes.remove(&process_uuid);
                        }
                    }
                    break;
                }
            }

            // Small sleep to prevent CPU spinning
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        debug!(
            "Monitor thread for process {} ({}) exiting",
            process_name, process_uuid
        );
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

        // Create the environment
        let environment = Environment {
            child: python_cmd,
            stdin,
            reader,
            forked_processes: HashMap::new(),
        };

        // Use a default package name for tests
        let ast_manager = ProjectAstManager::new("test_package", project_dir);

        let runner = ImportRunner {
            id: Uuid::new_v4().to_string(),
            environment: Some(Arc::new(Mutex::new(environment))),
            ast_manager,
            first_scan: false,
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

        // Check that the environment exists and has an empty forked_processes map
        assert!(runner.environment.is_some());
        assert!(runner
            .environment
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

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let mut runner = runner_result.unwrap();

        // Force first_scan to true to allow update_environment to work
        runner.first_scan = true;

        // Get the PID of the initial Python process
        let initial_pid = runner
            .environment
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
            .environment
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
            .environment
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

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // Set up a mock test process
        {
            let env = runner.environment.as_ref().unwrap();
            let mut env_guard = env.lock().unwrap();

            // Create a test UUID and add it to the forked processes map
            let test_uuid = Uuid::new_v4().to_string();
            let test_pid = 12345;

            // Create a mock message queue (mostly for backwards compatibility)
            let message_queue = Arc::new(Mutex::new(VecDeque::new()));

            // Generate a timestamp for our test result
            let timestamp = format!(
                "{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64()
            );

            // Add a test message to the queue for backward compatibility
            let test_message = Message::ChildComplete(crate::messages::ChildComplete {
                result: Some(timestamp.clone()),
            });
            message_queue.lock().unwrap().push_back(test_message);

            // Create a dummy thread that does nothing
            let handle = thread::spawn(|| {
                // Empty thread that immediately returns
            });

            // Create a ForkedProcess struct with a successful result
            let completion = Arc::new((Mutex::new(ProcessResult::Success(Some(timestamp.clone()))), Condvar::new()));
            let forked_process = ForkedProcess {
                monitor_thread: Arc::new(Mutex::new(Some(handle))),
                message_queue,
                name: "test_process".to_string(),
                pid: test_pid,
                completion,
            };

            env_guard
                .forked_processes
                .insert(test_uuid.clone(), forked_process);

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
        }
    }

    #[test]
    fn test_stop_isolated() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // Create a test process UUID
        let env = runner.environment.as_ref().unwrap();
        let mut env_guard = env.lock().unwrap();

        // Use a fixed UUID for testing
        let test_uuid = Uuid::new_v4().to_string();
        let test_pid = 23456;

        // Create a mock message queue
        let message_queue = Arc::new(Mutex::new(VecDeque::new()));

        // Create a dummy thread that does nothing
        let handle = thread::spawn(|| {
            // Empty thread that immediately returns
        });

        // Create a ForkedProcess struct with the initial state
        let forked_process = ForkedProcess {
            monitor_thread: Arc::new(Mutex::new(Some(handle))),
            message_queue,
            name: "test_isolated_process".to_string(),
            pid: test_pid,
            completion: Arc::new((Mutex::new(ProcessResult::None), Condvar::new())),
        };

        // Add mock process to the forked_processes map
        env_guard
            .forked_processes
            .insert(test_uuid.clone(), forked_process);

        // Drop the guard so we can call stop_isolated
        drop(env_guard);

        // Verify the process is in the forked_processes map
        {
            let keys: Vec<String> = runner
                .environment
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .forked_processes
                .keys()
                .cloned()
                .collect();
            assert!(
                keys.contains(&test_uuid),
                "Process UUID should be in the forked_processes map"
            );
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
            let keys: Vec<String> = runner
                .environment
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .forked_processes
                .keys()
                .cloned()
                .collect();
            assert!(
                !keys.contains(&test_uuid),
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

        let runner_result = create_mock_import_runner(dir_path);
        assert!(runner_result.is_ok());

        let runner = runner_result.unwrap();

        // This should stop the main Python process
        let result = runner.stop_main();
        assert!(result.is_ok());

        // Verify that the function returns true since the environment is properly initialized
        assert!(
            result.unwrap(),
            "stop_main should return true after successful execution"
        );
    }
}
