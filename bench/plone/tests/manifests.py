"""The files of a fixture that Locust reads, and Locust on them.

`write_like_the_build` writes `spec.json`, `credentials.json` and
`manifest.json` without Zope, with the fields that `inzope/build_site.py`
writes, from the plan of a spec. The states, the versions and the
workflow histories are those that a build gives: a Document or a News
Item has the state of its plan and one version, a File has no state, and
the other types are published.

`probe` runs `scripts/locust_probe.py` on these files: the Locust file of
a scenario and its users, with no server.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

from plonebench import corpus

SCRIPTS = Path(__file__).resolve().parent / "scripts"


def state_of(item: corpus.Item) -> str | None:
    if item.portal_type in ("Document", "News Item"):
        return item.state
    if item.portal_type == "File":
        return None
    return corpus.PUBLISHED


def write_like_the_build(plan: corpus.Plan, directory: Path) -> Path:
    """Write the files of a fixture of `plan` into `directory`."""
    spec = plan.spec
    directory.mkdir(parents=True, exist_ok=True)
    items = []
    for item in plan.items:
        state = state_of(item)
        items.append(
            {
                "path": item.path,
                "portal_type": item.portal_type,
                "title": item.title,
                "state": state,
                "pool": item.pool,
                "section": item.section,
                "size": item.size,
                "uid": hashlib.sha256(
                    f"{spec.seed}:uid:{item.path}".encode()
                ).hexdigest()[:32],
                "versions": 1 if item.portal_type in ("Document", "News Item") else 0,
                "workflow_history": 1 if state == corpus.PRIVATE else 2,
                "description": item.description,
            }
        )
    manifest = {
        "spec": spec.to_json(),
        "site": "Plone",
        "items": items,
        "users": corpus.user_ids(spec),
        "search_terms": corpus.search_terms(plan),
        "subjects": plan.words[50:90],
        "catalog_size": len(items) + 1,
        "build_seconds": 0.0,
    }
    with open(directory / "manifest.json", "w") as file:
        json.dump(manifest, file, indent=1)
    (directory / "spec.json").write_text(json.dumps(spec.to_json(), indent=1))
    (directory / "credentials.json").write_text(json.dumps({"password": "unused"}))
    return directory


def probe(
    directory: Path, scenario: str, users: int, calls: int, out: Path, timeout=120
) -> dict:
    """Run the Locust file of `scenario` on the fixture files of
    `directory`, with `users` users and `calls` calls of the task of each
    one, in a process of its own. Return what the probe measured."""
    env = {
        **os.environ,
        "PLONEBENCH_MANIFEST": str(directory / "manifest.json"),
        "PLONEBENCH_CREDENTIALS": str(directory / "credentials.json"),
        "PLONEBENCH_LEDGER": str(out.with_suffix(".jsonl")),
    }
    command = [
        sys.executable,
        str(SCRIPTS / "locust_probe.py"),
        scenario,
        str(users),
        str(calls),
        str(out),
    ]
    try:
        result = subprocess.run(
            command, env=env, capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        pytest.fail(f"{scenario}: Locust did not start and work in {timeout} s")
    assert result.returncode == 0, result.stderr[-3000:]
    return json.loads(out.read_text())
