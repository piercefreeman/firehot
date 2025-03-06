"""
Child entrypoint for hotreload.

Intended for embeddable usage in Rust, can only import stdlib modules.

"""

import base64
import importlib
import importlib.util
import pickle
import sys

# These will imported dynamically by rust
module_path: str
pickled_str: str

# If we have a module path, import it first to ensure the function is available
# Technically we could just unpickle the data and pickle will automatically try to resolve the module, but
# this lets us more explicitly handle errors and issue debugging logs.
if module_path != "null":
    print(f"Importing module: {module_path}")
    # Try to import the module or reload it if already imported
    if module_path in sys.modules:
        importlib.reload(sys.modules[module_path])
    else:
        importlib.import_module(module_path)
else:
    raise Exception("No module path provided")

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
