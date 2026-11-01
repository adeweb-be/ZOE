"""`just bench load` and `just bench saturate`: `zoe-bench` against
`zoe serve` on a new store.

    python -m benchcommon.zoebench ZOE_BENCH WORK OUT COMMAND [OPTIONS]

starts `zoe serve` (`ZOE_BINARY`) on a new store in WORK, runs
`ZOE_BENCH COMMAND --server <address> OPTIONS`, and writes its stdout, the
JSON result, to stdout and to OUT/<COMMAND>-<started>.json. A run that
fails writes no result. The server gets no `ZOE_*` variable of the
caller; `zoe-bench` gets `ZOE_BENCH_COMMIT`, else the commit of the
repository.
"""

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from benchcommon import files, provenance, servers


def run(zoe_bench: str, work: Path, out: Path, command: str, args: list[str]) -> Path:
    """Run `zoe-bench COMMAND` against a new `zoe serve`. Return the path of
    the result."""
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    env = dict(os.environ, ZOE_BENCH_COMMIT=provenance.commit() or "")
    with servers.zoe_serve(work) as address:
        done = subprocess.run(
            [zoe_bench, command, "--server", address, *args],
            stdout=subprocess.PIPE,
            env=env,
            check=True,
        )
    result = out / f"{command}-{stamp}.json"
    files.write_bytes(result, done.stdout)
    return result


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 4:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    zoe_bench, work, out, command, *args = argv
    try:
        result = run(zoe_bench, Path(work), Path(out), command, args)
    except subprocess.CalledProcessError as error:
        print(f"just bench: zoe-bench exited with {error.returncode}", file=sys.stderr)
        return 1
    except (OSError, RuntimeError) as error:
        print(f"just bench: {error}", file=sys.stderr)
        return 1
    sys.stdout.buffer.write(result.read_bytes())
    return 0


if __name__ == "__main__":
    sys.exit(main())
