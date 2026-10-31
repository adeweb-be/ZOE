"""The servers of the benchmarks: a free port, the `zoe` binary, the
environment of a server, a ZEO server and `zoe serve` on a new store, and
the stop of a process."""

import contextlib
import os
import signal
import socket
import subprocess
import sys
import time
from collections.abc import Iterator, Mapping
from pathlib import Path

# The repository that holds bench/common.
REPO = Path(__file__).resolve().parents[4]
# The longest time that a server takes to listen or to stop, in seconds.
TIMEOUT = 30.0
# The interval between two looks at a server that starts, in seconds.
POLL = 0.05
# The command that builds the `zoe` binary of `zoe_path`.
BUILD = "cargo build --release --locked -p zoe-server --bin zoe"


def free_port() -> int:
    """Return a TCP port of 127.0.0.1 that no socket holds now."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def zoe_path() -> Path:
    """Return the path of the `zoe` binary: `ZOE_BINARY`, else
    `target/release/zoe` of the repository, under `CARGO_TARGET_DIR` when
    it is set. The file can be missing."""
    if binary := os.environ.get("ZOE_BINARY"):
        return Path(binary)
    # cargo runs in the repository, so a relative target directory starts
    # there.
    target = REPO / (os.environ.get("CARGO_TARGET_DIR") or "target")
    return target / "release" / "zoe"


def zoe_binary() -> Path:
    """Return the `zoe` binary of `zoe_path`. Raise FileNotFoundError, with
    the command that builds it, when it is missing."""
    binary = zoe_path()
    if not binary.is_file():
        if os.environ.get("ZOE_BINARY"):
            raise FileNotFoundError(f"ZOE_BINARY names no file: {binary}")
        raise FileNotFoundError(f"{binary} is missing: run `{BUILD}`")
    return binary


def scrubbed(environ: Mapping[str, str] | None = None) -> dict[str, str]:
    """Return a copy of `environ`, by default os.environ, without the
    `ZOE_*` variables. So `zoe serve` keeps the settings of its command
    line and its defaults."""
    environ = os.environ if environ is None else environ
    return {key: value for key, value in environ.items() if not key.startswith("ZOE_")}


def stop(process: subprocess.Popen, timeout: float = TIMEOUT) -> int | None:
    """Stop `process`: SIGTERM, then a wait of `timeout` seconds, then
    SIGKILL. Return its exit code."""
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    return process.returncode


def wait_for_port(
    port: int, process: subprocess.Popen, log: Path, timeout: float = TIMEOUT
) -> None:
    """Wait until the ZEO server `process` accepts connections on `port`.
    Raise RuntimeError, with its log, when it stops or does not listen."""
    deadline = time.monotonic() + timeout
    while True:
        if process.poll() is not None:
            raise RuntimeError(f"the ZEO server stopped:\n{_text(log)}")
        with contextlib.suppress(OSError):
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return
        if time.monotonic() > deadline:
            raise RuntimeError(
                f"the ZEO server did not listen on port {port}:\n{_text(log)}"
            )
        time.sleep(POLL)


def wait_for_listening(
    out: Path, process: subprocess.Popen, log: Path, timeout: float = TIMEOUT
) -> str:
    """Return the address that `zoe serve` listens on. The server writes
    `listening <address>` to its stdout, the file `out`, once it accepts
    connections. Raise RuntimeError, with its log, when it stops or does
    not listen."""
    deadline = time.monotonic() + timeout
    while True:
        for line in _text(out).splitlines():
            if line.startswith("listening "):
                return line.split()[1]
        if process.poll() is not None:
            raise RuntimeError(f"zoe serve stopped:\n{_text(log)}")
        if time.monotonic() > deadline:
            raise RuntimeError(f"zoe serve did not listen:\n{_text(log)}")
        time.sleep(POLL)


def host_and_port(address: str) -> tuple[str, int]:
    """Split an address of `zoe serve`, such as `[::1]:8100`."""
    host, _, port = address.rpartition(":")
    return host.strip("[]"), int(port)


# The program of a ZEO server in the interpreter of this process.
RUNZEO = ("-m", "ZEO.runzeo")


@contextlib.contextmanager
def zeo_server(
    directory: Path,
    program: tuple[str, ...] = RUNZEO,
    env: Mapping[str, str] | None = None,
    timeout: float = TIMEOUT,
) -> Iterator[int]:
    """Run a ZEO server on the FileStorage `directory/Data.fs`, with the
    arguments `program` of this interpreter, and yield its port. The
    server writes its log to `directory/zeo.log`. Its environment is
    `env`, by default this one without `ZOE_*`."""
    port = free_port()
    log = directory / "zeo.log"
    command = [sys.executable, *program, "-a", f"127.0.0.1:{port}"]
    command += ["-f", str(directory / "Data.fs")]
    with log.open("wb") as out:
        server = subprocess.Popen(
            command,
            stdout=out,
            stderr=subprocess.STDOUT,
            env=scrubbed() if env is None else dict(env),
        )
    try:
        wait_for_port(port, server, log, timeout)
        yield port
    finally:
        stop(server, timeout)


@contextlib.contextmanager
def zoe_serve(
    directory: Path, args: tuple[str, ...] = (), timeout: float = TIMEOUT
) -> Iterator[str]:
    """Run `zoe serve` on the new store `directory/store`, on a free port of
    127.0.0.1, with `args` after its own arguments. Yield the address that
    it listens on, as `host:port`.

    The server writes its stdout to `directory/zoe-serve.out` and its
    stderr to `directory/zoe-serve.log`. No `ZOE_*` variable of the
    environment reaches it.
    """
    out = directory / "zoe-serve.out"
    log = directory / "zoe-serve.log"
    command = [str(zoe_binary()), "serve", "--path", str(directory / "store")]
    # The port 0 takes a free port.
    command += ["--listen", "127.0.0.1:0", *args]
    with out.open("wb") as stdout, log.open("wb") as stderr:
        server = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env=scrubbed(),
        )
    try:
        yield wait_for_listening(out, server, log, timeout)
    finally:
        # SIGTERM makes zoe serve close its store cleanly.
        stop(server, timeout)


def _text(path: Path) -> str:
    try:
        return path.read_text(errors="replace")
    except OSError:
        return ""
