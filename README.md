# Hot Reload

A package to quickly hot reload large Python projects. Currently in development.

## Background

Once your project gets to a certain size, the main overhead of application startup is typically the loading of 3rd party packages. Packages can do
varying degrees of global initialization when they're imported. They can also require 100s or 1000s of additional files, which the AST parser has to
step through one by one. Cached bytecode can help but it's not a silver bullet.

Eventually your bootup can take 5-10s just to load these modules. Fine for a production service that's intended to run continuously, but
horrible for a development experience when you have to reboot after changes..

## Python Usage

```python
from hotreload import isolate_imports
from os import getpid, getppid

def run_under_environment(value: int):
   parent_pid = getppid()
   print(f"Running with value: {value} (pid: {getpid()}, parent_pid: {parent_pid})")

with isolate_imports("my_package") as environment:
   # Each exec will be run in a new process with the environment isolated, inheriting
   # the package's third party imports without having to re-import them from scratch.
   context1 = environment.exec(run_under_environment, 1)
   context2 = environment.exec(run_under_environment, 2)

   # These can potentially be long running - to wait on the completion status, you can do:
   result1 = environment.communicate_isolated(context1)
   result2 = environment.communicate_isolated(context2)

   # If you change the underlying file on disk to add an import, you can run update_environment.
   # If the files on disk don't add any new imports, it will be a no-op and keep the current
   # environment process running.
   environment.update_environment()

   # Subsequent execs will use the updated environment.
   context3 = environment.exec(run_under_environment, 3)
   result3 = environment.communicate_isolated(context3)
```

## Architecture

When launched, our Rust pipeline will parse code's AST and determine which imports are used by your project.

It will then launch a continuously running process that caches only the 3rd party packages/modules that are used by your project. We
think of this as the "template" or "parent" process because it establishes the environment that will be used to run your code. None of your
user code is run in this process.

When code changes are made in your project, we will:

- Determine if your changes affected the imported packages
- If not, we can fork the parent process and pass in your user code. This will load all modules from scratch, but because importlib caches the modules in global space, it will be a no-op because of the template.
- If so, we will tear down the current parent process and start a new one with the full 3rd party packages imported.

## Local Experiments

To test how hotreload works with a real project, we bundle a `mypackage` and `external-package` library in this repo.

```bash
make
uv run test-hotreload
```

## Unit tests

```bash
cargo test
```
