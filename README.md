# Hot Reload

A package to quickly hot reload large Python projects. Currently in development.

## Background

Once your project gets to a certain size, the main overhead of application startup is typically the loading of 3rd party packages. Packages can do
varying degrees of global initialization when they're imported. They can also require 100s or 1000s of additional files, which the AST parser has to
step through one by one. Cached bytecode can help but it's not a silver bullet.

Eventually your bootup can take 5-10s just to load these modules. Fine for a production service that's intended to run continuously, but
horrible for a development experience when you have to reboot after changes.

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
