import time

import pytest

from firehot.context import isolate_imports
from firehot.environment import Environment


@pytest.fixture
def import_runner(sample_package):
    with isolate_imports(sample_package) as runner:
        yield runner


def function_with_exception():
    raise ValueError("This is a deliberate test exception")


def function_with_success(name):
    return f"Hello, {name}!"


def test_successful_execution(import_runner: Environment):
    """Test that we can successfully execute a function in isolation."""

    # Execute the function in isolation
    process = import_runner.exec(function_with_success, "World")

    # Give it a moment to complete
    time.sleep(0.1)

    # Get the result
    result = import_runner.communicate_isolated(process)

    # Verify the result
    assert result == "Hello, World!"


def test_exception_in_child_process(import_runner: Environment):
    """Test that exceptions in child processes are properly handled."""

    # Execute the function in isolation
    process = import_runner.exec(function_with_exception)

    # Give it a moment to fail
    time.sleep(0.1)

    # Try to get the result, which should raise an exception
    with pytest.raises(Exception) as excinfo:
        import_runner.communicate_isolated(process)

    # Verify the exception contains our error message
    assert "This is a deliberate test exception" in str(excinfo.value)


def test_stop_isolated(import_runner: Environment):
    """Test that we can stop an isolated process."""

    # Create a long-running function for testing stop
    def long_running_function():
        import time

        time.sleep(10)
        return "This should never be returned!"

    # Execute the function in isolation
    process = import_runner.exec(long_running_function)

    # Stop the process before it completes
    import_runner.stop_isolated(process)

    # Verify that communicating with the stopped process raises an exception
    with pytest.raises(RuntimeError):
        import_runner.communicate_isolated(process)
