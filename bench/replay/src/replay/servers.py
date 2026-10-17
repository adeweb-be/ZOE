"""The servers and the clients of a run: a ZEO 6 server on a FileStorage
with `ZEO.ClientStorage`, and `zoe serve` with `zoe.zeo.ClientStorage`, the
native client."""

import contextlib
import hashlib
import os
import platform
import signal
import socket
import subprocess
import sys
import time
from collections.abc import Iterator
from pathlib import Path

# The longest time that a server takes to listen, in seconds.
TIMEOUT = 30
# The program of the ZEO server process. In the interpreter of a Plone
# instance, eggs share namespace packages such as `zc`. pkg_resources
# declares them when it loads, so the program loads it first when it can.
RUNZEO = """\
import runpy
import warnings

with warnings.catch_warnings():
    warnings.simplefilter("ignore")
    try:
        import pkg_resources  # noqa: F401
    except ImportError:
        pass
runpy.run_module("ZEO.runzeo", run_name="__main__", alter_sys=True)
"""


def child_environment() -> dict[str, str]:
    """Return the environment of a server process: the import path of this
    process, so the ZEO server has the classes of the replay, and no ZOE_*
    variable, so `zoe serve` keeps its defaults."""
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    environ["PYTHONPATH"] = os.pathsep.join(p for p in sys.path if p)
    return environ


def zoe_binary() -> str:
    """Return the `zoe` binary: `ZOE_BINARY`, else `target/release/zoe`
    of the repository, under `CARGO_TARGET_DIR` when it is set."""
    if binary := os.environ.get("ZOE_BINARY"):
        return binary
    root = Path(__file__).resolve().parents[4]
    target = root / (os.environ.get("CARGO_TARGET_DIR") or "target")
    return str(target / "release" / "zoe")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextlib.contextmanager
def zeo_server(directory: Path) -> Iterator[tuple[str, int]]:
    """Run a ZEO server on a new FileStorage in directory. Yield its
    address."""
    port = free_port()
    log = directory / "zeo.log"
    with log.open("wb") as out:
        server = subprocess.Popen(
            [
                sys.executable,
                "-c",
                RUNZEO,
                "-a",
                f"127.0.0.1:{port}",
                "-f",
                str(directory / "Data.fs"),
            ],
            stdout=out,
            stderr=subprocess.STDOUT,
            env=child_environment(),
        )
    try:
        deadline = time.monotonic() + TIMEOUT
        while True:
            if server.poll() is not None:
                raise RuntimeError(f"the ZEO server stopped:\n{log.read_text()}")
            with contextlib.suppress(OSError):
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            if time.monotonic() > deadline:
                raise RuntimeError(f"the ZEO server did not listen:\n{log.read_text()}")
            time.sleep(0.05)
        yield ("127.0.0.1", port)
    finally:
        server.terminate()
        try:
            server.wait(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


@contextlib.contextmanager
def zoe_server(directory: Path, args=()) -> Iterator[tuple[str, int]]:
    """Run `zoe serve` on a new store in directory, with `args` after its
    own arguments. Yield its address."""
    out = directory / "zoe-serve.out"
    log = directory / "zoe-serve.log"
    command = [zoe_binary(), "serve", "--path", str(directory / "store"),
               "--listen", "127.0.0.1:0", *args]  # fmt: skip
    with out.open("wb") as stdout, log.open("wb") as stderr:
        server = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env=child_environment(),
        )
    try:
        deadline = time.monotonic() + TIMEOUT
        address = None
        while address is None:
            for line in out.read_text(errors="replace").splitlines():
                if line.startswith("listening "):
                    host, _, port = line.split()[1].rpartition(":")
                    address = (host.strip("[]"), int(port))
            if address is None:
                if server.poll() is not None:
                    raise RuntimeError(f"zoe serve stopped:\n{log.read_text()}")
                if time.monotonic() > deadline:
                    raise RuntimeError(f"zoe serve did not listen:\n{log.read_text()}")
                time.sleep(0.05)
        yield address
    finally:
        server.send_signal(signal.SIGTERM)
        try:
            server.wait(timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


def server(stack: str, directory: Path, serve_args=()):
    """Return the context manager of the server of `stack`. `serve_args` go
    to `zoe serve`."""
    if stack == "zeo":
        return zeo_server(directory)
    return zoe_server(directory, serve_args)


def software(serve_args=()) -> dict:
    """Return what a result records of the software of its runs: the
    commit of the repository and whether its tree differs from it, the
    `zoe` binary with its version and its sha256, the arguments of `zoe
    serve`, the package `zoe` that the native client runs, Python and the
    machine."""
    root = Path(__file__).resolve().parents[4]

    def output(command):
        try:
            done = subprocess.run(
                command, capture_output=True, text=True, timeout=TIMEOUT, check=True
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return done.stdout.strip() or None

    binary = zoe_binary()
    try:
        digest = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    except OSError:
        digest = None
    status = output(["git", "-C", str(root), "status", "--porcelain"])
    try:
        import zoe

        package = str(Path(zoe.__file__).resolve().parent)
    except ImportError:
        package = None
    try:
        from importlib.metadata import version

        zoedb = version("zoedb")
    except Exception:
        zoedb = None
    return {
        "commit": output(["git", "-C", str(root), "rev-parse", "HEAD"]),
        # True when the tree holds changes that the commit does not.
        "dirty": status is not None,
        "zoe": binary,
        "zoe_version": output([binary, "--version"]),
        "zoe_sha256": digest,
        "zoe_serve_args": list(serve_args),
        "zoedb": zoedb,
        "zoe_package": package,
        "python": sys.version.split()[0],
        "machine": f"{platform.system()} {platform.machine()}",
    }


def client(stack: str, address):
    """Open a client of `stack` on `address`."""
    if stack == "zeo":
        from ZEO.ClientStorage import ClientStorage

        return ClientStorage(address, wait_timeout=TIMEOUT)
    from zoe.zeo import ClientStorage

    return ClientStorage(address, native=True, wait_timeout=TIMEOUT)
