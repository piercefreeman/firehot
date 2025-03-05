"""
This is the main entrypoint for our continuously running parent process.

"""
import time
import sys
import os
import json
import traceback
import pickle
import base64
from dataclasses import dataclass, asdict
from json import loads, dumps
from enum import StrEnum

class MessageType(StrEnum):
    FORK_REQUEST = "FORK_REQUEST"
    FORK_RESPONSE = "FORK_RESPONSE"
    CHILD_COMPLETE = "CHILD_COMPLETE"
    CHILD_ERROR = "CHILD_ERROR"
    UNKNOWN_COMMAND = "UNKNOWN_COMMAND"
    IMPORT_ERROR = "IMPORT_ERROR"
    IMPORT_COMPLETE = "IMPORT_COMPLETE"

class MessageBase:
    name: MessageType

@dataclass
class ForkRequest(MessageBase):
    name: str = "FORK_REQUEST"
    code: str

@dataclass
class ForkResponse(MessageBase):
    name: str = "ForkResponse"
    child_pid: int

@dataclass
class ChildComplete(MessageBase):
    name: str = "ChildComplete"
    result: str

@dataclass
class ChildError(MessageBase):
    name: str = "ChildError"
    error: str

@dataclass
class UnknownCommandError(MessageBase):
    error: str
    traceback: str | None

@dataclass
class ImportError(MessageBase):
    error: str
    traceback: str | None

@dataclass
class ImportComplete(MessageBase):
    pass

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
                # Set up globals and locals for execution
                exec_globals = globals().copy()
                exec_locals = {}
                
                # Execute the code
                exec(code_to_execute, exec_globals, exec_locals)
                
                # If we have a result, print it
                if 'result' in exec_locals:
                    result_msg = {"result": str(exec_locals['result'])}
                    print(f"FORK_RUN_COMPLETE:{json.dumps(result_msg)}", flush=True)
                else:
                    print(f"FORK_RUN_COMPLETE:{{}}", flush=True)
            except Exception as e:
                error_msg = {"error": str(e), "traceback": traceback.format_exc()}
                print(f"FORK_RUN_ERROR:{json.dumps(error_msg)}", flush=True)
            finally:
                # Exit the child process
                sys.exit(0)
        else:
            # Parent process. The PID will represent the child process.
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
                print("Exiting loader process", flush=True)
                break
            else:
                print(f"UNKNOWN_COMMAND:{command}", flush=True)
        except Exception as e:
            error_msg = {"error": str(e), "traceback": traceback.format_exc()}
            print(f"ERROR:{json.dumps(error_msg)}", flush=True)

if __name__ == "__main__":
    main() 