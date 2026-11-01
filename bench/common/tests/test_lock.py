"""The bench lock: every benchmark of `just bench` holds it, and a child
command of a holder does not wait for its parent."""

import fcntl
import os
import shutil
import signal
import subprocess
import sys
import threading
import time

from benchcommon import lock
from benchcommon.servers import REPO

TIMEOUT = 120


def lines_of(stream, into):
    """Read the lines of `stream` into the list `into`, in a thread."""

    def read():
        for line in stream:
            into.append(line)

    thread = threading.Thread(target=read, daemon=True)
    thread.start()
    return thread


def test_the_bench_recipe_waits_for_the_bench_lock(tmp_path):
    just = shutil.which("just")
    assert just, "run the tests inside `devenv shell`: `just` is not on PATH"
    path = tmp_path / "bench.lock"
    env = {key: value for key, value in os.environ.items() if key != lock.FD}
    env["ZOE_BENCH_LOCK"] = str(path)
    seen = []
    with open(path, "a") as file:
        # Another benchmark holds the lock.
        fcntl.flock(file, fcntl.LOCK_EX)
        process = subprocess.Popen(
            [just, "bench", "load"],
            cwd=REPO,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        try:
            lines_of(process.stderr, seen)
            deadline = time.monotonic() + TIMEOUT
            while time.monotonic() < deadline and process.poll() is None:
                if any("wait for the bench lock" in line for line in seen):
                    break
                time.sleep(0.1)
            # The recipe waits: it has not exited.
            running = process.poll() is None
        finally:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    assert running, "".join(seen)
    assert any(f"wait for the bench lock {path}" in line for line in seen), "".join(
        seen
    )


def test_a_child_of_a_holder_takes_the_lock_at_once(tmp_path, monkeypatch):
    path = tmp_path / "bench.lock"
    monkeypatch.setenv("ZOE_BENCH_LOCK", str(path))
    monkeypatch.delenv(lock.FD, raising=False)
    messages = []
    with lock.held(messages.append) as fd:
        env = dict(os.environ)
        assert env[lock.FD] == str(fd)
        child = subprocess.run(
            [sys.executable, "-m", "benchcommon.lock"],
            env=env,
            pass_fds=lock.pass_fds(env),
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
        assert child.returncode == 0, child.stderr
        # Another open file of the lock does not get it.
        with open(path, "a") as other:
            try:
                fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
                taken = True
            except BlockingIOError:
                taken = False
        assert not taken
    assert messages == []
    assert lock.FD not in os.environ
    with open(path, "a") as other:
        fcntl.flock(other, fcntl.LOCK_EX | fcntl.LOCK_NB)
