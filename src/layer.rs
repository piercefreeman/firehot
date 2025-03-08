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
    /// Raw log output from the process
    Log(MultiplexedLogLine),
}


/// Runtime layer for executing Python code. This is a single "built" layer that should be immutable. Any client executed code will be in a forked process and any
pub struct Layer {
    pub child: Child,                    // The forkable process with all imports loaded
    pub stdin: std::process::ChildStdin, // The stdin of the forkable process
    pub reader: Option<std::io::Lines<BufReader<std::process::ChildStdout>>>, // The reader of the forkable process

    pub forked_processes: HashMap<String, i32>, // Map of UUID to PID

    // These are pinged when the forked process finishes startup - either successful or failure
    pub fork_status_map: HashMap<String, Option<ForkResult>>, // Map of UUID to fork result
    pub fork_notifiers: HashMap<String, Arc<(Mutex<bool>, Condvar)>>, // Map of UUID to fork notifier

    // Condition variables for completion notification
    pub result_map: HashMap<String, Option<ProcessResult>>, // Map of UUID to final result status
    pub completion_notifiers: HashMap<String, Arc<(Mutex<bool>, Condvar)>>, // Map of UUID to completion notifier

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

        // Clone the result map into an Arc<Mutex<>> for sharing with the thread
        let result_map = Arc::new(Mutex::new(self.result_map.clone()));
        let completion_notifiers = Arc::new(Mutex::new(self.completion_notifiers.clone()));

        // Clone the fork notifiers map
        let fork_status_map = Arc::new(Mutex::new(self.fork_status_map.clone()));
        let fork_notifiers = Arc::new(Mutex::new(self.fork_notifiers.clone()));

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
                                        &result_map,
                                        &completion_notifiers,
                                        &fork_status_map,
                                        &fork_notifiers,
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
                                    &result_map,
                                    &completion_notifiers,
                                    &fork_status_map,
                                    &fork_notifiers,
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

            debug!("Monitor thread terminated");
        });

        self.monitor_thread = Some(thread_handle);
    }

    fn handle_message(
        content: &str,
        uuid: Option<&String>,
        result_map: &Arc<Mutex<HashMap<String, Option<ProcessResult>>>>,
        completion_notifiers: &Arc<Mutex<HashMap<String, Arc<(Mutex<bool>, Condvar)>>>>,
        fork_status_map: &Arc<Mutex<HashMap<String, Option<ForkResult>>>>,
        fork_notifiers: &Arc<Mutex<HashMap<String, Arc<(Mutex<bool>, Condvar)>>>>,
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

                    // Store the fork response in the result map
                    let mut result_map_guard = result_map.lock().unwrap();

                    result_map_guard.insert(
                        response.response_id,
                        response.child_pid,
                    );
                    
                    // Update the fork status map
                    let mut fork_status_map = fork_status_map.lock().unwrap();
                    fork_status_map.insert(
                        response.response_id,
                        Some(ForkResult::Complete(response.child_pid)),
                    );

                    // Notify waiters that the fork is ready
                    let notifiers = fork_notifiers.lock().unwrap();
                    if let Some(pair) = notifiers.get(&response.response_id) {
                        let (mutex, condvar) = &**pair;
                        let mut completed = mutex.lock().unwrap();
                        *completed = true;
                        condvar.notify_all();
                    }
                }
                Message::ChildComplete(complete) => {
                    trace!(
                        "Monitor thread received function result: {:?}",
                        complete
                    );

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");
                    
                    // Find all processes
                    let mut result_map = result_map.lock().unwrap();
                    if let Some(result) = result_map.get_mut(&uuid)
                        {
                            *result = Some(ProcessResult::Complete(
                                complete.result.clone(),
                            ));

                            // Notify waiters that the result is ready
                            // TODO: Refactor this into a common handler
                            let notifiers =
                                completion_notifiers.lock().unwrap();
                            if let Some(pair) = notifiers.get(&uuid) {
                                let (mutex, condvar) = &**pair;
                                let mut completed =
                                    mutex.lock().unwrap();
                                *completed = true;
                                condvar.notify_all();
                            }
                        }
                }
                Message::ChildError(error) => {
                    error!(
                        "Monitor thread received function error: {:?}",
                        error
                    );

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");

                    // Format the error with traceback information if available
                    let error_message = match error.traceback {
                        Some(traceback) => {
                            format!("{}\n\n{}", error.error, traceback)
                        }
                        None => error.error,
                    };

                    // Store the final error status for all processes
                    let mut result_map = result_map.lock().unwrap();
                    let process_uuids: Vec<String> =
                    result_map.keys().cloned().collect();

                    if let Some(result) = result_map.get_mut(&uuid)
                    {
                        *result = Some(ProcessResult::Error(
                            error_message.clone(),
                        ));

                        // Notify waiters that the result is ready (even though it's an error)
                        let notifiers =
                            completion_notifiers.lock().unwrap();
                        if let Some(pair) = notifiers.get(&uuid) {
                            let (mutex, condvar) = &**pair;
                            let mut completed =
                                mutex.lock().unwrap();
                            *completed = true;
                            condvar.notify_all();
                        }
                    }
                }
                _ => {
                    trace!(
                        "Monitor thread received other message type: {:?}",
                        message
                    );
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
