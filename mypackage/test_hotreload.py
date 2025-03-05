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

import asyncio
import time
from pathlib import Path
from hotreload import isolate_imports

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
    with open("test.txt", "w") as f:
        f.write(f"Running in forked process with PID: {__import__('os').getpid()}")

    for i in range(count):
        print(f"{i+1}: {msg}")
        time.sleep(0.5)
    return f"Completed {count} iterations"

async def main() -> None:
    """Run tasks with a specific runner."""
    package_path = str(Path(".").absolute())
    runner_name = "test_hotreload"
    
    with isolate_imports(package_path) as runner:
        print(f"{runner_name}: Imports have been loaded in an isolated process")
        
        # Execute a function in the forked process
        print(f"\n{runner_name}: Executing function in forked process...")
        result = runner.exec(
            global_fn,
            f"Hello from {runner_name}!",
            3
        )
        print(f"{runner_name} result: {result}")

        # Wait 5s
        # Then communicate with the forked process
        # And print the output
        print("Waiting 2s for results...")
        time.sleep(2)
        print("Communicating with the forked process...")
        result = runner.communicate_isolated(result)
        print(f"{runner_name} final result: {result}")


if __name__ == "__main__":
    asyncio.run(main()) 