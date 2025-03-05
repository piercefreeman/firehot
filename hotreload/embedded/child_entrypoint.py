"""
Child entrypoint for hotreload.

Intended for embeddable usage in Rust, can only import stdlib modules.

"""
import pickle
import base64
import importlib
import importlib.util
import sys
import os

# These will imported dynamically by rust 
module_path: str
file_path: str
pickled_str: str

# If we have a module path, import it first to ensure the function is available
if module_path != "null":
    print(f"Importing module: {module_path}")
    # Try to import the module or reload it if already imported
    if module_path in sys.modules:
        importlib.reload(sys.modules[module_path])
    else:
        importlib.import_module(module_path)
elif file_path != "null" and os.path.exists(file_path):
    # For ad-hoc files that aren't part of a module
    print(f"Importing ad-hoc file: {file_path}")
    
    # Get the directory and filename for the ad-hoc file
    file_dir = os.path.dirname(os.path.abspath(file_path))
    file_name = os.path.basename(file_path)
    module_name = os.path.splitext(file_name)[0]  # Remove .py extension
    
    # Add the file's directory to sys.path if it's not already there
    if file_dir not in sys.path:
        sys.path.insert(0, file_dir)
    
    # Use importlib.util to load the file as a module
    if module_name in sys.modules:
        importlib.reload(sys.modules[module_name])
    else:
        spec = importlib.util.spec_from_file_location(module_name, file_path)
        if spec:
            module = importlib.util.module_from_spec(spec)
            sys.modules[module_name] = module
            spec.loader.exec_module(module)
        else:
            raise ImportError(f"Could not import {{file_path}}")

# Decode base64 and unpickle
pickled_bytes = base64.b64decode(pickled_str)
data = pickle.loads(pickled_bytes)
func, args = data

# Run the function with args
if isinstance(args, tuple):
    result = func(*args)
elif args is not None:
    result = func(args)
else:
    result = func()
