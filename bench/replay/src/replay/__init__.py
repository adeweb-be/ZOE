"""The log replay of the specification (B6) and the gate report of phase 4.

`read_log` reads the transactions of a store that a run of B5 kept. `run`
replays them on ZEO and on `zoe serve`, at several speeds, and counts the
conflicts by class and by path. `gate` gives the verdict of phase 4 from a
result: the `ConflictError`s of `zoe serve` are at most 10 % of ZEO's.
"""

from pathlib import Path

from replay.gate import LIMIT, Gate, gate, verdict
from replay.log import Log, Transaction, read_log
from replay.runner import PATHS, SPEEDS, STACKS, check, run

# The JSON Schema of a result.
SCHEMA = str(Path(__file__).resolve().parents[3] / "schema" / "replay.schema.json")

__all__ = [
    "LIMIT",
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
    "verdict",
]
