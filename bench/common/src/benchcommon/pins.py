"""The check that the harnesses pin the forks of the workspace in python/.

    python -m benchcommon.pins

compares the uv source of each fork in `python/pyproject.toml` with the
one of each harness that builds zoedb, and exits with 1 when one differs.
`just check` runs it.
"""

import sys
import tomllib
from pathlib import Path

from benchcommon.servers import REPO

FORKS = ("persistent", "zodbpickle", "BTrees")
WORKSPACE = Path("python") / "pyproject.toml"
HARNESSES = (
    Path("bench") / "shootout" / "pyproject.toml",
    Path("bench") / "replay" / "pyproject.toml",
)


def sources(pyproject: Path) -> dict:
    """Return the uv source of each fork in `pyproject`, None when it has
    none."""
    table = tomllib.loads(pyproject.read_text())
    found = table.get("tool", {}).get("uv", {}).get("sources", {})
    return {name: found.get(name) for name in FORKS}


def differences(root: Path = REPO) -> list[str]:
    """Return one line for each fork that a harness pins to another source
    than the workspace."""
    wanted = sources(root / WORKSPACE)
    lines = []
    for harness in HARNESSES:
        for name, source in sources(root / harness).items():
            if source != wanted[name]:
                lines.append(
                    f"{harness}: {name} is {source}, not {wanted[name]} as in "
                    f"{WORKSPACE}"
                )
    return lines


def main() -> int:
    lines = differences()
    for line in lines:
        print(f"pins: {line}", file=sys.stderr)
    return 1 if lines else 0


if __name__ == "__main__":
    sys.exit(main())
