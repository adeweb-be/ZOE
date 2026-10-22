# 0006. The targets: CPython 3.12 to 3.14 and 3.14t, ZODB 6 and ZEO 6, on Linux x86-64 and macOS

Status: accepted.

## Context

The wheel, the server binary and Loupe must be built and tested on every target. Free-threaded CPython is officially supported from 3.14; 3.13t is experimental. The first adopting sites run Linux servers.

Plone 6.0 sites still run on the ZODB 5 and ZEO 5 lines. Support for them doubles the conformance matrix and the protocol edge cases.

A developer on a Mac with Apple silicon must edit, check and test the project on that machine, and a result on that machine must count. Without changes, `devenv shell` does not open on macOS: `devenv.nix` evaluates packages that exist only on Linux (`glibcLocales`, `util-linux`, `perf`, `heaptrack`, the musl cross compiler, the GTK and WebKitGTK libraries of Loupe). The crates do not compile on macOS either: the client cache file calls `fallocate` and `posix_fallocate`, and reads the boot id of the kernel from `/proc`.

The specification makes a commit durable with an fsync before the acknowledgement. On Apple systems, `File::sync_all` and `File::sync_data` of the Rust standard library call `fcntl(F_FULLFSYNC)`. It also flushes the write cache of the drive, and on the development laptop it costs a few milliseconds more than `fsync(2)` for each commit. FileStorage, under ZEO, calls `os.fsync`, which is `fsync(2)`. SQLite and PostgreSQL also call `fsync(2)` on macOS by default. The phase 3 gate compares a commit of ZOE with a commit of ZEO. On the development laptop, with no sync at all, ZOE wins every commit cell of the gate (1.45× to 2.48×). With `F_FULLFSYNC`, it loses the commit cells at concurrency 1 (0.48× to 0.74×).

## Decision

### Python

The targets are CPython 3.12, 3.13 and 3.14, plus the free-threaded 3.14t build. The extension module declares that it does not use the GIL. 3.13t is not a target. 3.15 joins when it ships.

### ZODB, ZEO and Plone

ZOE targets ZODB 6.0 or later and ZEO 6.0 or later, which means Plone 6.1 or later.

### Platforms

**Linux x86-64 and macOS (aarch64-darwin) are the targets.** ZOE runs on both, and a green run on either is evidence, for a pull request and for a gate. The release builds for macOS and Windows (the wheels, the server binary and the Loupe packages) come in a later phase of N11.

### What macOS changes

- `devenv.nix` puts each package and variable that exists only on Linux behind `pkgs.stdenv.isLinux`. On Linux, the shell is the same.
- The client cache file of `zoe-client` keeps its semantics on macOS. `posix_fallocate` becomes a write of zeros into the holes of the file, found with `SEEK_HOLE` and `SEEK_DATA`; the data of the file does not change. The zeroing in place with `fallocate` becomes a write of zeros. The boot id of the kernel is the boot session UUID (`sysctl kern.bootsessionuuid`).
- The lingering close of `zoe serve` reads the send queue of a socket with `SIOCOUTQ` on Linux. On macOS, it reads the socket option `SO_NWRITE`: for TCP, the bytes that the kernel did not send and the bytes that the peer did not acknowledge, without the FIN; for a unix domain socket, always 0, because the kernel moves the bytes into the buffer of the peer at once, and the peer can read them after the close.
- The tests that read the memory, the threads or the CPU of a process in `/proc` run on Linux only. They check bounds that were measured on Linux; macOS gives other numbers with another meaning. On macOS, they are ignored with the reason "Linux only".
- Five more tests check a behaviour of Linux, not of ZOE, and are ignored on macOS with their reason: the count of a unix domain socket in the send queue; a sparse copy of a cache file, which APFS does not make; two file names that differ only in case, which APFS keeps apart only on a case-sensitive volume; the end of a connection after a lingering close, which macOS reports as a reset when the client writes after the FIN, also when it got every frame; and a client that reads nothing, whose replies can all fit in the socket buffers of macOS, so the hub queue does not always fill.

### A sync on macOS is fsync(2)

On Apple systems, every sync of `OsVfs` calls `fsync(2)`: the sync of the database file, of the other files and of the directories. One function of `crates/zoe-core/src/vfs/os.rs` makes it, with one `unsafe` call to `libc::fsync` and its safety comment. On the other systems, a sync is `File::sync_all`, and the sync of the database file is `File::sync_data`.

## Consequences

- One protocol version and one set of conformance suites.
- A site on Plone 6.0 upgrades Plone before it moves to ZOE.
- The CI matrix has four interpreters and one platform: CI runs on Linux.
- Free-threaded parallel commits in one process also need `persistent`, `BTrees` and `zodbpickle` to declare free-threading support upstream.
- `just check`, the Rust tests, the Python tests, the benchmarks, `just tsan` and `just fuzz` run on macOS, and their results count. `just tsan` and `just fuzz` build for the target of the host. `just release binary` builds the static Linux binary and needs Linux, and so do the profiles `profiling` and `loupe`. `just release` builds in the Linux containers of Dagger.
- On macOS, libFuzzer does not give the symbol `__sancov_lowest_stack`, so `just fuzz` traces the depth of the stack on Linux only. A ThreadSanitizer report on macOS names the functions by their v0 symbols, so `just tsan` finds the canary test by its name or by its symbol.
- The fuzz target `handshake` connects to its server over a unix domain socket. Over TCP, each closed connection keeps its port for 30 seconds on macOS, and a run of 30 seconds uses more connections than the 16,384 ports of macOS.
- A run on macOS does not run the tests that are Linux only. CI runs on Linux and runs them.
- New code that calls an interface of Linux only gives a fallback for macOS, or runs on Linux only, behind `cfg(target_os = "linux")`.
- On macOS, ZOE makes the same promise as FileStorage: a commit survives a crash of the process and of the system, and a loss of power can lose the commits that the drive cache held. Linux keeps `File::sync_all` and `File::sync_data`.
- `OsVfs::File` is `OsFile`, a type of `zoe-core`, not `redb::backends::FileBackend`. The tests that wrap the file of an `OsVfs` name that type.
- The gate on the development laptop compares the two storages with the same sync.

## Alternatives

- **ZODB 5 and ZEO 5 too.** It doubles the conformance matrix and the protocol edge cases.
- **Keep `F_FULLFSYNC` on macOS.** The commit cells of the gate then fail on macOS for a cost that ZEO does not pay, and only a run on Linux could pass the gate.
- **Run the gate in a Linux container on macOS.** The virtual disk of the container has syncs of its own, which neither storage has in production.
- **A setting for the sync.** The specification names no such setting.

## Merged ADRs

- ADR 0005, ZODB 6 and ZEO 6 only: Decision, "ZODB, ZEO and Plone".
- ADR 0038, macOS is a target: Decision, "Platforms" and "What macOS changes"; its consequences are in Consequences.
- ADR 0040, a sync on macOS is fsync(2): Decision, "A sync on macOS is fsync(2)"; its alternatives are in Alternatives.
