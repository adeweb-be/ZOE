"""The check of the security index of `zoe.plone` (ADR 0061, gate item G5)
on a synthetic site and on `xlarge-mini`: no difference.

`plonebench/catalog/security.py` runs inside Zope, in the interpreter of
the zoe scenario, with an in-process `zoe.Storage` in the temporary
directory of the test. At each step of security changes, the resolved
value of each entry equals the value of a stock `KeywordIndex` indexed
again in full, and the searches of each token give the same rids on the
Python path, in the mirror and in the stock index.

- The synthetic site holds the cases that the corpus does not hold: an
  Owner local role, a sequence setting below the site root, a block, a
  group, and a comment below its conversation.
- `xlarge-mini` is the shape of `xlarge` at about 1/100; `build_site.py`
  builds it in the store, for about 3 minutes, and the check adds the
  steps that make opaque entries.

They need `just plone zoe build`. `just bench plone test` runs them; the
run on `xlarge-mini` needs `ZOE_BENCH_SECURITY_MINI=1`.
"""

import json
import os
import subprocess
from pathlib import Path

import pytest

from plonebench import corpus, stack

SCENARIO = stack.PLONE / "zoe"
INTERPRETER = SCENARIO / "parts" / "instance" / "bin" / "interpreter"
CHECK = Path(stack.INZOPE).parent / "catalog" / "security.py"

pytestmark = pytest.mark.skipif(
    not INTERPRETER.exists(), reason="needs the zoe scenario: just plone zoe build"
)


def zope_conf(home: Path) -> Path:
    """Write a zope.conf for an in-process `zoe.Storage` in `home`."""
    conf = home / "zope.conf"
    conf.write_text(
        "%import zoe\n"
        f"%define INSTANCEHOME {SCENARIO / 'parts' / 'instance'}\n"
        "instancehome $INSTANCEHOME\n"
        "debug-mode off\n"
        "security-policy-implementation C\n"
        "verbose-security off\n"
        "default-zpublisher-encoding utf-8\n"
        "<zodb_db main>\n"
        "    cache-size 30000\n"
        "    <zoe>\n"
        f"      path {home / 'zoe'}\n"
        f"      blob-dir {home / 'blobstorage'}\n"
        "    </zoe>\n"
        "    mount-point /\n"
        "</zodb_db>\n"
    )
    return conf


def run_in_zope(home: Path, script: str, *args: str, timeout: int) -> None:
    env = dict(stack.clean_environment(), LC_ALL="C.UTF-8")
    command = [
        str(INTERPRETER),
        str(Path(stack.INZOPE) / "run.py"),
        str(zope_conf(home)),
        script,
        *args,
    ]
    done = subprocess.run(
        command,
        env=env,
        cwd=home,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if done.returncode != 0:
        tail = "\n".join((done.stdout + done.stderr).splitlines()[-40:])
        raise AssertionError(f"{script} exited with {done.returncode}:\n{tail}")


def check(home: Path, *options: str) -> dict:
    out = home / "security-check.json"
    run_in_zope(home, str(CHECK), str(out), *options, timeout=3600)
    report = json.loads(out.read_text())
    assert report["stock_stale"] == 0, report
    for step in report["steps"]:
        assert step["differ"] == 0, step
        assert step["searches_differ"] == [], step
        assert step["native"] == step["searches"], step
    assert report["differ"] == 0
    return report


def test_the_check_finds_no_difference_on_a_synthetic_site(tmp_path):
    report = check(tmp_path, "--synthetic")
    names = [step["step"] for step in report["steps"]]
    assert names[0] == "install"
    assert "share on the largest folder" in names
    share = report["steps"][1]
    transition = report["steps"][2]
    # A share or a transition writes the entry of the folder, whatever the
    # size of its subtree.
    assert share["records"] < 10, share
    assert transition["transition"] is not None
    assert report["largest"]["descendants"] > 20


@pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_SECURITY_MINI") != "1",
    reason="builds xlarge-mini for about 3 minutes: set ZOE_BENCH_SECURITY_MINI=1",
)
def test_the_check_finds_no_difference_on_xlarge_mini(tmp_path):
    spec = tmp_path / "spec.json"
    spec.write_text(json.dumps(corpus.PRESETS["xlarge-mini"].to_json()))
    credentials = tmp_path / "credentials.json"
    credentials.write_text(json.dumps({"password": "security-check-pw"}))
    os.chmod(credentials, 0o600)
    run_in_zope(
        tmp_path,
        "build_site.py",
        str(spec),
        str(credentials),
        str(tmp_path / "manifest.json"),
        timeout=3600,
    )
    report = check(tmp_path, "--opaque")
    names = [step["step"] for step in report["steps"]]
    assert "a setting that is a str" in names
    assert report["largest"]["descendants"] > 1000
