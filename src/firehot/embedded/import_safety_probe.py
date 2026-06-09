"""
Probe imports in a disposable Python process and report modules that make the process unsafe
to use as a fork parent.

This script is embedded into the Rust binary and executed with ``python -c``.
"""

import builtins
import importlib
import json
import os
import sys
import threading
import traceback
from collections.abc import Iterable
from typing import Any, NoReturn

try:
    from firehot._core import get_total_thread_count
except ImportError:

    def get_total_thread_count() -> int:
        return threading.active_count()


MARKER = "FIREHOT_IMPORT_SAFETY_PROBE="


def emit(payload: dict[str, Any], status: int) -> NoReturn:
    sys.stdout.write(MARKER + json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()
    os._exit(status)


def loaded_requested_module(loaded_modules: Iterable[str], requested_module: str) -> bool:
    if requested_module in loaded_modules:
        return True

    for loaded_module in loaded_modules:
        if requested_module.startswith(loaded_module + "."):
            return True
        if loaded_module.startswith(requested_module + "."):
            return True

    return False


raw_modules: list[Any] = []
try:
    raw_modules = json.loads(sys.argv[1]) if len(sys.argv) > 1 else []
    if not isinstance(raw_modules, list):
        raise ValueError("Expected a JSON list of module names")
except Exception as exc:
    emit(
        {
            "status": "error",
            "module": None,
            "error": str(exc),
            "traceback": traceback.format_exc(),
        },
        2,
    )

unsafe_modules: list[str] = []
unsafe_module_set: set[str] = set()
requested_modules = [str(module_name) for module_name in raw_modules]


def mark_unsafe(module_name: str) -> None:
    if module_name not in unsafe_module_set:
        unsafe_module_set.add(module_name)
        unsafe_modules.append(module_name)


def build_tracking_import(imported_modules: list[str], original_import: Any) -> Any:
    def tracking_import(
        name: str,
        globals: dict[str, Any] | None = None,
        locals: dict[str, Any] | None = None,
        fromlist: tuple[Any, ...] = (),
        level: int = 0,
    ) -> Any:
        if level == 0 and name:
            imported_modules.append(str(name))
            for imported in fromlist or ():
                if imported != "*":
                    imported_modules.append(f"{name}.{imported}")
        return original_import(name, globals, locals, fromlist, level)

    return tracking_import


for raw_module_name in raw_modules:
    module_name = str(raw_module_name)
    if module_name in unsafe_module_set:
        continue

    try:
        imported_modules: list[str] = []
        original_import = builtins.__import__
        tracking_import = build_tracking_import(imported_modules, original_import)

        pre_modules = set(sys.modules)
        pre_total = get_total_thread_count()
        builtins.__import__ = tracking_import
        try:
            importlib.import_module(module_name)
        finally:
            builtins.__import__ = original_import
        post_total = get_total_thread_count()
        touched_modules = (set(sys.modules) - pre_modules) | set(imported_modules) | {module_name}
    except Exception as exc:
        emit(
            {
                "status": "error",
                "module": module_name,
                "error": str(exc),
                "traceback": traceback.format_exc(),
            },
            2,
        )

    if post_total > pre_total:
        mark_unsafe(module_name)
        for requested_module in requested_modules:
            if loaded_requested_module(touched_modules, requested_module):
                mark_unsafe(requested_module)
    else:
        touched_unsafe_module = any(
            loaded_requested_module(touched_modules, unsafe_module)
            for unsafe_module in unsafe_module_set
        )
        if touched_unsafe_module:
            mark_unsafe(module_name)
            for requested_module in requested_modules:
                if loaded_requested_module(touched_modules, requested_module):
                    mark_unsafe(requested_module)

emit({"status": "ok", "unsafe_modules": unsafe_modules}, 0)
