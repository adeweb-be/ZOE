# 0040. A sync on macOS is fsync(2), as for FileStorage

Status: accepted. The human chose it while the phase 3 gate ran on the development laptop (ZOE-201).

## Context

The specification makes a commit durable with an fsync before the acknowledgement. ADR 0038 lets macOS open the development shell and run the tests and the benchmarks, but macOS is not a target.

On Apple systems, `File::sync_all` and `File::sync_data` of the Rust standard library call `fcntl(F_FULLFSYNC)`. It also flushes the write cache of the drive, and on the development laptop it costs a few milliseconds more than `fsync(2)` for each commit. FileStorage, under ZEO, calls `os.fsync`, which is `fsync(2)`. SQLite and PostgreSQL also call `fsync(2)` on macOS by default.

The phase 3 gate compares a commit of ZOE with a commit of ZEO. On macOS, ZOE then pays for a flush of the drive cache that ZEO does not make. With no sync at all, ZOE wins every commit cell of the gate (1.45× to 2.48×). With `F_FULLFSYNC`, it loses the commit cells at concurrency 1 (0.48× to 0.74×).

## Decision

On Apple systems, every sync of `OsVfs` calls `fsync(2)`: the sync of the database file, of the other files and of the directories. One function of `crates/zoe-core/src/vfs.rs` makes it, with one `unsafe` call to `libc::fsync` and its safety comment. On the other systems, a sync stays `File::sync_all`, and the sync of the database file stays `File::sync_data`.

## Consequences

- On macOS, ZOE makes the same promise as FileStorage: a commit survives a crash of the process and of the system, and a loss of power can lose the commits that the drive cache held. A store on macOS is a store for development.
- Linux, the target, does not change.
- `OsVfs::File` is `OsFile`, a type of `zoe-core`, and no longer `redb::backends::FileBackend`. The tests that wrap the file of an `OsVfs` name the new type.
- The gate on the development laptop compares the two storages with the same sync.

## Alternatives

- **Keep `F_FULLFSYNC`.** The commit cells of the gate then fail on macOS for a cost that ZEO does not pay, and only a run on Linux could pass the gate.
- **Run the gate in a Linux container on macOS.** The virtual disk of the container has syncs of its own, which neither storage has in production.
- **A setting for the sync.** The specification names no such setting, and macOS is not a target.
