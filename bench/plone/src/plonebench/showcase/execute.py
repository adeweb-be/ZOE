"""The real runs of the entries of the set. Each run writes its output to
DIR/logs/<id>.log."""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

from benchcommon import lock
from benchcommon import servers as common

from plonebench import corpus, fixture, runner, stack
from plonebench.showcase.plan import SetOptions


def _stop_group(process: subprocess.Popen, timeout: float = 60.0) -> None:
    """Stop each process of the group of `process`: SIGTERM, then SIGKILL
    for what is left after `timeout` seconds."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            return
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            process.poll()
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.1)


def child_environment(commit: str | None, **extra: str) -> dict[str, str]:
    """Return the environment of a child command: not the environment of
    bench/plone, and the commit of the showcase. ZOE_BENCH_MACHINE and
    ZOE_CXX_LIBS go on, so every document names the same machine."""
    env = dict(os.environ)
    venv = env.pop("VIRTUAL_ENV", None)
    env.pop("UV_PROJECT_ENVIRONMENT", None)
    env.pop("UV_PYTHON", None)
    if venv:
        paths = env.get("PATH", "").split(os.pathsep)
        env["PATH"] = os.pathsep.join(p for p in paths if p != f"{venv}/bin")
    if commit:
        env["ZOE_BENCH_COMMIT"] = commit
    env.update(extra)
    return env


@contextlib.contextmanager
def _environ(**values: str | None):
    """Set the variables of `values` that are not None in os.environ, and
    restore them at the end."""
    before = {key: os.environ.get(key) for key in values}
    os.environ.update({key: value for key, value in values.items() if value})
    try:
        yield
    finally:
        for key, value in before.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


class Execute:
    """Run the entries of the set. Each run writes its output to
    DIR/logs/<id>.log."""

    def __init__(self, options: SetOptions, port_offset: int, commit: str | None):
        self.options = options
        self.settings = options.settings(port_offset)
        self.commit = commit

    def __call__(self, run: dict, directory: Path):
        return getattr(self, run["kind"])(run, Path(directory))

    def log(self, run: dict, directory: Path) -> Path:
        path = directory / "logs" / f"{run['id']}.log"
        path.parent.mkdir(parents=True, exist_ok=True)
        return path

    def command(self, command: list[str], log: Path, env: dict[str, str]) -> int:
        """Run `command` in a process group of its own, and return its exit
        code. When it ends, or on an interrupt, stop what is left of its
        group, so that no process of the run survives it."""
        with open(log, "a") as out:
            out.write(f"$ {' '.join(command)}\n")
            out.flush()
            process = subprocess.Popen(
                command,
                cwd=stack.REPO,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                # The bench lock of the run, which `just bench` takes through
                # this open file.
                pass_fds=lock.pass_fds(env),
            )
            try:
                return process.wait()
            finally:
                _stop_group(process)

    @contextlib.contextmanager
    def inprocess(self, log: Path):
        """Send the output of an in-process run to its log, with the commit
        of the showcase."""
        with (
            open(log, "a") as out,
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(out),
            _environ(ZOE_BENCH_COMMIT=self.commit),
        ):
            try:
                yield
            except SystemExit as error:
                # The runner exits on a missing prerequisite.
                raise RuntimeError(str(error)) from None

    def build(self, run: dict, directory: Path) -> None:
        log = self.log(run, directory)
        if run["target"] == "zoe":
            if os.environ.get("ZOE_BINARY"):
                common.zoe_binary()
                return
            command = ["cargo", "build", "--release", "--locked"]
            command += ["-p", "zoe-server", "--bin", "zoe"]
        else:
            command = ["just", "plone", run["target"].removeprefix("plone-"), "build"]
        code = self.command(command, log, child_environment(self.commit))
        if code != 0:
            raise RuntimeError(f"{' '.join(command)} exited with {code}")

    def prepare(self, run: dict, directory: Path) -> None:
        with self.inprocess(self.log(run, directory)):
            spec = corpus.PRESETS[run["preset"]]
            fixture.prepare(spec, run["fixture"], self.settings)

    def b5(self, run: dict, directory: Path) -> dict:
        out = directory / "runs" / run["id"]
        if out.exists():
            # An earlier attempt failed or stopped. Keep its files for the
            # diagnosis, but not its copy of the store, so that failures
            # do not fill the disk.
            shutil.rmtree(out / "store", ignore_errors=True)
            stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
            failed = directory / "failed" / f"{run['id']}-{stamp}"
            failed.parent.mkdir(parents=True, exist_ok=True)
            out.rename(failed)
        config = stack.STACKS[run["stack"]]
        options = runner.RunOptions(
            stack=run["stack"],
            scenario=run["scenario"],
            fixture=run["fixture"],
            steps=[(users, run["step_seconds"]) for users in run["users"]],
            warmup=run["warmup_seconds"],
            trim=run["trim_seconds"],
            settings=stack.campaign_settings(config, self.settings),
            out=out,
            profile=run["profile"],
        )
        with self.inprocess(self.log(run, directory)):
            runner.run(options)
        result = json.loads((out / "result.json").read_text())
        return {"verdict": result.get("verification", {}).get("verdict", "not run")}

    def summary(self, run: dict, directory: Path) -> None:
        from plonebench import __main__ as cli

        document = directory / f"{run['id']}.json"
        partial = directory / "state" / f"{run['id']}.json"
        partial.unlink(missing_ok=True)
        argv = ["report", *(str(directory / "runs" / id_) for id_ in run["runs"])]
        argv += ["--out", str(directory / f"{run['id']}.md"), "--json", str(partial)]
        with self.inprocess(self.log(run, directory)):
            try:
                code = cli.main(argv)
            except SystemExit as error:
                code = error.code
        if code not in (0, None) or not partial.exists():
            raise RuntimeError(f"report exited with {code}")
        partial.replace(document)

    def native(self, run: dict, directory: Path) -> dict:
        return self.shootout(run, directory, ["just", "bench", "native"])

    def client(self, run: dict, directory: Path) -> dict:
        command = ["just", "bench", "client", "--objects", str(run["objects"])]
        return self.shootout(run, directory, command)

    def shootout(self, run: dict, directory: Path, command: list[str]) -> dict:
        """Run the native gate or the client benchmark in the environment
        of bench/shootout, on the Python of the run. A run that writes its
        result ended well, also when it exits with 1: the verdict is the
        work of the report."""
        partial = directory / "state" / f"{run['id']}.json"
        partial.unlink(missing_ok=True)
        command = [*command, "--output", str(partial)]
        env = child_environment(
            self.commit,
            UV_PYTHON=run["python"],
            ZOE_BINARY=str(stack.zoe_binary().absolute()),
        )
        code = self.command(command, self.log(run, directory), env)
        if code not in (0, 1) or not partial.exists():
            raise RuntimeError(f"{' '.join(command[:3])} exited with {code}")
        # Python exits with 1 also on an error, which can leave a part of
        # the result. Only a whole JSON document is a result.
        try:
            json.loads(partial.read_text())
        except ValueError:
            raise RuntimeError(
                f"{' '.join(command[:3])} exited with {code}, and {partial.name} "
                "is not a whole JSON document"
            ) from None
        partial.replace(directory / f"{run['id']}.json")
        return {"exit": code}
