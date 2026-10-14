"""Write the seeds of the fuzz targets of zoe serve.

Run it from the root of the repository:

    python3 crates/zoe-server/fuzz/corpus/generate.py

A seed is the byte stream of one client connection of the wire traces in
docs/spikes/zeo-wire: the protocol of the client, then its messages, each
one as a frame (a 4-byte big-endian length, then the bytes). The script
keeps the frames of a stream up to 4 KiB and cuts it before the first frame
that does not fit. It names the seed `<trace>-<connection>`, and it skips a
connection that sends only its protocol. Each target also gets `ruok`, the
status request in place of the protocol. The seeds of `frame` and
`handshake` are the same. All data is synthetic.
"""

import json
import struct
from pathlib import Path

HERE = Path(__file__).resolve().parent
TRACES = HERE.parents[3] / "docs" / "spikes" / "zeo-wire"
TARGETS = ("frame", "handshake")
LIMIT = 4096


def frame(payload: bytes) -> bytes:
    """Return the frame of `payload`."""
    return struct.pack(">I", len(payload)) + payload


def streams():
    """Yield the name and the frames of each client connection."""
    for path in sorted(TRACES.glob("*.jsonl")):
        connections = {}
        for line in path.read_text().splitlines():
            record = json.loads(line)
            if record["direction"] == "client":
                payload = bytes.fromhex(record["raw"])
                connections.setdefault(record["connection"], []).append(payload)
        for connection, payloads in sorted(connections.items()):
            yield f"{path.stem}-{connection}", [frame(p) for p in payloads]


def main():
    seeds = {"ruok": frame(b"ruok")}
    for name, frames in streams():
        if len(frames) < 2:
            continue
        data = b""
        for f in frames:
            if len(data) + len(f) > LIMIT:
                break
            data += f
        seeds[name] = data
    for target in TARGETS:
        directory = HERE / target
        directory.mkdir(exist_ok=True)
        for old in directory.iterdir():
            old.unlink()
        for name, data in seeds.items():
            (directory / name).write_bytes(data)


if __name__ == "__main__":
    main()
