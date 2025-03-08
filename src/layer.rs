use log::{debug, error, trace, warn};
use serde_json::{self};
use std::collections::HashMap;
use std::io::BufReader;
use std::process::Child;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::messages::Message;
use crate::multiplex_logs::{parse_multiplexed_line, MultiplexedLogLine};
use crate::async_resolve::AsyncResolve;

/// Result from the initial fork
#[derive(Debug, Clone)]
pub enum ForkResult {
    /// Fork completed successfully with an optional return value
    Complete(Option<String>),
    /// Fork failed with an error message
    Error(String),
}

/// Result from a forked process
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// Process completed successfully with an optional return value
    Complete(Option<String>),
    /// Process failed with an error message
    Error(String),
    // Raw log output from the process
    //Log(MultiplexedLogLine),
}


/// Runtime layer for executing Python code. This is a single "built" layer that should be immutable. Any client executed code will be in a forked process and any
pub struct Layer {
    pub child: Child,                    // The forkable process with all imports loaded
    pub stdin: std::process::ChildStdin, // The stdin of the forkable process
    pub reader: Option<std::io::Lines<BufReader<std::process::ChildStdout>>>, // The reader of the forkable process

    pub forked_processes: HashMap<String, i32>, // Map of UUID to PID

    // These are pinged when the forked process finishes startup - either successful or failure
    pub fork_resolvers: HashMap<String, AsyncResolve<ForkResult>>, // Map of UUID to fork resolver
    
    // These are pinged when the process completes execution
    pub completion_resolvers: HashMap<String, AsyncResolve<ProcessResult>>, // Map of UUID to completion resolver

    pub monitor_thread: Option<JoinHandle<()>>, // Thread handle for monitoring output
    pub thread_terminate_tx: Option<Sender<()>>, // Channel to signal thread termination
}

impl Layer {
    /// Start a monitoring thread that continuously reads from the child process stdout
    /// and populates the result_map with parsed output
    pub fn start_monitor_thread(&mut self) {
        // Create a channel for signaling thread termination
        let (terminate_tx, terminate_rx) = mpsc::channel();
        self.thread_terminate_tx = Some(terminate_tx);

        // Take ownership of the reader
        let reader = self.reader.take().expect("Reader should be available");

        // Clone the resolvers for sharing with the thread
        let fork_resolvers = Arc::new(Mutex::new(self.fork_resolvers.clone()));
        let completion_resolvers = Arc::new(Mutex::new(self.completion_resolvers.clone()));

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
                        // All lines streamed from the forked process (even our own messages)
                        // should be multiplexed lines
                        match parse_multiplexed_line(&line) {
                            Ok(log_line) => {
                                // Find which process this log belongs to based on PID
                                let forked_definitions = forked_processes.lock().unwrap();
                                let mut process_uuid = None;

                                for (uuid, pid) in forked_definitions.iter() {
                                    if *pid == log_line.pid as i32 {
                                        process_uuid = Some(uuid.clone());
                                        break;
                                    }
                                }

                                // Just print the log, don't store it
                                if let Some(uuid) = process_uuid {
                                    Self::handle_message(
                                        &log_line.content,
                                        Some(&uuid),
                                        &fork_resolvers,
                                        &completion_resolvers,
                                        &forked_processes
                                    );
                                } else {
                                    // If we can't match it to a specific process, log it with PID
                                    println!(
                                        "Unmatched log: [{}:{}] {}",
                                        log_line.pid, log_line.stream_name, log_line.content
                                    );
                                }
                            }
                            Err(e) => {
                                // If parsing fails, print the raw line
                                Self::handle_message(
                                    &line,
                                    None,
                                    &fork_resolvers,
                                    &completion_resolvers,
                                    &forked_processes
                                );
                            }
                        } 
                    }
                    Some(Err(e)) => {
                        error!("Error reading from child process: {}", e);
                        break;
                    }
                    None => {
                        // End of stream
                        debug!("End of child process output stream");
                        break;
                    }
                }

                // Short sleep to avoid tight loop
                thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        self.monitor_thread = Some(thread_handle);
    }

    /// Handle various messages from the child process
    fn handle_message(
        content: &str,
        uuid: Option<&String>,
        fork_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ForkResult>>>>,
        completion_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ProcessResult>>>>,
        forked_processes: &Arc<Mutex<HashMap<String, i32>>>
    ) {
        if let Ok(message) = serde_json::from_str::<Message>(content) {
            match message {
                Message::ForkResponse(response) => {
                    // Handle fork response and update the forked processes map
                    debug!(
                        "Monitor thread received fork response: {:?}",
                        response
                    );

                    // Store the PID in the forked processes map
                    let mut forked_processes_guard = forked_processes.lock().unwrap();
                    forked_processes_guard.insert(
                        response.request_id.clone(),
                        response.child_pid,
                    );
                    drop(forked_processes_guard);
                    
                    // Resolve the fork status
                    let fork_resolvers_guard = fork_resolvers.lock().unwrap();
                    if let Some(resolver) = fork_resolvers_guard.get(&response.request_id) {
                        resolver.resolve(ForkResult::Complete(Some(response.child_pid.to_string())));
                    }
                    drop(fork_resolvers_guard);
                }
                Message::ChildComplete(complete) => {
                    trace!(
                        "Monitor thread received function result: {:?}",
                        complete
                    );

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");
                    
                    // Resolve the completion
                    let completion_resolvers_guard = completion_resolvers.lock().unwrap();
                    if let Some(resolver) = completion_resolvers_guard.get(uuid) {
                        resolver.resolve(ProcessResult::Complete(complete.result.clone()));
                    }
                    drop(completion_resolvers_guard);
                }
                Message::ChildError(error) => {
                    trace!(
                        "Monitor thread received error result: {:?}",
                        error
                    );

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");
                    
                    // Resolve the completion with an error
                    let completion_resolvers_guard = completion_resolvers.lock().unwrap();
                    if let Some(resolver) = completion_resolvers_guard.get(uuid) {
                        resolver.resolve(ProcessResult::Error(error.error.clone()));
                    }
                    drop(completion_resolvers_guard);
                }
                /*Message::ForkError(error) => {
                    warn!(
                        "Monitor thread received fork error: {:?}",
                        error
                    );

                    // Resolve the fork status with an error
                    let fork_resolvers_guard = fork_resolvers.lock().unwrap();
                    if let Some(resolver) = fork_resolvers_guard.get(&error.request_id) {
                        resolver.resolve(ForkResult::Error(error.error.clone()));
                    }
                    drop(fork_resolvers_guard);
                }*/
                Message::UnknownError(error) => {
                    error!(
                        "Monitor thread received unknown error: {}",
                        error.error
                    );

                    // For unknown errors, we don't have a UUID, so we can't resolve a specific promise
                    // Only log the error for now
                }
                _ => {
                    // Ignore other message types
                    trace!("Monitor thread received unknown message type");
                }
            }
        } else {
            // Not a message - just print the raw content
            println!("{}", content);
        }
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
