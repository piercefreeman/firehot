from dataclasses import dataclass
from typing import Any, Callable
from uuid import UUID

from firehot.firehot import (
    communicate_isolated as communicate_isolated_rs,
)
from firehot.firehot import (
    exec_isolated as exec_isolated_rs,
)
from firehot.firehot import (
    stop_isolated as stop_isolated_rs,
)
from firehot.firehot import (
    update_environment as update_environment_rs,
)
from firehot.naming import NAME_REGISTRY


@dataclass
class IsolatedProcess:
    process_uuid: UUID
    process_name: str


class Environment:
    """
    A class that represents an isolated Python environment for executing code. At any one
    point in time, this environment will have a single built "layer" which shares the imported
    3rd party dependencies. When new code is executed, it is forked from this layer to share
    these dependencies.

    """

    def __init__(self, runner_id: str):
        """
        Initialize the Environment with a runner ID.

        :param runner_id: The unique identifier for this runner
        """
        self.runner_id = runner_id

    def exec(self, func: Callable, *args: Any, name: str | None = None) -> IsolatedProcess:
        """
        Execute a function in the isolated environment.

        :param func: The function to execute. A function should fully contain its content, including imports
        :param args: Arguments to pass to the function
        :param name: Optional name for the process
        :returns: An IsolatedProcess instance representing the execution
        """
        process_name = name or NAME_REGISTRY.reserve_random_name()
        exec_id = UUID(exec_isolated_rs(self.runner_id, process_name, func, args))
        return IsolatedProcess(process_uuid=exec_id, process_name=process_name)

    def stop_isolated(self, isolate: IsolatedProcess):
        """
        Stop an isolated process, terminating its execution.

        This method attempts to gracefully terminate the isolated process first with SIGTERM,
        then with SIGKILL if necessary. It also cleans up all resources associated with the process.

        :param isolate: The IsolatedProcess instance to stop
        :returns: True if the process was successfully stopped or if it had already completed,
                 False if the process did not exist

        .. note::
           It's good practice to stop isolated processes when they are no longer needed
           to free up system resources.
        """
        stop_isolated_rs(self.runner_id, str(isolate.process_uuid))

    def communicate_isolated(self, isolate: IsolatedProcess) -> str:
        """
        Communicate with an isolated process to get its output.

        :param isolate: Either an IsolatedProcess instance or a UUID object
        :returns: The output from the isolated process
        """
        # Handle both IsolatedProcess objects and raw UUIDs
        return communicate_isolated_rs(self.runner_id, str(isolate.process_uuid))

    def update_environment(self):
        """
        Update the environment by checking for import changes and restarting if necessary.
        """
        return update_environment_rs(self.runner_id)
