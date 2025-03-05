#!/usr/bin/env python3

import time
import sys
import os
import json
import traceback

def main():
    # This will be populated with dynamic import statements from Rust
    dynamic_imports = sys.argv[1] if len(sys.argv) > 1 else ""
    
    # Execute the dynamic imports
    try:
        if dynamic_imports:
            exec(dynamic_imports)
    except Exception as e:
        print(f"IMPORT_ERROR: {str(e)}", flush=True)
        sys.exit(1)

    # Signal that imports are complete
    print("IMPORTS_LOADED", flush=True)

    # Function to handle forking and executing code
    def handle_fork_request(code_to_execute):
        pid = os.fork()
        if pid == 0:
            # Child process
            try:
                # Execute the code
                exec(code_to_execute)
                print(f"FORK_COMPLETE:{{}}", flush=True)
            except Exception as e:
                error_msg = {{"error": str(e), "traceback": traceback.format_exc()}}
                print(f"FORK_ERROR:{json.dumps(error_msg)}", flush=True)
            finally:
                # Exit the child process
                sys.exit(0)
        else:
            # Parent process
            return pid

    # Main loop - wait for commands on stdin
    while True:
        try:
            command = sys.stdin.readline().strip()
            if not command:
                time.sleep(0.1)
                continue
                
            if command.startswith("FORK:"):
                # Extract code to execute
                code_to_execute = command[5:]
                fork_pid = handle_fork_request(code_to_execute)
                print(f"FORKED:{fork_pid}", flush=True)
            elif command == "EXIT":
                break
            else:
                print(f"UNKNOWN_COMMAND:{command}", flush=True)
        except Exception as e:
            error_msg = {{"error": str(e), "traceback": traceback.format_exc()}}
            print(f"ERROR:{json.dumps(error_msg)}", flush=True)

if __name__ == "__main__":
    main() 