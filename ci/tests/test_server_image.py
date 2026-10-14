"""Acceptance test of the static `zoe` binary and of the server image (N11).

Two Dagger functions build what Tier 1 and the bare-metal deployment run:

- `zoe-binary` builds the `zoe` binary, with `serve`, for Linux x86-64,
  statically linked: it needs no C library on the host. It returns the
  file.
- `server-image` builds the container image `ghcr.io/adeweb-be/zoe` from
  that binary and nothing else: no shell, no package manager, no Python.
  Its entrypoint is `zoe`, its default command `serve`, its user is not
  root, and it declares the ZEO port 8100 and the admin port 8101. It
  returns the image, which `export` writes as an OCI tarball.

`just release` calls both. Publishing the image is the release workflow,
which only the human changes.

Run it inside `devenv shell`, with a Dagger engine:
`uv run --directory ci pytest tests/test_server_image.py`.
"""

import json
import struct
import subprocess
import tarfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PT_INTERP = 3
PT_DYNAMIC = 2


def dagger(*args):
    result = subprocess.run(
        ["dagger", "call", "--mod", str(REPO / "ci"), *args],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout[-5000:] + result.stderr[-5000:]
    return result


def program_headers(path):
    """Return the types of the program headers of an ELF64 file."""
    data = path.read_bytes()
    assert data[:4] == b"\x7fELF", "not an ELF file"
    assert data[4] == 2, "not a 64-bit ELF file"
    phoff = struct.unpack_from("<Q", data, 0x20)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    return [
        struct.unpack_from("<I", data, phoff + i * phentsize)[0] for i in range(phnum)
    ]


def test_the_binary_is_static_and_runs(tmp_path):
    binary = tmp_path / "zoe"
    dagger("zoe-binary", "export", "--path", str(binary))
    headers = program_headers(binary)
    assert PT_INTERP not in headers, "the binary asks for a dynamic loader"
    binary.chmod(0o755)
    result = subprocess.run(
        [str(binary), "serve", "--help"], capture_output=True, text=True
    )
    assert result.returncode == 0
    assert "--listen" in result.stdout


def test_the_image_holds_only_the_binary(tmp_path):
    tarball = tmp_path / "image.tar"
    dagger("server-image", "export", "--path", str(tarball))
    with tarfile.open(tarball) as image:
        index = json.load(image.extractfile("index.json"))
        manifest_digest = index["manifests"][0]["digest"].split(":", 1)[1]
        manifest = json.load(image.extractfile(f"blobs/sha256/{manifest_digest}"))
        config_digest = manifest["config"]["digest"].split(":", 1)[1]
        config = json.load(image.extractfile(f"blobs/sha256/{config_digest}"))
        files = set()
        for layer in manifest["layers"]:
            digest = layer["digest"].split(":", 1)[1]
            with tarfile.open(
                fileobj=image.extractfile(f"blobs/sha256/{digest}")
            ) as tar:
                files |= {m.name.lstrip("./") for m in tar.getmembers() if m.isfile()}

    settings = config["config"]
    assert settings["Entrypoint"][-1].endswith("zoe")
    assert settings.get("Cmd") == ["serve"]
    assert settings.get("User") not in (None, "", "0", "root", "0:0")
    assert {"8100/tcp", "8101/tcp"} <= set(settings.get("ExposedPorts", {}))
    # No shell, no interpreter: the binary and what a static binary needs.
    for forbidden in ("bin/sh", "usr/bin/python3", "bin/bash"):
        assert forbidden not in files, forbidden
    assert any(name.endswith("zoe") for name in files)
