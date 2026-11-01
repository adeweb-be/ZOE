"""The bench lock: an exclusive flock(2) on `ZOE_BENCH_LOCK`, by default
`target/bench/bench.lock`, so that one benchmark runs at a time.

A holder of the lock gives the descriptor of its open file to its child
commands in `ZOE_BENCH_LOCK_FD`. A child that finds this descriptor open
on the lock file takes the lock through it: the lock is on the open file,
so the child gets it at once and does not wait for its parent.

    python -m benchcommon.lock

takes the lock for `just bench`: through `ZOE_BENCH_LOCK_FD`, which the
recipe opens on the lock file. The lock stays with the open file of the
recipe when the command exits.
"""

import contextlib
import fcntl
import os
import sys
from collections.abc import Callable, Iterator
from pathlib import Path

from benchcommon.servers import REPO

FD = "ZOE_BENCH_LOCK_FD"


def path() -> Path:
    """Return the lock file: `ZOE_BENCH_LOCK`, else
    `target/bench/bench.lock` of the repository."""
    default = REPO / "target" / "bench" / "bench.lock"
    return Path(os.environ.get("ZOE_BENCH_LOCK") or default)


def inherited(lock: Path | None = None) -> int | None:
    """Return the descriptor of `ZOE_BENCH_LOCK_FD` when it is open on the
    lock file, else None."""
    try:
        fd = int(os.environ.get(FD, ""))
        mine = os.fstat(fd)
        file = os.stat(lock or path())
    except (ValueError, OSError):
        return None
    if (mine.st_dev, mine.st_ino) != (file.st_dev, file.st_ino):
        return None
    return fd


def take(fd: int, lock: Path, log: Callable[[str], object]) -> None:
    """Take an exclusive flock on `fd`. Wait for it, and say so through
    `log`, when another open file holds it."""
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        log(f"wait for the bench lock {lock}")
        fcntl.flock(fd, fcntl.LOCK_EX)


@contextlib.contextmanager
def held(log: Callable[[str], object]) -> Iterator[int]:
    """Hold the bench lock. Yield the descriptor of the lock, which
    `ZOE_BENCH_LOCK_FD` names for the child commands during the hold."""
    lock = path()
    fd = inherited(lock)
    if fd is not None:
        # A parent holds the lock through this open file, and releases it.
        take(fd, lock, log)
        yield fd
        return
    lock.parent.mkdir(parents=True, exist_ok=True)
    with open(lock, "a") as file:
        fd = file.fileno()
        take(fd, lock, log)
        os.set_inheritable(fd, True)
        before = os.environ.get(FD)
        os.environ[FD] = str(fd)
        try:
            yield fd
        finally:
            if before is None:
                os.environ.pop(FD, None)
            else:
                os.environ[FD] = before
            fcntl.flock(fd, fcntl.LOCK_UN)


def pass_fds(env: dict[str, str]) -> tuple[int, ...]:
    """Return the descriptor that a child command with the environment
    `env` needs: the one of `ZOE_BENCH_LOCK_FD`, when this process holds
    it open."""
    try:
        fd = int(env.get(FD, ""))
        os.fstat(fd)
    except (ValueError, OSError):
        return ()
    return (fd,)


def main() -> int:
    """Take the bench lock through the open file of `ZOE_BENCH_LOCK_FD`."""
    lock = path()
    fd = inherited(lock)
    if fd is None:
        print(f"bench lock: {FD} names no open file of {lock}", file=sys.stderr)
        return 2
    take(fd, lock, lambda message: print(f"just bench: {message}", file=sys.stderr))
    return 0


if __name__ == "__main__":
    sys.exit(main())
