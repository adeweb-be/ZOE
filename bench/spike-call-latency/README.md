# Spike ZOE-188: scripts and switches

Prototypes and scripts of the call-latency spike. Nothing here merges.

Switches:

- `SPIKE_ZEO_FULLFSYNC=1`: on macOS, the ZEO server of the harness syncs with `F_FULLFSYNC`, as `zoe serve` does.
- `ZOE_SPIKE_INLINE_LOAD=1`: `zoe serve` runs each `loadBefore` on the task of the reader, with no thread of the read pool.
- `ZOE_SPIKE_EAGER_ADDR=1`: the native client looks up the canonical name of the server at the connection, as ZEO does, and not at the first `sortKey()`.
- `ZOE_SPIKE_TRACE=PREFIX`: the probes of `zoe-client`, `zoe-py` and `zoe serve` write `PREFIX.<role>.<pid>`; with `trace/` on `PYTHONPATH`, so do the probes of the Python side and of ZEO.
- The harness passes each `ZOE_SPIKE_*` variable to `zoe serve` and to the pyperf workers.

Scripts (run with the venv of the harness, `uv run --project bench/shootout python ...`):

- `phases.py STORAGE SIZE LOOPS SCENARIO...`: the phases of each scenario at threads 1, and the calls of the storage.
- `shoot.py`: one value of zodbshootout as the harness runs it; with `probe/` on `PYTHONPATH`, `split.py` splits each `read_warm` sample.
- `terms.py PREFIX STORAGE`: the terms of each `loadBefore` of a trace.
- `addr.py`: when the native client looks up the name of the server.
- `rtt.py SIZE`: the loopback round trip with blocking sockets.
- `fsync.py DIR full|plain`: one durable write of 4 KiB.
