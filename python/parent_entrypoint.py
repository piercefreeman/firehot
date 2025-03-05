"""
This is the main entrypoint for our continuously running parent process. It will receive commands
from Rust and execute them.

"""
import sys
import os
from traceback import format_exc
from time import sleep
from dataclasses import dataclass, asdict
from json import loads as json_loads, dumps as json_dumps
from enum import StrEnum
from json.decoder import JSONDecodeError

#
# Messages
#

class MessageType(StrEnum):
    FORK_REQUEST = "FORK_REQUEST"
    FORK_RESPONSE = "FORK_RESPONSE"
    CHILD_COMPLETE = "CHILD_COMPLETE"
    CHILD_ERROR = "CHILD_ERROR"
    UNKNOWN_COMMAND = "UNKNOWN_COMMAND"
    UNKNOWN_ERROR = "UNKNOWN_ERROR"
    IMPORT_ERROR = "IMPORT_ERROR"
    IMPORT_COMPLETE = "IMPORT_COMPLETE"
    EXIT_REQUEST = "EXIT_REQUEST"

class MessageBase:
    name: MessageType

# Requests

@dataclass
class ForkRequest(MessageBase):
    name: MessageType = MessageType.FORK_REQUEST
    code: str

@dataclass
class ExitRequest(MessageBase):
    name: MessageType = MessageType.EXIT_REQUEST

# Responses

@dataclass
class ForkResponse(MessageBase):
    name: MessageType = MessageType.FORK_RESPONSE
    child_pid: int

@dataclass
class ChildComplete(MessageBase):
    name: MessageType = MessageType.CHILD_COMPLETE
    result: str | None

@dataclass
class ChildError(MessageBase):
    name: MessageType = MessageType.CHILD_ERROR
    error: str

@dataclass
class UnknownCommandError(MessageBase):
    name: MessageType = MessageType.UNKNOWN_COMMAND
    command: str

@dataclass
class UnknownError(MessageBase):
    name: MessageType = MessageType.UNKNOWN_ERROR
    error: str
    traceback: str | None

@dataclass
class ImportError(MessageBase):
    name: MessageType = MessageType.IMPORT_ERROR
    error: str
    traceback: str | None

@dataclass
class ImportComplete(MessageBase):
    name: MessageType = MessageType.IMPORT_COMPLETE

MESSAGES = {
    MessageType.FORK_REQUEST: ForkRequest,
    MessageType.FORK_RESPONSE: ForkResponse,
    MessageType.CHILD_COMPLETE: ChildComplete,
    MessageType.CHILD_ERROR: ChildError,
    MessageType.UNKNOWN_COMMAND: UnknownCommandError,
    MessageType.UNKNOWN_ERROR: UnknownError,
    MessageType.IMPORT_ERROR: ImportError,
    MessageType.IMPORT_COMPLETE: ImportComplete,
    MessageType.EXIT_REQUEST: ExitRequest,
}

def write_message(message: MessageBase):
    print(f"{message.name}:{json_dumps(asdict(message))}", flush=True)

def read_message() -> MessageBase | None:
    line = sys.stdin.readline().strip()
    if not line:
        return None

    payload = line.split(":", 1)
    if len(payload) != 2:
        print(f"ERROR: Invalid message format", flush=True)
        return None

    name, data = payload

    try:
        message_type = MessageType(name)
        return MESSAGES[message_type](**json_loads(data))
    except (ValueError, JSONDecodeError) as e:
        # We expect these errors since they are likely due to another message (not intended for us)
        # that has a colon in it
        print(f"ERROR: {str(e)}", flush=True)
        return None

#
# Main Logic
#

def main():
    # This will be populated with dynamic import statements from Rust
    dynamic_imports = sys.argv[1] if len(sys.argv) > 1 else ""
    
    # Execute the dynamic imports
    try:
        if dynamic_imports:
            exec(dynamic_imports)
    except Exception as e:
        write_message(ImportError(error=str(e), traceback=format_exc()))
        sys.exit(1)

    # Signal that imports are complete
    write_message(ImportComplete())

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
                
                # By convention, the result is stored in the 'result' variable
                if 'result' in exec_locals:
                    write_message(ChildComplete(result=str(exec_locals['result'])))
                else:
                    write_message(ChildComplete(result=None))
            except Exception as e:
                write_message(ChildError(error=str(e), traceback=format_exc()))
            finally:
                # Exit the child process
                sys.exit(0)
        else:
            # Parent process. The PID will represent the child process.
            return pid

    # Main loop - wait for commands on stdin
    while True:
        try:
            command = read_message()
            if not command:
                sleep(0.1)
                continue
            
            if isinstance(command, ForkRequest):
                fork_pid = handle_fork_request(command.code)
                write_message(ForkResponse(child_pid=fork_pid))
            elif isinstance(command, ExitRequest):
                print("Exiting loader process", flush=True)
                break
            else:
                write_message(UnknownCommandError(command=str(command)))
        except Exception as e:
            write_message(UnknownError(error=str(e), traceback=format_exc()))

if __name__ == "__main__":
    main()
