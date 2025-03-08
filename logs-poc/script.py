#!/usr/bin/env python3
import os
import sys
import time

def write_to_custom_fd() -> None:
    """Write a message to the specified file descriptor."""
    print("Regular stdout message that shouldn't be captured by Rust")
    
    # In this case, the FD 3 is already set up for us by Rust
    # We just need to create a file object from it
    with os.fdopen(3, "w") as f:
        f.write("Hello from Python via custom file descriptor!\n")
        f.flush()  # Make sure it's written immediately
    
    # Add a short delay to ensure message is sent before process exits
    time.sleep(0.1)

if __name__ == "__main__":
    write_to_custom_fd() 