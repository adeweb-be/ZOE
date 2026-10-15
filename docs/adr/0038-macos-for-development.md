# 0038. macOS opens the development shell

Status: accepted. The human decided it.

## Context

Linux x86-64 is the only target of the first phases (N11). macOS and Windows come later, for the wheels, the server binary and Loupe. Until now, `devenv shell` did not open on macOS: `devenv.nix` evaluated packages that exist only on Linux (`glibcLocales`, `util-linux`, `perf`, `heaptrack`, the musl cross compiler, the GTK and WebKitGTK libraries of Loupe). The crates did not compile on macOS either: the client cache file called `fallocate` and `posix_fallocate`, and read the boot id of the kernel from `/proc`.

A developer on a Mac with Apple silicon could then not edit, check or test the project on that machine.

## Decision

**macOS (aarch64-darwin) opens the development shell.** It is a platform for development, not a target. N11 and the targets of the specification do not change, and CI and the release builds stay on Linux.

- `devenv.nix` puts each package and variable that exists only on Linux behind `pkgs.stdenv.isLinux`. On Linux, the shell is the same as before.
- The client cache file of `zoe-client` keeps its semantics on macOS. `posix_fallocate` becomes a write of zeros into the holes of the file, found with `SEEK_HOLE` and `SEEK_DATA`; the data of the file does not change. The zeroing in place with `fallocate` becomes a write of zeros. The boot id of the kernel is the boot session UUID (`sysctl kern.bootsessionuuid`).
- The lingering close of `zoe serve` reads the send queue of a socket with `SIOCOUTQ` on Linux. On macOS, it reads the socket option `SO_NWRITE`: for TCP, the bytes that the kernel did not send and the bytes that the peer did not acknowledge, without the FIN; for a unix domain socket, always 0, because the kernel moves the bytes into the buffer of the peer at once, and the peer can read them after the close.
- The tests that read the memory, the threads or the CPU of a process in `/proc` run on Linux only. They check bounds that were measured on Linux; macOS gives other numbers with another meaning. On macOS, they are ignored with the reason "Linux only".
- Five more tests check a behaviour of Linux, not of ZOE, and are ignored on macOS with their reason: the count of a unix domain socket in the send queue; a sparse copy of a cache file, which APFS does not make; two file names that differ only in case, which APFS keeps apart only on a case-sensitive volume; the end of a connection after a lingering close, which macOS reports as a reset when the client writes after the FIN, also when it got every frame; and a client that reads nothing, whose replies can all fit in the socket buffers of macOS, so the hub queue does not always fill.

## Consequences

- `just check` and the Rust tests run on macOS. `just tsan`, `just fuzz`, `just release` and the profiles `profiling` and `loupe` still need Linux, as do the benchmark numbers.
- A green run on macOS is not evidence for a pull request. CI on Linux stays the evidence.
- New code that calls an interface of Linux only gives a fallback for macOS, or runs on Linux only, behind `cfg(target_os = "linux")`.
