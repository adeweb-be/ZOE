"""The code that every benchmark harness of bench/ shares.

- `servers`: a free port, the `zoe` binary, the environment of a server
  without `ZOE_*`, a ZEO server and `zoe serve` on a new store, and the
  stop of a process: SIGTERM, then a wait, then SIGKILL.
- `provenance`: the CPU model, the name of the machine, the commit and
  whether the tree differs from it (ADR 0054).
- `lock`: the bench lock, so that one benchmark runs at a time.
- `files`: the atomic write of a JSON file.
- `pins`: the check that the harnesses pin the forks of python/.
- `zoebench`: `zoe-bench` against `zoe serve` on a new store, for
  `just bench load` and `just bench saturate`.

The package uses the standard library only.
"""
