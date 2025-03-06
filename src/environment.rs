use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

// Add PyO3 imports for Python bindings
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use libc;
use scripts::PYTHON_CHILD_SCRIPT;

use crate::messages::{Message, ForkRequest, ExitRequest};
use crate::scripts;
use crate::ast::{self, ProjectAstManager};
use std::collections::HashSet;

/// Runner for isolated Python code execution
pub struct ImportRunner {
    pub id: String,
    pub child: Arc<Mutex<Child>>,
    pub stdin: Arc<Mutex<std::process::ChildStdin>>,
    pub reader: Arc<Mutex<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
    pub forked_processes: Arc<Mutex<HashMap<String, i32>>>, // Map of UUID to PID
    pub ast_manager: ProjectAstManager, // Project AST manager for this environment
}

impl ImportRunner {
    /// Execute a function in the isolated environment. This should be called from the main thread (the one
    /// that spawned our hotreloader) so we can get the local function and closure variables.
    pub fn exec_isolated(
        &self, 
        module_path: &str,
        pickled_data: &str
    ) -> Result<String, String> {
        // Create the Python execution code that will unpickle and run the function
        // Errors don't seem to get caught in the parent process, so we need to log them here
        let exec_code = format!(
            r#"
module_path = "{}"
pickled_str = "{}"

{}
            "#, 
            module_path,
            pickled_data,
            PYTHON_CHILD_SCRIPT,
        );
        
        // Send the code to the forked process
        let mut stdin_guard = self.stdin.lock()
            .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;
        
        // Create a ForkRequest message
        let fork_request = Message::ForkRequest(ForkRequest::new(exec_code));
        let serialized = serde_json::to_string(&fork_request)
            .map_err(|e| format!("Failed to serialize message: {}", e))?;
        
            println!("Sending message: {}", serialized);
        writeln!(stdin_guard, "{}", serialized)
            .map_err(|e| format!("Failed to write to child process: {}", e))?;
        
        drop(stdin_guard);
        
        // Wait for response
        let mut reader_guard = self.reader.lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;

        for line in &mut *reader_guard {
            let line = line.map_err(|e| format!("Failed to read line: {}", e))?;
            
            // Parse the line as a message
            if let Ok(message) = serde_json::from_str::<Message>(&line) {
                println!("Received message: {:?}", message);

                match message {
                    Message::ForkResponse(response) => {
                        // Handle fork response message
                        let fork_pid = response.child_pid;
                        
                        // Generate a UUID for this process
                        let process_uuid = Uuid::new_v4().to_string();
                        
                        // Store the PID with its UUID
                        let mut forked_processes = self.forked_processes.lock()
                            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
                        forked_processes.insert(process_uuid.clone(), fork_pid);
                        drop(forked_processes);
                        
                        // Wait a bit for the fork to complete to avoid race conditions
                        std::thread::sleep(Duration::from_millis(100));
                        
                        // Return the UUID
                        return Ok(process_uuid);
                    },
                    Message::ChildError(error) => {
                        // Handle child error message
                        return Err(format!("Function execution failed: {}", error.error));
                    },
                    Message::UnknownError(error) => {
                        // Handle unknown error message
                        return Err(format!("Process error: {}", error.error));
                    },
                    _ => {
                        // Log unhandled message types
                        println!("Unhandled message type: {:?}", message.name());
                        continue;
                    }
                }
            } else {
                // If we can't parse it as a message, log and continue
                println!("[python stdout]: {}", line);
            }
        }
        
        Err("Unexpected end of output from child process".to_string())
    }
    
    /// Stop an isolated process by UUID
    pub fn stop_isolated(&self, process_uuid: &str) -> Result<bool, String> {
        let mut forked_processes = self.forked_processes.lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
            
        if let Some(pid) = forked_processes.get(process_uuid) {
            // Send EXIT_REQUEST message to the process
            let mut stdin_guard = self.stdin.lock()
                .map_err(|e| format!("Failed to lock stdin mutex: {}", e))?;
            
            // Create an ExitRequest message
            let exit_request = ExitRequest::new();
            let serialized = serde_json::to_string(&exit_request)
                .map_err(|e| format!("Failed to serialize message: {}", e))?;
            writeln!(stdin_guard, "{}", serialized)
                .map_err(|e| format!("Failed to write exit request: {}", e))?;
            drop(stdin_guard);
            
            // Give the process a chance to exit gracefully
            std::thread::sleep(Duration::from_millis(100));
            
            // If it's still running, terminate it forcefully
            unsafe {
                // Use libc::kill to terminate the process
                if libc::kill(*pid, libc::SIGTERM) != 0 {
                    return Err(format!("Failed to terminate process: {}", std::io::Error::last_os_error()));
                }
            }
            
            // Remove the process from the map
            forked_processes.remove(process_uuid);
            Ok(true)
        } else {
            // This process doesn't own the UUID
            Ok(false)
        }
    }
    
    /// Communicate with an isolated process to get its output
    pub fn communicate_isolated(&self, process_uuid: &str) -> Result<Option<String>, String> {
        let forked_processes = self.forked_processes.lock()
            .map_err(|e| format!("Failed to lock forked processes mutex: {}", e))?;
            
        if !forked_processes.contains_key(process_uuid) {
            // This process doesn't own the UUID
            return Ok(None);
        }
        drop(forked_processes);
        
        // Read from the reader
        let mut reader_guard = self.reader.lock()
            .map_err(|e| format!("Failed to lock reader mutex: {}", e))?;
            
        // Check for messages from the process
        for _ in 0..1000 { // Limit to avoid infinite loop
            match reader_guard.next() {
                Some(Ok(line)) => {
                    // Parse the line as a message
                    if let Ok(message) = serde_json::from_str::<Message>(&line) {
                        match message {
                            Message::ChildComplete(complete) => {
                                // If we have a result, return it
                                if let Some(result) = complete.result {
                                    return Ok(Some(result));
                                }
                            },
                            Message::ChildError(error) => {
                                // Return error message as output
                                return Ok(Some(format!("Error: {}", error.error)));
                            },
                            _ => {
                                // For other message types, add them to the output
                                let json = serde_json::to_string(&message)
                                    .map_err(|e| format!("Failed to serialize message: {}", e))?;
                                /*output.push_str(&json);
                                output.push('\n');*/
                                println!("[hotreload]: Unhandled message type: {}", json);
                            }
                        }
                    } else {
                        // Log unrecognized output but don't add it to the result
                        println!("[python stdout]: {}", line);
                    }
                },
                Some(Err(e)) => return Err(format!("Error reading output: {}", e)),
                None => break, // No more output
            }
        }
        
        Ok(None)
    }

    pub fn stop_main(&self) -> Result<bool, String> {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        Ok(true)
    }

    /// Update the runner by checking for changes in imports and restarting if necessary
    pub fn update_environment(&mut self) -> Result<bool, String> {
        // Compute the delta of imports
        let (added, removed) = self.ast_manager.compute_import_delta()
            .map_err(|e| format!("Failed to compute import delta: {}", e))?;

        println!("Added: {:?}", added);
        println!("Removed: {:?}", removed);

        // Check if there are any changes in imports
        if !added.is_empty() || !removed.is_empty() {
            println!("Detected changes in imports:");
            if !added.is_empty() {
                println!("Added imports: {:?}", added);
            }
            if !removed.is_empty() {
                println!("Removed imports: {:?}", removed);
            }

            // Attempt to stop any existing forked processes
            let mut forked_processes = self.forked_processes.lock().unwrap();
            for (uuid, pid) in forked_processes.iter() {
                // Try to stop each process, but continue if one fails
                match self.stop_isolated(uuid) {
                    Ok(_) => println!("Successfully stopped forked process {}", uuid),
                    Err(e) => println!("Warning: Failed to stop forked process {}: {}", uuid, e),
                }
            }
            forked_processes.clear();
            drop(forked_processes); // Release the lock

            // Get all the current third-party modules
            let third_party_modules = self.ast_manager.process_all_py_files()
                .map_err(|e| format!("Failed to process Python files: {}", e))?;

            // Stop the current main process
            self.stop_main()?;

            // Create and spawn a new Python loader process
            let mut child = crate::spawn_python_loader(&third_party_modules)
                .map_err(|e| format!("Failed to spawn Python loader: {}", e))?;

            let stdin = child.stdin.take()
                .ok_or_else(|| format!("Failed to capture stdin for python process"))?;

            let stdout = child.stdout.take()
                .ok_or_else(|| format!("Failed to capture stdout for python process"))?;

            let reader = BufReader::new(stdout);
            let mut lines_iter = reader.lines();

            // Wait for the ImportComplete message
            let mut imports_loaded = false;
            for line in &mut lines_iter {
                let line = line.map_err(|e| format!("Failed to read line: {}", e))?;

                // Parse the line as a message
                if let Ok(message) = serde_json::from_str::<Message>(&line) {
                    match message {
                        Message::ImportComplete(_) => {
                            imports_loaded = true;
                            break;
                        }
                        Message::ImportError(error) => {
                            return Err(format!(
                                "Import error: {}: {}",
                                error.error,
                                error.traceback.unwrap_or_default()
                            ));
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
                return Err("Python loader did not report successful imports".to_string());
            }

            // Update the runner with the new process
            *self.child.lock().unwrap() = child;
            *self.stdin.lock().unwrap() = stdin;
            *self.reader.lock().unwrap() = lines_iter;

            return Ok(true); // Environment was updated
        }

        Ok(false) // No changes, environment not updated
    }
}