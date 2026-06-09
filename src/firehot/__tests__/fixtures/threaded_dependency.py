from __future__ import annotations

import os
import threading
import time
from pathlib import Path

LOG_PATH_ENV = "FIREHOT_THREAD_DEP_IMPORT_LOG"


def write_import_log() -> None:
    log_path = os.environ.get(LOG_PATH_ENV)
    if log_path is None:
        raise RuntimeError(f"{LOG_PATH_ENV} must be set")

    path = Path(log_path)
    previous = path.read_text() if path.exists() else ""
    path.write_text(previous + "imported\n")


def keep_alive() -> None:
    time.sleep(30)


write_import_log()
threading.Thread(target=keep_alive, daemon=True).start()
