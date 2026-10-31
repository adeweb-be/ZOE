"""The atomic write of a file: a reader sees the old bytes or the new
bytes, never a part of them."""

import json
import os
from pathlib import Path


def write_bytes(path: Path, data: bytes) -> None:
    """Write `data` to a temporary file beside `path`, then replace `path`
    with it. Create the parent directories."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        partial.write_bytes(data)
        os.replace(partial, path)
    finally:
        partial.unlink(missing_ok=True)


def write_json(path: Path, data, indent: int = 2) -> None:
    """Write `data` as JSON, with a newline at the end, atomically."""
    write_bytes(path, (json.dumps(data, indent=indent) + "\n").encode())
