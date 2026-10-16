# 0038. macOS is a target

Status: accepted. The human decided it.

## Context

Linux x86-64 was the only target of the first phases (N11), with macOS and Windows later, for the wheels, the server binary and Loupe. `devenv shell` did not open on macOS: `devenv.nix` evaluated packages that exist only on Linux (`glibcLocales`, `util-linux`, `perf`, `heaptrack`, the musl cross compiler, the GTK and WebKitGTK libraries of Loupe). The crates did not compile on macOS either: the client cache file called `fallocate` and `posix_fallocate`, and read the boot id of the kernel from `/proc`.

A developer on a Mac with Apple silicon could then not edit, check or test the project on that machine, and a result on that machine did not count.

## Decision

**macOS (aarch64-darwin) is a target, beside Linux x86-64.** ZOE runs on it, and a green run on it is evidence, for a pull request and for a gate.

- `devenv.nix` puts each package and variable that exists only on Linux behind `pkgs.stdenv.isLinux`. On Linux, the shell is the same.
- The client cache file of `zoe-client` keeps its semantics on macOS. `posix_fallocate` becomes a write of zeros into the holes of the file, found with `SEEK_HOLE` and `SEEK_DATA`; the data of the file does not change. The zeroing in place with `fallocate` becomes a write of zeros. The boot id of the kernel is the boot session UUID (`sysctl kern.bootsessionuuid`).
- The lingering close of `zoe serve` reads the send queue of a socket with `SIOCOUTQ` on Linux. On macOS, it reads the socket option `SO_NWRITE`: for TCP, the bytes that the kernel did not send and the bytes that the peer did not acknowledge, without the FIN; for a unix domain socket, always 0, because the kernel moves the bytes into the buffer of the peer at once, and the peer can read them after the close.
- The tests that read the memory, the threads or the CPU of a process in `/proc` run on Linux only. They check bounds that were measured on Linux; macOS gives other numbers with another meaning. On macOS, they are ignored with the reason "Linux only".
- Five more tests check a behaviour of Linux, not of ZOE, and are ignored on macOS with their reason: the count of a unix domain socket in the send queue; a sparse copy of a cache file, which APFS does not make; two file names that differ only in case, which APFS keeps apart only on a case-sensitive volume; the end of a connection after a lingering close, which macOS reports as a reset when the client writes after the FIN, also when it got every frame; and a client that reads nothing, whose replies can all fit in the socket buffers of macOS, so the hub queue does not always fill.

## Consequences

- `just check`, the Rust tests, the Python tests, the benchmarks, `just tsan` and `just fuzz` run on macOS, and their results count. `just tsan` and `just fuzz` build for the target of the host. `just release binary` builds the static Linux binary and needs Linux, and so do the profiles `profiling` and `loupe`. `just release` builds in the Linux containers of Dagger.
- On macOS, libFuzzer does not give the symbol `__sancov_lowest_stack`, so `just fuzz` traces the depth of the stack on Linux only. A ThreadSanitizer report on macOS names the functions by their v0 symbols, so `just tsan` finds the canary test by its name or by its symbol.
- The fuzz target `handshake` connects to its server over a unix domain socket. Over TCP, each closed connection keeps its port for 30 seconds on macOS, and a run of 30 seconds uses more connections than the 16,384 ports of macOS.
- A run on macOS does not run the tests that are Linux only. CI runs on Linux and runs them.
- The release builds for macOS (the wheels, the server binary and the Loupe packages) come in the later phase of N11, with Windows.
- New code that calls an interface of Linux only gives a fallback for macOS, or runs on Linux only, behind `cfg(target_os = "linux")`.
