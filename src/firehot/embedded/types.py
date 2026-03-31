"""
Common types for embedded scripts. Logic is not supported here since the final
injected scripts need to be run standalone.

"""

from typing import Optional, TypedDict

SerializedCall = TypedDict(
    "SerializedCall",
    {
        "func_module_path": Optional[str],
        "func_name": str,
        "func_qualname": str,
        "args": tuple,
    },
)
