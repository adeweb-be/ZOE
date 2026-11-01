"""The servers and the clients of a run: a ZEO 6 server on a FileStorage
with `ZEO.ClientStorage`, and `zoe serve` with `zoe.zeo.ClientStorage`, the
native client."""

import contextlib
import hashlib
import os
import platform
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

from benchcommon import provenance
from benchcommon import servers as common

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
    environ = common.scrubbed()
    environ["PYTHONPATH"] = os.pathsep.join(p for p in sys.path if p)
    return environ


@contextlib.contextmanager
def zeo_server(directory: Path) -> Iterator[tuple[str, int]]:
    """Run a ZEO server on a new FileStorage in directory. Yield its
    address."""
    program = ("-c", RUNZEO)
    with common.zeo_server(directory, program, child_environment(), TIMEOUT) as port:
        yield ("127.0.0.1", port)


@contextlib.contextmanager
def zoe_server(directory: Path, args=()) -> Iterator[tuple[str, int]]:
    """Run `zoe serve` on a new store in directory, with `args` after its
    own arguments. Yield its address."""
    with common.zoe_serve(directory, tuple(args), TIMEOUT) as address:
        yield common.host_and_port(address)


def server(stack: str, directory: Path, serve_args=()):
    """Return the context manager of the server of `stack`. `serve_args` go
    to `zoe serve`."""
    if stack == "zeo":
        return zeo_server(directory)
    return zoe_server(directory, serve_args)


def software(serve_args=()) -> dict:
    """Return what a result records of the software of its runs: the
    commit and whether the tree differs from it, as `benchcommon.provenance`
    gives them, the `zoe` binary with its version and its sha256, the
    arguments of `zoe serve`, the package `zoe` that the native client runs
    with the sha256 of its extension, Python and the machine."""

    def output(command):
        try:
            done = subprocess.run(
                command, capture_output=True, text=True, timeout=TIMEOUT, check=True
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return done.stdout.strip()

    def sha256(path):
        try:
            return hashlib.sha256(Path(path).read_bytes()).hexdigest()
        except OSError:
            return None

    binary = str(common.zoe_path())
    try:
        import zoe
        import zoe._zoe

        package = Path(zoe.__file__).resolve().parent
        # Hash the extension that this interpreter loaded. The package can
        # also hold the extensions of other versions of Python.
        extension = getattr(zoe._zoe, "__file__", None)
    except ImportError:
        package = extension = None
    try:
        from importlib.metadata import version

        zoedb = version("zoedb")
    except Exception:
        zoedb = None
    return {
        "commit": provenance.commit(),
        "dirty": provenance.dirty(),
        "zoe": binary,
        "zoe_version": output([binary, "--version"]) or None,
        "zoe_sha256": sha256(binary),
        "zoe_serve_args": list(serve_args),
        "zoedb": zoedb,
        "zoe_package": str(package) if package else None,
        "zoe_extension_sha256": sha256(extension) if extension else None,
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
