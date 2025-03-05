#!/usr/bin/env python3
"""
Example demonstrating the isolate_imports context manager.

This shows how to:
1. Isolate imports for a specific package path
2. Execute functions in a forked process using async/await syntax

Run this with:
    python examples/isolate_imports_example.py
"""

import asyncio
import time
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
    print(f"Running in forked process with PID: {__import__('os').getpid()}")
    for i in range(count):
        print(f"{i+1}: {msg}")
        time.sleep(0.5)
    return f"Completed {count} iterations"

async def main() -> None:
    """Main example function demonstrating the requested API."""
    # Path to the package to isolate imports for
    package_path = "."  # Current directory, adjust as needed
    
    print("Starting with isolate_imports context manager...")
    
    # Use the context manager as specified in the API request
    async with isolate_imports(package_path) as runner:
        print("Imports have been loaded in an isolated process")
        
        # Wait for a bit as specified in the requested API
        print("Sleeping for 10 seconds...")
        await asyncio.sleep(10)
        
        # Execute a function in the forked process, as specified in the API
        print("\nExecuting function in forked process...")
        # Pass a tuple as args
        await runner.exec(
            global_fn,
            ("Hello from isolate_imports!", 3)
        )
        
        # Execute another function with different arguments
        print("\nExecuting function again with different arguments...")
        await runner.exec(
            global_fn,
            ("Second execution", 2)
        )
    
    print("\nContext manager exited")

if __name__ == "__main__":
    asyncio.run(main()) 