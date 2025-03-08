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

    pub forked_processes: HashMap<String, i32>, // Map of UUID to PID

    // New fields for thread-based monitoring
    pub result_map: HashMap<String, Option<ProcessResult>>, // Map of UUID to final result status

    // Condition variables for completion notification
    pub completion_notifiers: HashMap<String, Arc<(Mutex<bool>, Condvar)>>, // Map of UUID to completion notifier

    pub monitor_thread: Option<JoinHandle<()>>, // Thread handle for monitoring output
    pub thread_terminate_tx: Option<Sender<()>>, // Channel to signal thread termination
}

impl Layer {
    /// Start a monitoring thread that continuously reads from the child process stdout
    /// and populates the result_map with parsed output
    pub fn start_monitor_thread(
        &mut self,
        reader: std::io::Lines<BufReader<std::process::ChildStdout>>,
    ) {
        // Create a channel for signaling thread termination
        let (terminate_tx, terminate_rx) = mpsc::channel();
        self.thread_terminate_tx = Some(terminate_tx);

        // Clone the result map into an Arc<Mutex<>> for sharing with the thread
        let result_map = Arc::new(Mutex::new(self.result_map.clone()));

        // Clone the completion notifiers map
        let completion_notifiers = Arc::new(Mutex::new(self.completion_notifiers.clone()));

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
                                    trace!(
                                        "Monitor thread received function result: {:?}",
                                        complete
                                    );

                                    // Find all processes
                                    let mut result_map = result_map.lock().unwrap();
                                    let process_uuids: Vec<String> =
                                        result_map.keys().cloned().collect();

                                    for uuid in process_uuids {
                                        // Store the final result status
                                        if let Some(result) = result_map.get_mut(&uuid) {
                                            *result = Some(ProcessResult::Complete(
                                                complete.result.clone(),
                                            ));

                                            // Notify waiters that the result is ready
                                            let notifiers = completion_notifiers.lock().unwrap();
                                            if let Some(pair) = notifiers.get(&uuid) {
                                                let (mutex, condvar) = &**pair;
                                                let mut completed = mutex.lock().unwrap();
                                                *completed = true;
                                                condvar.notify_all();
                                            }
                                        }
                                    }
                                }
                                Message::ChildError(error) => {
                                    error!("Monitor thread received function error: {:?}", error);

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

                                    for uuid in process_uuids {
                                        if let Some(result) = result_map.get_mut(&uuid) {
                                            *result =
                                                Some(ProcessResult::Error(error_message.clone()));

                                            // Notify waiters that the result is ready (even though it's an error)
                                            let notifiers = completion_notifiers.lock().unwrap();
                                            if let Some(pair) = notifiers.get(&uuid) {
                                                let (mutex, condvar) = &**pair;
                                                let mut completed = mutex.lock().unwrap();
                                                *completed = true;
                                                condvar.notify_all();
                                            }
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
                                        println!(
                                            "[{}:{}] {}",
                                            uuid, log_line.stream_name, log_line.content
                                        );
                                    } else {
                                        // If we can't match it to a specific process, log it with PID
                                        println!(
                                            "Unmatched log: [{}:{}] {}",
                                            log_line.pid, log_line.stream_name, log_line.content
                                        );
                                    }
                                }
                                Err(_) => {
                                    // If parsing fails, print the raw line
                                    println!("Raw output: {}", line);
                                }
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
