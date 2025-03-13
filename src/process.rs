use std::io;

#[cfg(target_os = "macos")]
use libc::{c_int, proc_pidinfo, PROC_PIDTASKINFO};

#[cfg(target_os = "linux")]
use std::fs;

/// Get the total number of threads for the current process, including threads spawned
/// by C-level extensions. This is more comprehensive than Python's threading.active_count()
/// as it uses OS-level APIs to get the true thread count.
///
/// # Returns
/// * `Result<u32, io::Error>` - The number of threads or an error if the thread count
///   could not be determined
pub fn get_total_thread_count() -> Result<u32, io::Error> {
    #[cfg(target_os = "macos")]
    {
        unsafe {
            // Define the task info structure
            #[repr(C)]
            struct ProcTaskInfo {
                pti_virtual_size: u64,      // virtual memory size (bytes)
                pti_resident_size: u64,     // resident memory size (bytes)
                pti_total_user: u64,        // total time (user)
                pti_total_system: u64,      // total time (system)
                pti_threads_user: u64,      // total time (user) for threads
                pti_threads_system: u64,    // total time (system) for threads
                pti_policy: c_int,          // default policy
                pti_faults: i32,            // number of page faults
                pti_pageins: i32,           // number of actual pageins
                pti_cow_faults: i32,        // number of copy-on-write faults
                pti_messages_sent: i32,     // number of messages sent
                pti_messages_received: i32, // number of messages received
                pti_syscalls_mach: i32,     // number of mach system calls
                pti_syscalls_unix: i32,     // number of unix system calls
                pti_csw: i32,               // number of context switches
                pti_threadnum: i32,         // number of threads
                pti_numrunning: i32,        // number of running threads
                pti_priority: i32,          // task priority
            }

            let mut task_info: ProcTaskInfo = std::mem::zeroed();
            let pid = libc::getpid();

            let result = proc_pidinfo(
                pid,
                PROC_PIDTASKINFO,
                0,
                &mut task_info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<ProcTaskInfo>() as i32,
            );

            if result <= 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(task_info.pti_threadnum as u32)
        }
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux, we can read the number of threads from /proc/self/status
        let status = fs::read_to_string("/proc/self/status")?;

        // Find the Threads line
        for line in status.lines() {
            if line.starts_with("Threads:") {
                // Parse the number after "Threads:"
                if let Some(count_str) = line.split_whitespace().nth(1) {
                    if let Ok(count) = count_str.parse::<u32>() {
                        return Ok(count);
                    }
                }
                break;
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not find thread count in /proc/self/status",
        ))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Thread counting is only supported on Linux and macOS",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_thread_count() {
        // Get initial thread count (should be at least 1)
        let initial_count = get_total_thread_count().unwrap();
        assert!(initial_count >= 1);

        // Spawn some threads
        let handles: Vec<_> = (0..3)
            .map(|_| {
                thread::spawn(|| {
                    // Sleep to ensure we can count it
                    thread::sleep(std::time::Duration::from_millis(500));
                })
            })
            .collect();

        // Sleep briefly to ensure threads are running
        thread::sleep(std::time::Duration::from_millis(50));

        // Get new thread count
        let new_count = get_total_thread_count().unwrap();

        // Should have at least 3 more threads than initial
        assert!(new_count >= initial_count + 3);

        // Clean up threads
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
