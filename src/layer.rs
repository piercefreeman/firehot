use log::{debug, error, info, trace, warn};
use owo_colors::OwoColorize;
use serde_json::{self};
use std::collections::HashMap;
use std::io::BufReader;
use std::process::Child;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::async_resolve::AsyncResolve;
use crate::messages::Message;
use crate::multiplex_logs::parse_multiplexed_line;

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
    pub stderr_reader: Option<std::io::Lines<BufReader<std::process::ChildStderr>>>, // The stderr reader of the forkable process

    pub forked_processes: Arc<Mutex<HashMap<String, i32>>>, // Map of UUID to PID
    pub forked_names: Arc<Mutex<HashMap<String, String>>>,  // Map of UUID to name

    // These are pinged when the forked process finishes startup - either successful or failure
    pub fork_resolvers: Arc<Mutex<HashMap<String, AsyncResolve<ForkResult>>>>, // Map of UUID to fork resolver

    // These are pinged when the process completes execution
    pub completion_resolvers: Arc<Mutex<HashMap<String, AsyncResolve<ProcessResult>>>>, // Map of UUID to completion resolver

    pub monitor_thread: Option<JoinHandle<()>>, // Thread handle for monitoring output
    pub thread_terminate_tx: Arc<Mutex<Option<Sender<()>>>>, // Channel to signal thread termination
}

impl Layer {
    // New constructor for Layer with shared state
    pub fn new(
        child: Child,
        stdin: std::process::ChildStdin,
        reader: std::io::Lines<BufReader<std::process::ChildStdout>>,
        stderr_reader: std::io::Lines<BufReader<std::process::ChildStderr>>,
    ) -> Self {
        Self {
            child,
            stdin,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            forked_processes: Arc::new(Mutex::new(HashMap::new())),
            forked_names: Arc::new(Mutex::new(HashMap::new())),
            fork_resolvers: Arc::new(Mutex::new(HashMap::new())),
            completion_resolvers: Arc::new(Mutex::new(HashMap::new())),
            monitor_thread: None,
            thread_terminate_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Start monitoring threads that concurrently read from the child process stdout and stderr
    /// to avoid blocking if one stream has no content while the other does
    pub fn start_monitor_thread(&mut self) {
        // Create a channel for signaling thread termination
        let (terminate_tx, terminate_rx) = mpsc::channel();
        let (terminate_tx_stderr, terminate_rx_stderr) = mpsc::channel();
        {
            let mut tx_guard = self.thread_terminate_tx.lock().unwrap();
            *tx_guard = Some(terminate_tx.clone());
        }

        // Take ownership of the readers
        let stdout_reader = self.reader.take().expect("Reader should be available");
        let stderr_reader = self
            .stderr_reader
            .take()
            .expect("Stderr reader should be available");

        // Clone the shared resolver maps for the monitor thread
        let fork_resolvers = Arc::clone(&self.fork_resolvers);
        let completion_resolvers = Arc::clone(&self.completion_resolvers);
        let forked_processes = Arc::clone(&self.forked_processes);
        let forked_names = Arc::clone(&self.forked_names);

        // Clone for stderr thread
        let fork_resolvers_stderr = Arc::clone(&self.fork_resolvers);
        let completion_resolvers_stderr = Arc::clone(&self.completion_resolvers);
        let forked_processes_stderr = Arc::clone(&self.forked_processes);
        let forked_names_stderr = Arc::clone(&self.forked_names);

        // Start a separate thread for stderr to avoid blocking on stdout reads
        let stderr_thread = thread::spawn(move || {
            info!("Monitor thread for stderr started");
            let mut stderr_reader = stderr_reader;

            loop {
                // Check if we've been asked to terminate
                if terminate_rx_stderr.try_recv().is_ok() {
                    info!("Stderr monitor thread received terminate signal, breaking out of loop");
                    break;
                }

                // Try to read a line from stderr
                match stderr_reader.next() {
                    Some(Ok(line)) => {
                        trace!("Stderr monitor thread read line: {}", line);
                        Self::process_output_line(
                            &line,
                            &fork_resolvers_stderr,
                            &completion_resolvers_stderr,
                            &forked_processes_stderr,
                            &forked_names_stderr,
                        );
                    }
                    Some(Err(e)) => {
                        error!("Error reading from child process stderr: {}", e);
                        break;
                    }
                    None => {
                        // End of stream for stderr
                        info!("End of child process stderr stream detected, exiting stderr monitor thread");
                        break;
                    }
                }
            }

            info!("Stderr monitor thread exiting");
        });

        // Start the stdout monitor thread
        let thread_handle = thread::spawn(move || {
            info!("Monitor thread for stdout started");
            let mut stdout_reader = stdout_reader;

            loop {
                // Check if we've been asked to terminate
                if terminate_rx.try_recv().is_ok() {
                    info!("Monitor thread received terminate signal, breaking out of loop");
                    // Also terminate the stderr thread
                    let _ = terminate_tx_stderr.send(());
                    break;
                }

                // Try to read a line from stdout
                match stdout_reader.next() {
                    Some(Ok(line)) => {
                        trace!("Monitor thread read line from stdout: {}", line);
                        Self::process_output_line(
                            &line,
                            &fork_resolvers,
                            &completion_resolvers,
                            &forked_processes,
                            &forked_names,
                        );
                    }
                    Some(Err(e)) => {
                        error!("Error reading from child process stdout: {}", e);
                        // Also terminate the stderr thread
                        let _ = terminate_tx_stderr.send(());
                        break;
                    }
                    None => {
                        // End of stream for stdout
                        info!("End of child process stdout stream detected, breaking out of monitor loop");
                        // Also terminate the stderr thread
                        let _ = terminate_tx_stderr.send(());
                        break;
                    }
                }
            }

            // Wait for stderr thread to finish
            if let Err(e) = stderr_thread.join() {
                error!("Failed to join stderr thread: {:?}", e);
            } else {
                info!("Successfully joined stderr thread");
            }

            info!("Monitor thread exiting");
        });

        self.monitor_thread = Some(thread_handle);
    }

    /// Helper function to process an output line from either stdout or stderr
    fn process_output_line(
        line: &str,
        fork_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ForkResult>>>>,
        completion_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ProcessResult>>>>,
        forked_processes: &Arc<Mutex<HashMap<String, i32>>>,
        forked_names: &Arc<Mutex<HashMap<String, String>>>,
    ) {
        // All lines streamed from the forked process (even our own messages)
        // should be multiplexed lines
        match parse_multiplexed_line(line) {
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
                    // If we're resolved a UUID from the PID, we should also have a name
                    let forked_names_guard = forked_names.lock().unwrap();
                    let process_name = forked_names_guard.get(&uuid.clone());

                    match Self::handle_message(
                        &log_line.content,
                        Some(&uuid),
                        fork_resolvers,
                        completion_resolvers,
                        forked_processes,
                        forked_names,
                    ) {
                        Ok(_) => {
                            // Successfully handled the message, nothing more to do
                        }
                        Err(_e) => {
                            // Expected error condition in the case that we didn't receive a message
                            // but instead standard stdout
                            println!(
                                "[{}]: {}",
                                process_name
                                    .unwrap_or(&String::from("unknown"))
                                    .cyan()
                                    .bold(),
                                log_line.content
                            );
                        }
                    }
                } else {
                    // If we can't match it to a specific process, log it with PID
                    println!(
                        "Unmatched log: [{}] {}",
                        format!("{}:{}", log_line.pid, log_line.stream_name)
                            .cyan()
                            .bold(),
                        log_line.content
                    );
                }
            }
            Err(_e) => {
                // If parsing fails, treat the line as a raw message. We will log the contents
                // separately if we fail processing
                if let Err(_e) = Self::handle_message(
                    line,
                    None,
                    fork_resolvers,
                    completion_resolvers,
                    forked_processes,
                    forked_names,
                ) {
                    error!("Error handling log format: {}", line);
                }
            }
        }
    }

    /// Handle various messages from the child process
    fn handle_message(
        content: &str,
        uuid: Option<&String>,
        fork_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ForkResult>>>>,
        completion_resolvers: &Arc<Mutex<HashMap<String, AsyncResolve<ProcessResult>>>>,
        forked_processes: &Arc<Mutex<HashMap<String, i32>>>,
        forked_names: &Arc<Mutex<HashMap<String, String>>>,
    ) -> Result<(), String> {
        if let Ok(message) = serde_json::from_str::<Message>(content) {
            match message {
                Message::ForkResponse(response) => {
                    // Handle fork response and update the forked processes map
                    debug!("Monitor thread received fork response: {:?}", response);

                    // Store the PID in the forked processes map
                    let mut forked_processes_guard = forked_processes.lock().unwrap();
                    forked_processes_guard.insert(response.request_id.clone(), response.child_pid);
                    drop(forked_processes_guard);

                    // Store the process name in the forked names map
                    let mut forked_names_guard = forked_names.lock().unwrap();
                    forked_names_guard.insert(response.request_id.clone(), response.request_name);
                    drop(forked_names_guard);

                    // Resolve the fork status
                    let fork_resolvers_guard = fork_resolvers.lock().unwrap();
                    if let Some(resolver) = fork_resolvers_guard.get(&response.request_id) {
                        resolver
                            .resolve(ForkResult::Complete(Some(response.child_pid.to_string())));
                    } else {
                        error!("No resolver found for UUID: {}", response.request_id);
                    }
                    drop(fork_resolvers_guard);
                    Ok(())
                }
                Message::ChildComplete(complete) => {
                    trace!("Monitor thread received function result: {:?}", complete);

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");

                    // Resolve the completion
                    let completion_resolvers_guard = completion_resolvers.lock().unwrap();
                    if let Some(resolver) = completion_resolvers_guard.get(uuid) {
                        resolver.resolve(ProcessResult::Complete(complete.result.clone()));
                    } else {
                        error!("No resolver found for UUID: {}", uuid);
                    }
                    drop(completion_resolvers_guard);
                    Ok(())
                }
                Message::ChildError(error) => {
                    trace!("Monitor thread received error result: {:?}", error);

                    // We should always have a known UUID to receive this status, since it's issued
                    // from the child process
                    let uuid = uuid.expect("UUID should be known");

                    // Resolve the completion with an error, include both error message and traceback
                    let completion_resolvers_guard = completion_resolvers.lock().unwrap();
                    if let Some(resolver) = completion_resolvers_guard.get(uuid) {
                        // Create a complete error message with both the error text and traceback if available
                        let full_error = if let Some(traceback) = &error.traceback {
                            format!("{}\n\n{}", error.error, traceback)
                        } else {
                            error.error.clone()
                        };
                        resolver.resolve(ProcessResult::Error(full_error));
                    } else {
                        error!("No resolver found for UUID: {}", uuid);
                    }
                    drop(completion_resolvers_guard);
                    Ok(())
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
                    // For unknown errors, we don't have a UUID, so we can't resolve a specific promise
                    // Only log the error for now
                    error!("Monitor thread received unknown error: {}", error.error);
                    Ok(())
                }
                _ => {
                    // We should have a handler implemented for all messages types, capture the
                    // unknown ones
                    warn!("Monitor thread received unknown message type: {}", content);
                    Ok(())
                }
            }
        } else {
            // Not a message
            Err(format!(
                "Failed to parse message, received raw content: {}",
                content
            ))
        }
    }

    /// Stop the monitoring thread if it's running
    pub fn stop_monitor_thread(&mut self) {
        info!("Stopping monitor thread");

        // Send termination signal to the main monitor thread (which will relay to stderr thread)
        {
            let tx_guard = self.thread_terminate_tx.lock().unwrap();
            match &*tx_guard {
                Some(_) => info!("Termination sender exists - will attempt to send signal"),
                None => warn!(
                    "No termination sender found in the mutex - already taken or never created"
                ),
            }
        }

        if let Some(terminate_tx) = self.thread_terminate_tx.lock().unwrap().take() {
            info!("Acquired termination sender, sending terminate signal to monitor thread");
            if let Err(e) = terminate_tx.send(()) {
                // Avoid logging warning for expected error
                // If the channel is closed, it means the thread has already exited
                if e.to_string().contains("sending on a closed channel") {
                    info!("Monitor thread already exited (channel closed)");
                } else {
                    warn!("Failed to send terminate signal to monitor thread: {}", e);
                }
            } else {
                info!("Successfully sent termination signal to channel");
            }
        } else {
            warn!("No termination channel found - monitor thread might not be running or already being shut down");
        }

        // Wait for main monitor thread to complete (which also waits for stderr thread)
        match &self.monitor_thread {
            Some(_) => info!("Monitor thread handle exists - will attempt to join"),
            None => warn!("No monitor thread handle found - already taken or never created"),
        }

        if let Some(handle) = self.monitor_thread.take() {
            info!("Acquired thread handle, waiting for monitor thread to terminate");
            if let Err(e) = handle.join() {
                error!("Failed to join monitor thread: {:?}", e);
            } else {
                info!("Successfully joined monitor thread");
            }
        } else {
            warn!("No monitor thread handle found - already taken or never created");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::environment::Environment;
    use tempfile::TempDir;

    #[test]
    fn test_stderr_handling() -> Result<(), String> {
        // Import gag for capturing stdout in tests
        use gag::BufferRedirect;
        use std::io::Read;

        // Create a temporary directory for our test
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path().to_str().unwrap();

        // Create a Python script that writes to stderr
        let python_script = r#"
def function_with_stderr_output():
    # Write to stderr with a unique string we can look for
    import sys
    sys.stderr.write("UNIQUE_STDERR_OUTPUT_FOR_TESTING_12345\n")
    sys.stderr.flush()
    
    # Also write to stdout with a different unique string
    sys.stdout.write("UNIQUE_STDOUT_OUTPUT_FOR_TESTING_67890\n")
    sys.stdout.flush()
    
    # Return success
    return "Function executed successfully"

def main():
    return function_with_stderr_output()
        "#;

        // Prepare the script for isolation
        let (pickled_data, _python_env) =
            crate::harness::prepare_script_for_isolation(python_script, "main")?;

        // Create a buffer to redirect stdout for capturing the output
        let mut buf = BufferRedirect::stdout().unwrap();

        // Create and boot the Environment
        let mut runner = Environment::new("test_package", dir_path);
        runner.boot_main()?;

        // Execute the script in isolation
        let process_uuid = runner.exec_isolated(&pickled_data, "test_stderr_script")?;

        // Wait a moment for the process to execute and logs to be processed
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Communicate with the isolated process to get the result
        let result = runner.communicate_isolated(&process_uuid)?;

        // Clean up first to ensure all output is generated
        runner.stop_isolated(&process_uuid)?;

        // Verify we got the return value from the function
        assert_eq!(
            result,
            Some("Function executed successfully".to_string()),
            "Incorrect return value from isolated process"
        );

        // Get the captured output
        let mut output = String::new();
        buf.read_to_string(&mut output).unwrap();

        // Drop the buffer to restore stdout
        drop(buf);

        // This assertion should PASS because stdout is being properly captured
        assert!(
            output.contains("UNIQUE_STDOUT_OUTPUT_FOR_TESTING_67890"),
            "Expected to find stdout message in the captured output"
        );

        // This assertion should FAIL because stderr is not being properly captured
        // When stderr capture is properly implemented, this test will pass
        assert!(
            output.contains("UNIQUE_STDERR_OUTPUT_FOR_TESTING_12345"),
            "Failed to find stderr message in the captured output - stderr is not being properly captured"
        );

        Ok(())
    }
}
