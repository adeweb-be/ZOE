"""The real runs of the showcase, without a benchmark: the environment of
a child command, the profile of a B5 run, the result of the native gate
and of the client benchmark, and the retry of a B5 run."""

import contextlib
import dataclasses
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time

import pytest

from plonebench import __main__ as cli
from plonebench import runner, showcase, stack

OPTIONS = showcase.SetOptions(
    fixtures=("small",),
    step_seconds=5.0,
    warmup=0.0,
    trim=1.0,
    client_objects=50,
    server_cpus="0",
    zope_cpus=("1", "3"),
    locust_cpus="6",
)


def entry(kind, **fields):
    for run in showcase.plan(OPTIONS, None, "c" * 40, False)["runs"]:
        if run["kind"] == kind and all(run.get(k) == v for k, v in fields.items()):
            return run
    raise AssertionError(f"no entry {kind} {fields}")


@pytest.fixture(autouse=True)
def lock(tmp_path, monkeypatch):
    """Give each test a bench lock of its own."""
    monkeypatch.setenv("ZOE_BENCH_LOCK", str(tmp_path / "bench.lock"))


def start(directory, execute, *options):
    argv = ["--dir", str(directory), "--max-load", "1000", *OPTIONS_ARGV, *options]
    return showcase.main(argv, execute=execute)


OPTIONS_ARGV = [
    "--fixtures",
    "small",
    "--step-seconds",
    "5",
    "--warmup",
    "0",
    "--trim",
    "1",
    "--client-objects",
    "50",
]


def gone(pid, timeout=10.0):
    """Tell if the process `pid` ends within `timeout` seconds."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        time.sleep(0.05)
    return False


def test_a_child_command_gets_no_environment_of_bench_plone(monkeypatch):
    venv = "/nonexistent/venv"
    monkeypatch.setenv("VIRTUAL_ENV", venv)
    monkeypatch.setenv("UV_PROJECT_ENVIRONMENT", venv)
    monkeypatch.setenv("UV_PYTHON", "3.12")
    monkeypatch.setenv("PATH", f"{venv}/bin:/usr/bin")
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    monkeypatch.setenv("ZOE_CXX_LIBS", "/nonexistent/lib")

    env = showcase.child_environment("c" * 40, UV_PYTHON="3.14t")

    assert "VIRTUAL_ENV" not in env
    assert "UV_PROJECT_ENVIRONMENT" not in env
    assert env["PATH"] == "/usr/bin"
    assert env["UV_PYTHON"] == "3.14t"
    assert env["ZOE_BENCH_COMMIT"] == "c" * 40
    assert env["ZOE_BENCH_MACHINE"] == "reference"
    assert env["ZOE_CXX_LIBS"] == "/nonexistent/lib"


def test_only_a_stack_of_zoe_serve_with_zoedb_imports_a_profile():
    assert stack.profile_refusal(stack.STACKS["zoe-native-qi-kv"]) is None
    for name in ("zeo", "zoe-stock"):
        assert name in stack.profile_refusal(stack.STACKS[name])

    parser = cli.argparse.ArgumentParser()
    cli.add_run(parser)
    args = parser.parse_args(["--profile", "zoe.plone:default", "--steps", "4:5"])
    assert cli.options_of(args, "zoe-native-qi-kv", "edit", 0).profile == (
        "zoe.plone:default"
    )
    # A campaign gives the profile only to the stacks that take it.
    assert cli.options_of(args, "zeo", "edit", 0, campaign=True).profile is None
    campaign = cli.options_of(args, "zoe-native-qi-kv", "edit", 0, campaign=True)
    assert campaign.profile == "zoe.plone:default"


@pytest.mark.parametrize("kind", ["native", "client"])
@pytest.mark.parametrize("code", [0, 1])
def test_a_result_that_the_run_writes_is_a_run_that_ended_well(
    tmp_path, monkeypatch, kind, code
):
    run = entry(kind, python="3.14t")
    calls = []

    def command(self, argv, log, env):
        calls.append((argv, env))
        output = pathlib.Path(argv[argv.index("--output") + 1])
        output.write_text('{"cells": []}\n')
        return code

    monkeypatch.setattr(showcase.Execute, "command", command)
    (tmp_path / "state").mkdir()
    assert showcase.Execute(OPTIONS, 0, "c" * 40)(run, tmp_path) == {"exit": code}

    (argv, env), *_ = calls
    assert argv[:3] == ["just", "bench", kind]
    assert env["UV_PYTHON"] == "3.14t"
    if kind == "client":
        assert argv[argv.index("--objects") + 1] == "50"
    assert json.loads((tmp_path / f"{run['id']}.json").read_text()) == {"cells": []}
    assert list((tmp_path / "state").iterdir()) == []


def test_a_run_without_its_result_failed(tmp_path, monkeypatch):
    run = entry("native", python="3.13")
    monkeypatch.setattr(showcase.Execute, "command", lambda *_: 1)
    (tmp_path / "state").mkdir()
    with pytest.raises(RuntimeError, match="exited with 1"):
        showcase.Execute(OPTIONS, 0, "c" * 40)(run, tmp_path)
    assert list(tmp_path.glob("*.json")) == []


def test_a_b5_run_keeps_the_directory_of_an_earlier_attempt(tmp_path, monkeypatch):
    run = entry("b5", stack="zoe-native-qi-kv", scenario="edit")
    earlier = tmp_path / "runs" / run["id"]
    (earlier / "store").mkdir(parents=True)
    (earlier / "result.json").write_text('{"error": "an earlier attempt"}')
    seen = []

    def fake_run(options):
        seen.append(options)
        options.out.mkdir(parents=True)
        result = {"verification": {"verdict": "pass"}}
        (options.out / "result.json").write_text(json.dumps(result))
        return options.out

    monkeypatch.setattr(runner, "run", fake_run)
    note = showcase.Execute(OPTIONS, 460, "c" * 40)(run, tmp_path)

    assert note == {"verdict": "pass"}
    (options,) = seen
    assert options.out == earlier
    assert options.profile == "zoe.plone:default"
    assert options.steps == [(users, 5.0) for users in (1, 4, 8, 16, 32)]
    assert (options.warmup, options.trim) == (0.0, 1.0)
    assert options.settings.zope_cpus == ("1", "3")
    assert options.settings.server_port == stack.Settings().server_port + 460
    (failed,) = (tmp_path / "failed").iterdir()
    assert failed.name.startswith(run["id"] + "-")
    assert "an earlier attempt" in (failed / "result.json").read_text()
    # Only the last attempt keeps a copy of the store.
    assert not (failed / "store").exists()


def test_a_directory_of_other_options_names_them(tmp_path):
    (tmp_path / "state").mkdir()
    record = {"commit": "c" * 40, "options": OPTIONS.to_json()}
    (tmp_path / "state" / "set.json").write_text(json.dumps(record))
    assert showcase.refusal(tmp_path, "c" * 40, OPTIONS) is None

    other = dataclasses.replace(OPTIONS, zope_cpus=("1", "2"))
    why = showcase.refusal(tmp_path, "c" * 40, other)
    assert "--zope-cpus 1;3 (not --zope-cpus 1;2)" in why
    assert "commit" in showcase.refusal(tmp_path, "d" * 40, OPTIONS)


def test_the_recipe_passes_each_argument_unchanged(tmp_path):
    """`just bench showcase` gives each argument to the showcase as it is:
    a `;` and a space do not split it."""
    just = shutil.which("just")
    assert just, "run the tests inside `devenv shell`: `just` is not on PATH"
    directory = tmp_path / "a b"
    result = subprocess.run(
        [just, "bench", "showcase", "--plan", "--json", "--zope-cpus", "2;4"]
        + ["--dir", str(directory)],
        cwd=stack.REPO,
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert result.returncode == 0, result.stderr
    document = json.loads(result.stdout)
    assert document["directory"] == str(directory)
    assert document["options"]["zope_cpus"] == ["2", "4"]
    assert not directory.exists()


def test_an_interrupt_stops_locust(tmp_path):
    seen = []

    class Sample:
        def watch(self, name, pid):
            seen.append(pid)
            raise KeyboardInterrupt

    command = [sys.executable, "-c", "import time; time.sleep(60)"]
    with pytest.raises(KeyboardInterrupt):
        runner.run_locust(command, dict(os.environ), tmp_path / "locust.log", Sample())
    (pid,) = seen
    assert gone(pid)


def test_a_child_command_leaves_no_process_behind(tmp_path):
    pidfile = tmp_path / "pid"
    command = ["bash", "-c", f"sleep 300 & echo $! > {pidfile}"]
    log = tmp_path / "child.log"

    code = showcase.Execute(OPTIONS, 0, None).command(command, log, dict(os.environ))

    assert code == 0
    assert gone(int(pidfile.read_text()))


def test_sighup_stops_the_current_run_as_sigterm_does(tmp_path):
    before = signal.getsignal(signal.SIGHUP)
    handlers = []

    def execute(run, directory):
        handlers.append(
            (signal.getsignal(signal.SIGHUP), signal.getsignal(signal.SIGTERM))
        )
        (pathlib.Path(directory) / "STOP").touch()

    assert start(tmp_path / "showcase", execute) == 0
    assert handlers == [(showcase._interrupt, showcase._interrupt)]
    assert signal.getsignal(signal.SIGHUP) == before


def test_the_end_counts_the_verdicts_and_publishes_only_a_pass(tmp_path):
    directory = tmp_path / "showcase"
    unverified = "b5-small-edit-zeo"

    def execute(run, directory):
        if run["id"] == unverified:
            return {"verdict": "fail"}
        if run["kind"] == "b5":
            return {"verdict": "pass"}
        if run["id"] == "native-3.13":
            return {"exit": 1}
        if run["kind"] in ("native", "client"):
            return {"exit": 0}
        return None

    assert start(directory, execute) == 0
    text = (directory / "showcase.log").read_text()
    assert "0 failed, 0 to do; 1 B5 runs not verified pass; 1 native gates not met" in (
        text
    )
    assert f"done {unverified} in 0 s: verification fail" in text
    assert "exit 1, a cell does not pass the gate" in text
    assert "to publish" not in text

    # A failed verification stays done: no start runs it again.
    calls = []
    assert start(directory, lambda run, directory: calls.append(run["id"])) == 0
    assert calls == []


def test_two_starts_on_one_directory_never_run_a_run_twice(tmp_path, monkeypatch):
    directory = tmp_path / "showcase"
    calls = []
    held = showcase.bench_lock

    @contextlib.contextmanager
    def contended(log):
        if calls == ["build-zoe"]:
            # Another start ends the next run while this one waits for the
            # lock.
            (directory / "state" / "build-plone-zeo.done").write_text("{}")
        with held(log):
            yield

    monkeypatch.setattr(showcase, "bench_lock", contended)
    assert start(directory, lambda run, directory: calls.append(run["id"])) == 0
    assert calls[:2] == ["build-zoe", "build-plone-zoeclient"]
    assert "skip build-plone-zeo: another start ran it" in (
        (directory / "showcase.log").read_text()
    )


def test_a_resume_warns_when_the_zoe_binary_changed(tmp_path, monkeypatch):
    binary = tmp_path / "zoe"
    binary.write_bytes(b"one build")
    monkeypatch.setenv("ZOE_BINARY", str(binary))
    directory = tmp_path / "showcase"

    def first(run, directory):
        (pathlib.Path(directory) / "STOP").touch()

    assert start(directory, first) == 0
    record = json.loads((directory / "state" / "build-zoe.done").read_text())
    assert len(record["zoe_sha256"]) == 64

    binary.write_bytes(b"another build")
    assert start(directory, lambda run, directory: None) == 0
    text = (directory / "showcase.log").read_text()
    assert "warning: the zoe binary changed since build-zoe ran" in text
