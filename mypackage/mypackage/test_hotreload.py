#!/usr/bin/env python3
"""
Example demonstrating the isolate_imports context manager with multiple concurrent runners.

This shows how to:
1. Isolate imports for a specific package path
2. Execute functions in a forked process
3. Use multiple concurrent import runners

Run this with:
    python mypackage/test_hotreload.py
"""

import time

from firehot import isolate_imports


# A global function to demonstrate execution in a forked process
def global_fn(msg: str, count: int) -> str:
    """
    A simple function that will be executed in the forked process.

    Args:
        msg: A message to print
        count: Number of times to print the message

    Returns:
        str: A completion message
    """
    # The heavy dependencies should have already been imported by the firehot environment
    from os import getpid, getppid

    from mypackage.app import run_everything

    run_everything()

    for i in range(count):
        print(f"{i + 1}: {msg}")

    print(f"Process ID: {getpid()}, Parent Process ID: {getppid()}")
    return f"Completed {count} iterations"


def main():
    """Run tasks with a specific runner."""
    runner_name = "[test_hotreload]"

    start = time.time()
    print(f"{runner_name}: This should be the main time expenditure...")
    with isolate_imports("mypackage") as runner:
        print(
            f"{runner_name}: Imports have been loaded in an isolated process in {time.time() - start}s"
        )

        count_reloads = 100
        group_start = time.time()
        for _ in range(count_reloads):
            print("-" * 80)
            start = time.time()

            # Update the environment
            runner.update_environment()

            # Execute a function in the forked process
            print(f"\n{runner_name}: Executing function in forked process...")
            process_id = runner.exec(global_fn, f"Hello from {runner_name}!", 3)
            print(f"{runner_name} result: {process_id} in {time.time() - start}s")

            # Then communicate with the forked process to get the final function result
            # And print the output
            print("Communicating with the forked process...")
            result = runner.communicate_isolated(process_id)
            print(f"{runner_name} final result: {result} in {time.time() - start}s")
        print(f"Completed {count_reloads} reloads in {time.time() - group_start}s")
