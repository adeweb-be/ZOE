"""The console script of the wheel has no server.

The extension never links tokio, so `zoe serve` through the console script
fails with 1, names the `zoe` binary and creates nothing.
"""

import os
import subprocess
import sysconfig
from pathlib import Path


def test_the_console_script_refuses_to_serve(tmp_path):
    script = Path(sysconfig.get_path("scripts")) / "zoe"
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    store = tmp_path / "store"
    result = subprocess.run(
        [str(script), "serve", "--path", str(store), "--listen", "127.0.0.1:0"],
        env=environ,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 1, result.stdout + result.stderr
    assert result.stdout == ""
    assert "`zoe` binary" in result.stderr
    assert not store.exists()
