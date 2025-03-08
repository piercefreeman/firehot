use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::os::unix::io::FromRawFd;
use std::process::{Command, Stdio};

fn main() -> io::Result<()> {
    // Create a pipe
    let (mut reader, writer) = os_pipe::pipe()?;
    
    println!("Created pipe with reader_fd={}, writer_fd={}", reader.as_raw_fd(), writer.as_raw_fd());
    
    // Spawn the Python process using pre_exec to set up file descriptors
    let mut python_process = Command::new("python3");
    
    // We need to ensure the writer fd is set to fd 3 in the child process
    let writer_fd = writer.as_raw_fd();
    unsafe {
        python_process.pre_exec(move || {
            // Move fd to 3 (which is the fd we use in Python)
            if libc::dup2(writer_fd, 3) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    
    // Execute the Python script
    let child = python_process
        .arg("script.py")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    
    // We can drop the writer now
    drop(writer);
    
    // Read from the pipe
    let mut buffer = String::new();
    reader.read_to_string(&mut buffer)?;
    
    // Wait for the Python process to complete
    let status = child.wait_with_output()?;
    println!("Python process exited with status: {}", status.status);
    
    // Display what we read from the pipe
    println!("Received from Python via pipe: {}", buffer);
    
    Ok(())
}
