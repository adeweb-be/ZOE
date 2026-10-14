# 0031. The in-process storage closes its stores on a signal

Status: accepted. The human decided the design on the issue of the stores that a stop leaves unclean (#188). The card settles its details.

## Context

A process that opens stores with `zoe.Storage` is often a Zope instance. Docker, systemd and supervisord stop it with SIGTERM, and a closed terminal stops it with SIGHUP. Zope, waitress and plone.recipe.zope2instance install no handler of these signals. So the default action ends the process before Python finalizes, and the drop of the storage never runs. redb writes its allocator state and clears its recovery flag only when the database closes. So each routine stop leaves the store unclean: a read-only open, as Loupe, `zoe mcp` and `zoe verify` do, refuses it with `NotClosedCleanly`, and only a writer open repairs it.

SIGINT does not have this problem: Python raises `KeyboardInterrupt`, and the drop at the end of the interpreter closes the store. `zoe serve` has its own handling of SIGTERM and SIGINT.

A signal handler is a policy of the whole process, and the application can have its own. A handler in Python runs only on the main thread, between two bytecodes. A main thread that waits in a long call delays it, and a thread that holds the GIL in C code stops it.

## Decision

**The close lives in zoe-py, on the Rust side.** zoe-core knows no signal: `zoe serve`, the CLI and Loupe share it, and `zoe serve` has its own handling. The extension uses signal-hook. Its handler only writes to a pipe, which is safe in a signal handler.

**The install.** When `zoe.Storage` opens a store, the extension installs a handler of SIGTERM and one of SIGHUP. A read-only storage installs them too, because it opens the store for writes. The extension installs a handler only when `signal.getsignal` gives `signal.SIG_DFL` for that signal. A return of `None`, a handler that Python did not install, counts as not the default. So an ignored signal, as `nohup` sets SIGHUP, and a handler of the application keep their action. The check reads the view of Python, so zoe-py has no `unsafe` code and no `libc`. The extension installs each handler at most once per process. A later open checks again a signal that has no handler of the extension yet.

**The watcher.** The first install registers an at-fork hook and starts one watcher thread per process. The watcher reads the pipe. It never attaches to the interpreter: it runs no Python code, takes no GIL and drops no Python reference. So it works the same on CPython 3.14t and while a thread holds the GIL.

**The close at a signal.** The watcher closes each open `zoe.Storage` of the process: a store for each mount point. It marks each storage closed as `close` does, so `tpc_begin` and each later call raise the error that they raise after `close`. It aborts each transaction that no call uses. It waits until no call holds the store, then it drops the store in the watcher thread. The drop closes the redb database cleanly and releases the locks of the store. A `tpc_finish` whose commit started before the mark commits, and the commit is durable before the database closes. A commit whose `tpc_finish` returned stays. The wait lasts at most 5 seconds from the signal, for all the storages together. A store that a call still holds at the bound stays open, and the others close.

**The end.** After the close, or at the bound, the watcher restores the default action of the signal and raises the signal again (`signal_hook::low_level::emulate_default_handler`). So the process ends with the status of the signal, as without the handler: -15 for SIGTERM, -1 for SIGHUP. This holds also when no storage is open, when a close fails and when a close panics. A second signal during the close changes nothing.

**One close per store.** The store and the table of the transactions live in a part that the storage shares with the watcher. The process keeps a list of weak references to these parts. The store closes once: in `close`, in the drop of the storage or in the watcher, whichever takes it first. A flag tells when the database is closed. So when `close` or the drop took the store first, the watcher raises the signal only after that close ends, or at the bound. The entries of the table hold Python references: the watcher takes the transaction of the core out of an entry and leaves the entry, and `close` or the drop removes it.

**The handler of the application wins.** A handler that the application sets later with `signal.signal` replaces the handler of the extension, and the extension never puts its handler back. The application then decides what the signal does, and the drop at the end of the interpreter closes the store, as before.

**Fork.** A child of `os.fork()` inherits the handler but not the watcher thread, and the pipe of the handler is the pipe of the parent. So the at-fork hook (`os.register_at_fork`) gives the child the default action of each signal of the extension whose action Python still sees as `SIG_DFL`. A handler of the application stays in the child. The child does not install the handlers again.

**The bound of 5 seconds.** It stays inside the 10 seconds that Docker and supervisord wait before they send SIGKILL. A `tpc_finish` takes milliseconds: the bound only stops a call that does not return, such as a long pack. No option and no ZConfig key turns the handlers off or sets the bound: an application that wants another policy sets its own handler.

## Alternatives

- **The close in zoe-core.** The CLI, Loupe and `zoe serve` share the core, and each has its own policy for signals. A handler in the core would change them all.
- **A handler in Python.** It runs only on the main thread, between two bytecodes. A long call on the main thread delays it, and a thread that holds the GIL stops it.
- **A save of the allocator state at each commit**, the quick repair of redb. Each commit would write more. The human declined it.
- **The check with `sigaction`.** It would also see a handler that C code installs without Python. It needs `unsafe` code and `libc` in zoe-py.

## Consequences

- A routine stop of an instance on `<zoe>` leaves each store closed cleanly. Loupe, `zoe mcp` and `zoe verify` open it with no repair.
- Each process that opens a store has one more thread, `zoe-signals`. On CPython 3.12 and later, `os.fork()` in a process with more than one thread gives a `DeprecationWarning`. A fork after the open of a store is unsafe for the store anyway.
- The check does not see a handler that C code installs without Python after the start of the interpreter. signal-hook then calls that handler before the watcher closes the stores.
- On a free-threaded build, the check and the install are two steps. A handler that the main thread sets between them does not stop the close.
- A call that does not return, such as a running pack, holds its store past the bound, so that store stays unclean.
- The watcher does not wait for a read through a weak handle of the core: a read of the iterator of `_iterator` or of a blob file. Such a read holds the redb database for the length of the read, but it holds no share of the store. When the store drops while such a read is in progress, the watcher raises the signal at once, and the process ends while the read holds the database open. That store stays unclean, also when the read would end well before the bound. A full fix needs an API of the core.
- A store that opens after the watcher took its list of the open storages is not closed.
- A `close` or a drop of the storage that runs after the watcher took the store returns without a wait for the close of the watcher. So a normal exit that races the signal can end the process during that close.
- The repair on open (ZOE-128) covers each store that these cases leave unclean, as it covers SIGKILL and a power loss.
- A child of `os.fork()` that opens a store gets no handler: a signal ends it with the default action.
- The writer commands of the console script (`zoe pack`, `zoe convert --from-data-fs`, `zoe restore`) open no `zoe.Storage`, so a signal stops them at once (ADR 0028).
