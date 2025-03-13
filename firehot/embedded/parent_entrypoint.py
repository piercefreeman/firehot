"""
This is the main entrypoint for our continuously running parent process. It will receive commands
from Rust and execute them.

Intended for embeddable usage in Rust, can only import stdlib modules.

"""

import errno
import fcntl
import logging
import os
import select
import sys
import threading
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from json import dumps as json_dumps
from json import loads as json_loads
from json.decoder import JSONDecodeError
from os import getenv
from sys import _current_frames
from time import sleep
from traceback import format_exc, format_stack

from firehot.firehot import get_total_thread_count

try:
    from enum import StrEnum
except ImportError:
    from enum import Enum

    class StrEnum(str, Enum):
        """Polyfill for StrEnum from Python 3.11."""

        pass

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
    request_id: str
    code: str
    request_name: str
    name: MessageType = MessageType.FORK_REQUEST


@dataclass
class ExitRequest(MessageBase):
    name: MessageType = MessageType.EXIT_REQUEST


# Responses


@dataclass
class ForkResponse(MessageBase):
    request_id: str
    request_name: str

    child_pid: int

    name: MessageType = MessageType.FORK_RESPONSE


@dataclass
class ChildComplete(MessageBase):
    result: str | None

    name: MessageType = MessageType.CHILD_COMPLETE


@dataclass
class ChildError(MessageBase):
    error: str
    traceback: str | None

    name: MessageType = MessageType.CHILD_ERROR


@dataclass
class UnknownCommandError(MessageBase):
    command: str

    name: MessageType = MessageType.UNKNOWN_COMMAND


@dataclass
class UnknownError(MessageBase):
    error: str
    traceback: str | None

    name: MessageType = MessageType.UNKNOWN_ERROR


@dataclass
class ImportError(MessageBase):
    error: str
    traceback: str | None

    name: MessageType = MessageType.IMPORT_ERROR


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
    sys.stdout.write(f"{json_dumps(asdict(message))}\n")
    sys.stdout.flush()


def read_message() -> MessageBase | None:
    line = sys.stdin.readline().strip()
    if not line:
        return None

    # Try to parse it as a JSON object
    try:
        payload = json_loads(line)
    except JSONDecodeError:
        return None

    # Get the name from the payload
    name = payload.get("name")
    if not name:
        return None

    try:
        message_type = MessageType(name)
    except ValueError:
        return None

    return MESSAGES[message_type](**payload)


#
# Logging
#

DEFAULT_DESCRIPTORS = {
    "stdout": 1,
    "stderr": 2,
}


class MultiplexedStream:
    """
    Redirects a file descriptor (stdout/stderr) to capture all output,
    including output from the logging module that bypasses sys.stdout/stderr.
    """

    _instances: dict[str, "MultiplexedStream"] = {}

    def __init__(self, stream_name: str):
        self.stream_name = stream_name
        self.pid = os.getpid()
        self.original_fd = DEFAULT_DESCRIPTORS[stream_name]
        self.read_fd = None
        self.write_fd = None
        self.original_fd_dup = None
        self.monitor_thread = None
        self.active = False
        self._instances[stream_name] = self

    def start_redirection(self) -> None:
        """Set up file descriptor redirection and monitoring thread."""
        if self.active:
            return

        # Create a pipe
        self.read_fd, self.write_fd = os.pipe()

        # Save original file descriptor
        self.original_fd_dup = os.dup(self.original_fd)

        # Replace the file descriptor
        os.dup2(self.write_fd, self.original_fd)

        # Set pipe to non-blocking mode
        flags = fcntl.fcntl(self.read_fd, fcntl.F_GETFL)
        fcntl.fcntl(self.read_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)

        # Start monitoring thread
        self.active = True
        self.monitor_thread = threading.Thread(target=self._monitor_pipe, daemon=True)
        self.monitor_thread.start()

        # This ensures that the file object used for sys.stdout (or sys.stderr) does
        # not own (and eventually close) the original descriptor. We want to be in
        # charge of that closure process.
        if self.stream_name == "stdout":
            sys.stdout = os.fdopen(os.dup(self.original_fd), "w", 1)  # line buffered
        else:
            sys.stderr = os.fdopen(os.dup(self.original_fd), "w", 1)  # line buffered

    def _monitor_pipe(self) -> None:
        """Monitor the pipe and format output with PID prefix."""
        try:
            while self.active:
                # Wait for data with a timeout
                ready, _, _ = select.select([self.read_fd], [], [], 0.1)
                if not ready:
                    continue

                try:
                    data = os.read(self.read_fd, 4096)
                    if not data:  # EOF
                        break

                    # Format the data with PID and stream name
                    formatted_data = b""
                    for line in data.splitlines(True):  # Keep line endings
                        if line.strip():  # Skip empty lines
                            prefix = f"[PID:{self.pid}:{self.stream_name}]".encode()
                            formatted_data += prefix + line

                    # Write the formatted data to the original descriptor
                    os.write(self.original_fd_dup, formatted_data)
                except (IOError, OSError) as e:
                    if e.errno != errno.EAGAIN:  # Not just a would-block error
                        # Write error to original stdout for debugging
                        error_msg = f"[ERROR] MultiplexedStream exception: {str(e)}\n".encode()
                        os.write(self.original_fd_dup, error_msg)
        finally:
            # Only close the read_fd here; write_fd will be closed in stop_redirection
            if self.read_fd is not None:
                os.close(self.read_fd)
                self.read_fd = None

    def stop_redirection(self) -> None:
        """Stop redirection and restore original file descriptors."""
        if not self.active:
            return

        self.active = False

        # Wait for monitor thread to finish
        if self.monitor_thread and self.monitor_thread.is_alive():
            self.monitor_thread.join(timeout=1.0)

        # Restore original file descriptor
        if self.original_fd_dup is not None:
            os.dup2(self.original_fd_dup, self.original_fd)
            os.close(self.original_fd_dup)
            self.original_fd_dup = None

        # Close write fd
        if self.write_fd is not None:
            os.close(self.write_fd)
            self.write_fd = None

        # Restore Python stdout/stderr
        if self.stream_name == "stdout":
            sys.stdout = sys.__stdout__
        else:
            sys.stderr = sys.__stderr__

    def __del__(self) -> None:
        """Ensure resources are cleaned up."""
        self.stop_redirection()

    @classmethod
    @contextmanager
    def setup_stream_redirection(cls):
        """Set up redirection for both stdout and stderr."""
        stdout_stream = cls("stdout")
        stderr_stream = cls("stderr")
        stdout_stream.start_redirection()
        stderr_stream.start_redirection()

        try:
            yield stdout_stream, stderr_stream
        finally:
            for instance in cls._instances.values():
                instance.stop_redirection()


def build_firehot_logger():
    # This will be populated with dynamic import statements from Rust
    known_log_levels = {
        "TRACE": logging.DEBUG,
        "DEBUG": logging.DEBUG,
        "INFO": logging.INFO,
        "WARNING": logging.WARNING,
        "ERROR": logging.ERROR,
    }
    log_level = known_log_levels.get(getenv("FIREHOT_LOG_LEVEL", "WARNING"), logging.WARNING)

    logger = logging.getLogger("firehot")
    logger.setLevel(log_level)

    return logger


def check_thread_safety() -> None:
    """
    Check if we're running with multiple threads and warn about potential fork() dangers.
    This is especially important on macOS where fork() behavior with multiple threads
    can lead to deadlocks and other issues.

    More reading:
    https://stackoverflow.com/questions/50168647/multiprocessing-causes-python-to-crash-and-gives-an-error-may-have-been-in-progr
    https://github.com/ansible/ansible/issues/32499
    https://blog.phusion.nl/2017/10/13/why-ruby-app-servers-break-on-macos-high-sierra-and-what-can-be-done-about-it/

    """
    firehot_logger = build_firehot_logger()

    # Get the total thread count from our Rust implementation
    # This will catch ALL threads, including those spawned by C extensions
    total_thread_count = get_total_thread_count()

    if total_thread_count == 1:
        return

    firehot_logger.warning(
        f"WARNING: Detected {total_thread_count} active threads before fork() "
        f"({threading.active_count()} Python threads, {total_thread_count - threading.active_count()} C/native threads). "
        "Forking a process with multiple threads can lead to deadlocks and "
        "memory corruption, especially on macOS. Any threads besides the main "
        "thread will be terminated in the child process, potentially leaving "
        "shared resources in an inconsistent state."
    )

    # Get stack traces for all Python threads
    thread_frames = _current_frames()

    # Log details about each Python thread
    for thread in threading.enumerate():
        thread_id = thread.ident
        if thread_id in thread_frames:
            stack = "".join(format_stack(thread_frames[thread_id]))
            firehot_logger.warning(
                f"\nPython Thread Details:\n"
                f"  Name: {thread.name}\n"
                f"  ID: {thread_id}\n"
                f"  Daemon: {thread.daemon}\n"
                f"  Alive: {thread.is_alive()}\n"
                f"  Stack Trace:\n{stack.rstrip()}"
            )
        else:
            firehot_logger.warning(
                f"\nPython Thread Details:\n"
                f"  Name: {thread.name}\n"
                f"  ID: {thread_id}\n"
                f"  Daemon: {thread.daemon}\n"
                f"  Alive: {thread.is_alive()}\n"
                f"  Stack Trace: Unable to retrieve"
            )

    if total_thread_count > threading.active_count():
        firehot_logger.warning(
            f"\nWARNING: Detected {total_thread_count - threading.active_count()} "
            "threads that were created outside of Python (likely from C extensions). "
            "These threads cannot be inspected from Python but may still cause "
            "issues during fork()."
        )


#
# Main Logic
#


def main():
    dynamic_imports = sys.argv[1] if len(sys.argv) > 1 else ""
    firehot_logger = build_firehot_logger()

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
        # Check thread safety before forking
        check_thread_safety()

        pid = os.fork()
        if pid == 0:
            # Child process

            # Set up stream redirection to catch all output from the child process
            # NOTE: We can't run this before the child process has launched, since it spawns
            # a thread that will affect our fork() behavior.
            with MultiplexedStream.setup_stream_redirection():
                try:
                    # Set up globals and locals for execution
                    exec_globals = globals().copy()
                    exec_locals = {}

                    firehot_logger.info("Will execute code in forked process...")
                    sys.stdout.flush()

                    # Execute the code
                    exec(code_to_execute, exec_globals, exec_locals)

                    firehot_logger.info("Executed code in forked process")
                    sys.stdout.flush()

                    # By convention, the result is stored in the 'result' variable
                    if "result" in exec_locals:
                        write_message(ChildComplete(result=str(exec_locals["result"])))
                    else:
                        write_message(ChildComplete(result=None))

                    sys.exit(0)
                except Exception as e:
                    # Report the error
                    write_message(ChildError(error=str(e), traceback=format_exc()))
                    sys.exit(1)
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
                write_message(
                    ForkResponse(
                        request_id=command.request_id,
                        request_name=command.request_name,
                        child_pid=fork_pid,
                    )
                )
            elif isinstance(command, ExitRequest):
                firehot_logger.info("Exiting loader process")
                sys.stdout.flush()
                break
            else:
                write_message(UnknownCommandError(command=str(command)))
        except Exception as e:
            write_message(UnknownError(error=str(e), traceback=format_exc()))


if __name__ == "__main__":
    main()
