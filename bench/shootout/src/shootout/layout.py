"""The layout of the results repository.

A full run goes to `results/<machine>/<python>/<storage>/<run>.json`, a
quick run to the same path under `quick/` and a gate run under `gate/`.
`index.json` lists the full runs.
"""

import json
from pathlib import Path

from shootout.environment import python_tag

INDEX_VERSION = 1


def run_set(run: dict) -> str:
    """Return the set of a run: quick, gate or full. A result without the
    key `set` is quick or full."""
    return run.get("set") or ("quick" if run["quick"] else "full")


def result_path(root: Path, result: dict) -> Path:
    """Return the path of a result document under the repository root."""
    run = result["run"]
    stamp = run["started"].replace("-", "").replace(":", "")
    name = f"{stamp}-{run['commit'][:12]}" if run["commit"] else stamp
    return (
        root
        / {"quick": "quick", "gate": "gate"}.get(run_set(run), "results")
        / result["machine"]["name"]
        / python_tag(result["python"])
        / result["storage"]
        / f"{name}.json"
    )


def write_index(root: Path) -> Path:
    """Write index.json, the list of full runs that the dashboard reads."""
    entries = []
    for path in sorted((root / "results").glob("*/*/*/*.json")):
        result = json.loads(path.read_text())
        entries.append(
            {
                "path": path.relative_to(root).as_posix(),
                "machine": result["machine"]["name"],
                "python": python_tag(result["python"]),
                "storage": result["storage"],
                "started": result["run"]["started"],
                "commit": result["run"]["commit"],
            }
        )
    entries.sort(key=lambda e: (e["started"], e["path"]))
    index = root / "index.json"
    index.write_text(
        json.dumps({"index_version": INDEX_VERSION, "results": entries}, indent=2)
        + "\n"
    )
    return index
