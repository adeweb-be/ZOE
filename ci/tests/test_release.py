"""Tests of the Dagger function `release`.

`release` builds the static `zoe` binary and the server image. The tag of
the image is the version of the workspace, so the git tag of a release must
be `v` and that version. Run them inside `devenv shell`, with a Dagger
engine: `just release test`.
"""

import subprocess
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
VERSION = tomllib.loads((REPO / "Cargo.toml").read_text())["workspace"]["package"][
    "version"
]


def dagger(*args):
    return subprocess.run(
        ["dagger", "call", "--mod", str(REPO / "ci"), *args],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )


def test_the_release_holds_the_binary_and_the_image():
    result = dagger("release", "--tag", f"v{VERSION}", "entries")
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    assert set(result.stdout.split()) == {
        f"zoe-{VERSION}-x86_64-unknown-linux-musl",
        f"zoe-{VERSION}-image.tar",
    }


def test_a_tag_that_is_not_the_version_fails_the_release():
    result = dagger("release", "--tag", f"v{VERSION}.1", "entries")
    assert result.returncode != 0
    assert f"is not v{VERSION}" in result.stdout + result.stderr
