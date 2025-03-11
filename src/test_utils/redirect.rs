// redirect_logs.rs
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// A struct that redirects stdout at the file descriptor level.
/// It creates a pipe and then replaces STDOUT_FILENO with the pipe's write end.
/// A background thread reads from the pipe, writes data to the original stdout,
/// and saves the captured output in a buffer.
pub struct RedirectLogs {
    /// A duplicate of the original stdout file descriptor.
    original_stdout_fd: RawFd,
    /// Captured output stored in an Arc<Mutex<...>> for thread-safe access.
    captured: Arc<Mutex<Vec<u8>>>,
    /// Handle to the background thread reading from the pipe.
    thread_handle: Option<JoinHandle<()>>,
}

impl RedirectLogs {
    /// Creates a new RedirectLogs that redirects stdout.
    pub fn new() -> io::Result<Self> {
        unsafe {
            // Create a pipe: fds[0] for reading, fds[1] for writing.
            let mut fds = [0; 2];
            if libc::pipe(fds.as_mut_ptr()) < 0 {
                return Err(io::Error::last_os_error());
            }
            let read_fd = fds[0];
            let write_fd = fds[1];

            // Duplicate the current STDOUT_FILENO so we can later restore it.
            let original_stdout = libc::dup(libc::STDOUT_FILENO);
            if original_stdout < 0 {
                return Err(io::Error::last_os_error());
            }

            // Redirect STDOUT_FILENO to the pipe's write end.
            if libc::dup2(write_fd, libc::STDOUT_FILENO) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Now that STDOUT_FILENO refers to the pipe's write end, close the original write_fd.
            libc::close(write_fd);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let captured_thread = Arc::clone(&captured);

            // Duplicate original_stdout for use in the background thread.
            let orig_stdout_for_thread = libc::dup(original_stdout);
            if orig_stdout_for_thread < 0 {
                return Err(io::Error::last_os_error());
            }

            // Spawn a thread to read from the pipe.
            let thread_handle = thread::spawn(move || {
                // Wrap the read end of the pipe in a File so we can use standard I/O.
                let mut pipe_reader = File::from_raw_fd(read_fd);
                let mut buffer = [0u8; 1024];
                loop {
                    match pipe_reader.read(&mut buffer) {
                        Ok(0) => break, // EOF reached.
                        Ok(n) => {
                            // Write the read bytes to the original stdout.
                            let _ = libc::write(
                                orig_stdout_for_thread,
                                buffer.as_ptr() as *const libc::c_void,
                                n,
                            );
                            // Also, save the output to our captured buffer.
                            if let Ok(mut cap) = captured_thread.lock() {
                                cap.extend_from_slice(&buffer[..n]);
                            }
                        }
                        Err(_) => break, // On error, exit the loop.
                    }
                }
                // Close the duplicated original stdout for the thread.
                libc::close(orig_stdout_for_thread);
            });

            Ok(RedirectLogs {
                original_stdout_fd: original_stdout,
                captured,
                thread_handle: Some(thread_handle),
            })
        }
    }

    /// Returns the captured output as a vector of bytes.
    /// (May not be complete until redirection is finalized.)
    pub fn get_captured(&self) -> Vec<u8> {
        self.captured.lock().unwrap().clone()
    }

    /// Finalizes the redirection: flushes stdout, restores the original stdout
    /// (thereby closing the pipe's write end so the reader thread can finish),
    /// and joins the background thread. Returns the complete captured output.
    pub fn finish(mut self) -> io::Result<Vec<u8>> {
        // Flush stdout to ensure all output is written.
        io::stdout().flush()?;
        unsafe {
            // Restore the original stdout by duplicating our saved descriptor back to STDOUT_FILENO.
            if libc::dup2(self.original_stdout_fd, libc::STDOUT_FILENO) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::close(self.original_stdout_fd);
        }
        // Wait for the background thread to finish.
        if let Some(handle) = self.thread_handle.take() {
            handle.join().expect("Failed to join redirect thread");
        }
        // Return the captured output.
        Ok(self.get_captured())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_redirect_logs() {
        // Create the redirection.
        let redirection = RedirectLogs::new().expect("Failed to redirect stdout");

        // Write something to stdout.
        println!("Hello from redirect_logs!");

        // Finalize the redirection to ensure all output is captured.
        let output = redirection.finish().expect("Failed to finish redirection");
        let output_str = String::from_utf8(output).expect("Invalid UTF-8");

        // Now check the captured output.
        assert!(output_str.contains("Hello from redirect_logs!"));
    }
}
