"""The GIL record of the worker processes of a zodbshootout run.

`environ` gives the environment of a zodbshootout run that records the GIL
of each process, and `INHERIT` the variables that pyperf must pass to its
worker. `read` reads the records when the run ends. `startup/sitecustomize.py`
writes them.
"""

import json
import os
from pathlib import Path

VARIABLE = "ZOE_BENCH_GIL_RECORDS"
STARTUP = Path(__file__).resolve().parent / "startup"
# pyperf starts its worker with a reduced environment.
INHERIT = ("PYTHONPATH", VARIABLE)


def environ(records: Path) -> dict[str, str]:
    """Return the environment of a run that writes its records to records."""
    env = dict(os.environ)
    paths = [str(STARTUP)]
    if env.get("PYTHONPATH"):
        paths.append(env["PYTHONPATH"])
    env["PYTHONPATH"] = os.pathsep.join(paths)
    env[VARIABLE] = str(records)
    return env


def read(records: Path) -> list[dict]:
    """Return the record of each process, from the files in records."""
    workers = [json.loads(path.read_text()) for path in records.glob("*.json")]
    return sorted(workers, key=lambda w: (w["gil_enabled"], w["gil_enabled_by"]))
