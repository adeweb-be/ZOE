# 0031. The in-process storage closes its stores on a signal

Status: accepted.

## Context

Docker, systemd and supervisord stop a Zope instance with SIGTERM, and a closed terminal sends SIGHUP. Zope and waitress install no handler of these signals, so the process ends before the drop of `zoe.Storage` runs. redb clears its recovery flag only at a close, so each routine stop leaves the store unclean, and a read-only open needs a repair (ADR 0028). SIGINT raises `KeyboardInterrupt`, and the end of the interpreter closes the store. A signal handler is a policy of the whole process. A handler in Python runs only on the main thread, between two bytecodes.

## Decision

The behaviour: specification, "In-process mode", "Stop by a signal". The rules below complete it.

**The close lives in zoe-py, on the Rust side.** zoe-core knows no signal. The extension uses signal-hook, and its handler only writes to a pipe.

**The install.** The extension installs a handler only when `signal.getsignal` gives `signal.SIG_DFL`. A return of `None` counts as not the default. The check reads the view of Python, so zoe-py has no `unsafe` code and no `libc`. A later open checks again each signal that has no handler of the extension yet.

**The watcher.** The first install registers the at-fork hook (`os.register_at_fork`) and starts the thread `zoe-signals`. The watcher never attaches to the interpreter: it runs no Python code, takes no GIL and drops no Python reference. So it works on CPython 3.14t and while a thread holds the GIL.

**The close at a signal.** The watcher marks each storage closed as `close` does, so each later call raises the error of a closed storage.

**The end.** The watcher restores the default action and raises the signal again (`signal_hook::low_level::emulate_default_handler`), also when a close fails or panics. A second signal during the close changes nothing.

**One close per store.** The storage shares the store and its transaction table with the watcher. The process keeps weak references to these parts. The store closes once: in `close`, in the drop or in the watcher, whichever comes first. When `close` or the drop came first, the watcher raises the signal only after that close ends, or at the bound. The watcher takes the core transaction out of a table entry and leaves the entry, because the entry holds Python references.

**Fork.** A child inherits the handler but not the watcher, and the pipe is the one of the parent. The child does not install the handlers again.

**The bound of 5 seconds.** It stays inside the 10 seconds that Docker and supervisord wait before SIGKILL. No option and no ZConfig key turns the handlers off or sets the bound. An application that wants another policy sets its own handler.

## Consequences

- A routine stop leaves each store closed cleanly.
- `os.fork()` after the open of a store gives a `DeprecationWarning` on CPython 3.12 and later. Such a fork is unsafe for the store anyway.
- The check does not see a handler that C code installs without Python. signal-hook then calls it before the watcher closes the stores.
- On a free-threaded build, a handler that the main thread sets between the check and the install does not stop the close.
- A call that does not return, such as a running pack, leaves its store unclean.
- The watcher does not wait for a read through a weak handle of the core: an iterator of `_iterator` or a blob file. The process can end while such a read holds the database, and the store stays unclean. A fix needs an API of the core.
- A store that opens after the watcher took its list is not closed.
- A `close` or a drop after the watcher took the store does not wait for its close. A normal exit that races the signal can end the process during that close.
- The repair on open (ADR 0028) covers each store that these cases leave unclean.
- A child of `os.fork()` that opens a store gets no handler.
- The writer commands of the console script (`zoe pack`, `zoe convert --from-data-fs`, `zoe restore`) open no `zoe.Storage`, so a signal stops them at once.

## Alternatives

- **The close in zoe-core.** The CLI, Loupe and `zoe serve` each have their own signal policy.
- **A handler in Python.** A long call on the main thread delays it, and a thread that holds the GIL stops it.
- **A save of the allocator state at each commit.** Each commit writes more. The human declined it.
- **The check with `sigaction`.** It needs `unsafe` code and `libc` in zoe-py.
