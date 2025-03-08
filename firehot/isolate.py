from typing import Any, Callable
from uuid import UUID

from firehot.firehot import (
    communicate_isolated as communicate_isolated_rs,
)
from firehot.firehot import (
    exec_isolated as exec_isolated_rs,
)
from firehot.firehot import (
    update_environment as update_environment_rs,
)
from firehot.naming import NAME_REGISTRY


class ImportRunner:
    """
    A class that represents an isolated Python environment for executing code.
    """

    def __init__(self, runner_id: str):
        """
        Initialize the ImportRunner with a runner ID.

        :param runner_id: The unique identifier for this runner

        """
        self.runner_id = runner_id

    def exec(self, func: Callable, *args: Any, name: str | None = None) -> UUID:
        """
        Execute a function in the isolated environment.

        :param func: The function to execute. A function should fully contain its content, including imports.
        :param *args: Arguments to pass to the function

        :returns: The result of the function execution

        """
        name = name or NAME_REGISTRY.reserve_random_name()
        return UUID(exec_isolated_rs(self.runner_id, func, args if args else None, name))

    def communicate_isolated(self, process_uuid: UUID) -> str:
        """
        Communicate with an isolated process to get its output
        """
        return communicate_isolated_rs(self.runner_id, str(process_uuid))

    def update_environment(self):
        """
        Update the environment by checking for import changes and restarting if necessary
        """
        return update_environment_rs(self.runner_id)
