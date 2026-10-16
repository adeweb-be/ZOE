"""Run one zodbshootout value of one scenario as the harness does.

Usage: python shoot.py STORAGE SIZE CONCURRENCY MODE SCENARIO OUTDIR
"""

import os
import subprocess
import sys
import tempfile
from pathlib import Path

from shootout.storages import STORAGES, zconfig

name, size, conc, mode, bench, out = sys.argv[1:7]
out = Path(out)
out.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(dir=os.environ.get("SPIKE_DIR")) as d:
    d = Path(d)
    with zconfig(STORAGES[name], d, None) as text:
        (d / "zodb.conf").write_text(text)
        args = [sys.executable, "-m", "zodbshootout", "--processes", "1", "--values", "1",
                "--warmups", "1", "--loops", "1", "--object-counts", "100",
                "--object-size", size, "--concurrency", conc, "--include-mapping", "no",
                "--quiet", "--output", str(out / "pyperf.json")]
        if bench == "add":
            args += ["--zap", "force"]
        if mode == "threads":
            args += ["--threads", "shared"]
        args += ["--inherit-environ", "PYTHONPATH,SPIKE_PROBE_OUT", str(d / "zodb.conf"), bench]
        env = dict(os.environ, SPIKE_PROBE_OUT=str(out))
        done = subprocess.run(args, env=env, capture_output=True, text=True)
        (out / "log.txt").write_text(done.stdout + done.stderr)
        print("exit", done.returncode)
