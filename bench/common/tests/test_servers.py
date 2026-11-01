"""The start and the stop of the servers of the benchmarks."""

import json
import os
import subprocess
import sys
import time

import pytest

from benchcommon import servers, zoebench

# A `zoe serve` that records the names of the ZOE_* variables that it got,
# says that it listens, and waits for SIGTERM.
FAKE_ZOE = """\
import json, os, signal, sys, time
from pathlib import Path
args = sys.argv[1:]
store = Path(args[args.index("--path") + 1])
names = sorted(name for name in os.environ if name.startswith("ZOE_"))
(store.parent / "environment.json").write_text(json.dumps(names))
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
print("listening 127.0.0.1:1", flush=True)
time.sleep(60)
"""
# A `zoe-bench` that prints its command, its server and its commit.
FAKE_ZOE_BENCH = """\
import json, os, sys
print(json.dumps({"command": sys.argv[1], "server": sys.argv[3],
                  "commit": os.environ.get("ZOE_BENCH_COMMIT")}))
"""


def program(directory, name, code):
    """Write `code` as the executable `name` in `directory`."""
    script = directory / f"{name}.py"
    script.write_text(code)
    path = directory / name
    path.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
    path.chmod(0o755)
    return path


def test_load_and_saturate_give_zoe_serve_no_zoe_variable_of_the_caller(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("ZOE_BINARY", str(program(tmp_path, "zoe", FAKE_ZOE)))
    monkeypatch.setenv("ZOE_CACHE_SIZE", "1")
    monkeypatch.setenv("ZOE_BENCH_COMMIT", "c" * 40)
    zoe_bench = program(tmp_path, "zoe-bench", FAKE_ZOE_BENCH)
    work, out = tmp_path / "work", tmp_path / "out"

    result = zoebench.run(str(zoe_bench), work, out, "load", ["--clients", "2"])

    assert json.loads((work / "environment.json").read_text()) == []
    assert result.parent == out and result.name.startswith("load-")
    assert json.loads(result.read_text()) == {
        "command": "load",
        "server": "127.0.0.1:1",
        "commit": "c" * 40,
    }
    assert [path.name for path in out.iterdir()] == [result.name]


def test_a_missing_zoe_binary_names_how_to_build_it(tmp_path, monkeypatch):
    monkeypatch.delenv("ZOE_BINARY", raising=False)
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path))
    with pytest.raises(FileNotFoundError, match="cargo build --release"):
        servers.zoe_binary()
    assert servers.zoe_path() == tmp_path / "release" / "zoe"

    monkeypatch.setenv("ZOE_BINARY", str(tmp_path / "zoe"))
    with pytest.raises(FileNotFoundError, match="ZOE_BINARY names no file"):
        servers.zoe_binary()


def test_stop_kills_a_process_that_ignores_sigterm():
    code = "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
    code += "print(flush=True); time.sleep(60)"
    process = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE)
    process.stdout.readline()
    started = time.monotonic()
    assert servers.stop(process, timeout=0.5) == -9
    assert time.monotonic() - started < 30


def test_the_environment_of_a_server_has_no_zoe_variable():
    environ = {"ZOE_CACHE_SIZE": "1", "PATH": "/bin", "XZOE_": "kept"}
    assert servers.scrubbed(environ) == {"PATH": "/bin", "XZOE_": "kept"}
    assert "ZOE_BINARY" not in servers.scrubbed(dict(os.environ, ZOE_BINARY="x"))
