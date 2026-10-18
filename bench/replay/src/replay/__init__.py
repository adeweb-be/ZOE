"""The log replay of the specification (B6) and its report.

`read_log` reads the transactions of a store that a run of B5 kept. `run`
replays them on ZEO and on `zoe serve`, at several speeds, and counts the
conflicts by class and by path. `gate` gives the measure of the success
criterion from a result: the `ConflictError`s of `zoe serve` as a share of
ZEO's. It is a measure, not a gate.
"""

from pathlib import Path

from replay.gate import Gate, gate
from replay.log import Log, Transaction, read_log
from replay.runner import PATHS, SPEEDS, STACKS, check, run

# The JSON Schema of a result.
SCHEMA = str(Path(__file__).resolve().parents[3] / "schema" / "replay.schema.json")

__all__ = [
    "PATHS",
    "SCHEMA",
    "SPEEDS",
    "STACKS",
    "Gate",
    "Log",
    "Transaction",
    "check",
    "gate",
    "read_log",
    "run",
]
