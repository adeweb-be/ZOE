"""The real runs of the showcase, without a benchmark: the environment of
a child command, the profile of a B5 run, the result of the native gate
and of the client benchmark, and the retry of a B5 run."""

import dataclasses
import json
import pathlib
import shutil
import subprocess

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
    showcase.Execute(OPTIONS, 0, "c" * 40)(run, tmp_path)

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
    earlier.mkdir(parents=True)
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

    assert note == "verification pass"
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
