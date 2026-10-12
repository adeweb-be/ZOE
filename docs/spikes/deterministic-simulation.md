# Spike: a deterministic simulator for the server

Card ZOE-050, requirement N18. The simulator also serves N1 through the kill loop.

**Verdict.** Build an executor of our own, in a `sim` module of `zoe-server` that only test builds compile. Do not use turmoil or madsim. The hard part is not the network or the clock: it is the blocking pool. The core is synchronous, and a vote waits for its locks on a `Condvar` with a timeout on `Instant`. Neither framework runs that wait on simulated time. Under turmoil, `spawn_blocking` runs on real threads in an order that the seed does not fix, and a 300 ms lock wait costs 300 ms of real time. Under madsim, a blocking call runs as a task of the one simulation thread, `std::thread::spawn` panics, and a 300 ms `Condvar` wait returns at once, so every lock wait of the core would time out at once. With either framework, ZOE must still build the same two things: a seam in the core for its lock wait, and a scheduler that runs one blocking thread at a time. With these two things built, the rest of a simulator is small. The prototype on `card/ZOE-050` passes the acceptance test. It runs the session of the server and the core as they are, with two changes: the lock wait goes through the store clock, and `session::run` takes an iterator of calls. Over 1,000 seeds it acknowledged 96,085 commits through 4,374 power-loss crashes and lost none. One seed gives one trace, also across processes and between a debug and a release build. A deliberate durability bug is caught by every seed of 200. A seed of 4 clients, 100 commits and 5 crashes costs 54.8 ms in release. It adds no crate to the dependency graph. The decisions for the human are at the end.

## Question

How do the server and its clients run in one process on a simulated network and clock with seeded schedules, so that the kill loop, and later the failover test, runs inside a simulator and a failing seed reproduces (N18)? The finding compares turmoil, madsim and an executor of our own. It says how the blocking pool runs under simulation, how the `Clock` and `Vfs` traits of the core and the crash images of `MemVfs` plug in, what `zoe-server` and `zoe-client` route through traits, and the cost in code, build time and test time. It recommends one way and the shape of the `sim` API for ZOE-087.

## Method

1. **Prototype.** An executor of our own, `zoe_server::sim`, on the branch `card/ZOE-050` (12754fb). The feature `sim` of `zoe-server` builds it. Only the dev-dependency of the crate on itself turns the feature on, as `zoe-core` does with `testing`. So a build of `zoe` outside the tests does not link it, and `cargo tree -p zoe-py -e normal` still lists no tokio. The prototype is described in "What the prototype runs" below.
2. **Acceptance test.** `crates/zoe-server/tests/simulation_spike.rs`, unchanged. `crates/zoe-server/tests/simulation_probe.rs`, on the card branch, prints the measures that this finding quotes.
3. **Probes of the frameworks.** Three small crates outside the workspace, in `sim-probes/` on the card branch, with the toolchain of `devenv.nix` (Rust 1.99.0): tokio 1.53.2 alone, as the baseline of the build times; turmoil 0.7.2 on tokio 1.53.2; and madsim 0.2.34 with madsim-tokio 0.2.30 (which wraps tokio 1.53.2), built with `RUSTFLAGS="--cfg madsim"`. The turmoil and madsim probes check four things with one seed: the order in which four `spawn_blocking` tasks end, a `Condvar` wait of 300 ms in `spawn_blocking` (the shape of the lock wait of the core), the order of `select!` between two ready branches, and the time that `std::time` gives. The turmoil probe also ran with `RUSTFLAGS="--cfg tokio_unstable"`, and it polls `select!` from a plain executor, outside any tokio runtime. The current API and limits of turmoil, madsim and tokio come from their documentation (Context7), and the probes check the points that matter here.
4. **Determinism.** The acceptance test runs each of 20 seeds twice in one process and compares the traces. `print_trace_hashes` hashes the traces of 20 seeds of 4 clients, 100 commits and 5 crashes; it ran in three debug processes and one release build.
5. **Machine.** A 22-thread laptop CPU, 32 GB of memory, Linux 7.0. Other agents built and tested on the machine at the same time, so wall times are noisy. The tables give CPU time where it is the better measure.

## What the prototype runs

The driver is a single-threaded executor. It holds the simulated time, a queue of timers, the tasks that are ready, and the session threads. At each step it picks, with the seeded generator, one ready task or one session thread that can run. When nothing can run, it moves the clock to the next timer or lock deadline. Nothing in a run reads the real time.

- **The server.** For each connection, a task does what `protocol::serve_connection` does: it sends `M5`, reads `M5`, starts a session thread, starts a writer task for the replies, and forwards each decoded call to the session thread. This glue (`sim/server.rs`, 153 lines) mirrors the 82 lines of `serve_connection`; the build card makes `serve_connection` generic instead (see "What zoe-server and zoe-client route through traits"). The session thread runs `protocol::session::run`, `Session::call` and the core, with only the two changes of the verdict. The frames go through `protocol::frame::read` and `frame::write`, which already take any `AsyncRead` and `AsyncWrite`, and the calls through `protocol::message::decode`.
- **The clients.** Raw ZEO 6 clients, as `ZEO.ClientStorage` calls: `register`, `loadBefore`, then `tpc_begin` and `storea` without a reply, `vote` and `tpc_finish`. Each transaction writes one or two oids out of six, and half of the transactions write oid 1, a hot object. A conflict sends `tpc_abort` and tries again, up to 8 times. A lost connection reconnects. A `tpc_finish` whose reply does not arrive has an unknown outcome and does not count. `zoe-client` (ZOE-064) replaces these clients in the build.
- **The network.** A connection is two ordered byte pipes. `SimStream` implements tokio's `AsyncRead` and `AsyncWrite`. Each frame arrives after a seeded delay of 20 µs to `Faults::delay` (2 ms by default), never before the frame before it. A frame is lost with the chance `Faults::loss` (0.002 by default), and a lost frame breaks its connection at both ends, as TCP does when it gives up.
- **The disk.** The store is on a `MemVfs`, behind `SimVfs`, a `Vfs` that delegates every call to the `MemVfs`. Only the database file differs: its `sync_data` on a session thread is a point where the driver can crash the server. The core opens it with `Store::open_full(vfs, clock, options)`, which exists already.
- **Crashes.** The seed plans each crash at the n-th sync of the database file: at the sync itself (30 %), before the commit is durable, or up to 30 ms of simulated time after it. A crash is a power loss. `MemVfs::crash_now` takes an image of the disk: every synced write, and a seeded prefix of the pending writes. Then the driver breaks every connection of the server, drops its tasks, and ends its session threads. A session thread of a dead server sees an I/O error at its next sync and the end of time at its next lock wait, so it ends at once. The store then drops, and the server reopens the image 5 to 100 ms later. The end of a run is a power loss too: the driver reopens the last image and loads every record of every acknowledged commit with `load_serial`. `Report::lost` counts the acknowledged commits that miss a record.
- **The deliberate bug.** With `Faults::ack_before_durable`, the database file skips its sync at the end of `finish`, and the driver syncs it 5 to 100 ms later. So the server acknowledges `tpc_finish` before the commit is durable.
- **The stall.** With `Faults::stall_client_for`, client 0 waits that long after its first successful vote before it sends `tpc_finish`, and keeps its locks.

## The blocking pool under simulation

The server runs each connection on one thread of the blocking pool, so that one thread drives each core transaction: the core refuses a call from another thread with `WrongThread`. The core blocks a thread in two places where one call waits for another call: the lock wait of a vote, on a `Condvar` with a timeout on `Instant`, and the follower of a commit batch in group commit, on a `Condvar` with no timeout.

The prototype keeps one real thread per session, as production does, and runs one thread at a time. The driver hands the baton to a thread through a channel, then waits until the thread gives it back. A thread gives the baton back in four places, each a seam that the driver sees:

| Where | Seam | The driver resumes the thread |
| --- | --- | --- |
| The session waits for its next call | the iterator of calls that `session::run` now takes | when a call arrives, or when the connection closes |
| The core waits for a lock | `Waiter::wait` of the store clock | when the thread that releases the lock calls `Waiter::wake`, or at the deadline on the simulated clock |
| The core syncs the database file | `sync_data` of the `SimVfs` file | at once, or never: the driver crashes the server there |
| The session ends | the end of the thread | — |

Because only one thread runs at a time, and a sync never lets another thread in, no two finishes overlap and a commit batch never has a follower. So the group-commit wait never happens under the prototype, and it needs no seam yet. The simulator does not exercise batches of two or more finishes; the crash simulation and TSan of the core cover them. If a later card gives a sync a duration of simulated time, so that other threads run during it, the follower wait of group commit must go through the same `Waiter`.

The stall test shows the lock wait on simulated time. Client 0 votes and stalls for 60 s with its locks. Client 1 votes at 8.1 ms on an oid that client 0 holds, waits for the lock, and gets `ConflictError` at 10.0088 s: the lock timeout of 10 s, on the simulated clock. The run ends at 60.04 s of simulated time after 42 ms of real time.

## The core: `Clock`, `Vfs` and the crash images

- **`Clock`.** The core already reads the wall-clock time only through `Clock`: the tids and the durations of `lock-warn` and `transaction-warn`. `SimClock` gives the start of the run plus the simulated time, so the tids follow the simulated clock. The one gap was the lock wait, which used `Instant` and a `Condvar` because "a `Condvar` wait cannot run on a simulated clock". The prototype adds two provided methods to `Clock`, `monotonic()` and `waiter()`, and a new trait `Waiter` with `wait(timeout)` and `wake()`. The defaults keep the wait on the threads of the operating system: `monotonic` is `Instant` since a fixed origin, and the default waiter is a flag, a mutex and a `Condvar`, with permit semantics, so a `wake` before the `wait` is not lost. The lock manager takes one waiter for each `acquire`, outside its mutex, puts it in the queue of the oid, releases the mutex, waits, and checks the queue again. A guard takes the request out of the queue, or gives back the lock that a grant gave to it, if the clock panics during the wait. No existing signature changes. All 317 unit tests of the core and its lock tests pass unchanged, and so does `just test`. `SimClock` gives `Duration::MAX` as the monotonic time to a thread of a dead server, so its wait ends at once.
- **`Vfs`.** It needs no change. `SimVfs` wraps `MemVfs` and turns the sync of the database file into a crash point. The same wrapper injects the deliberate bug.
- **Crash images.** `MemVfs::crash_images` gives every image of every crash point, and replays the record for each one. That is right for the exhaustive tests of the core, and too much for a run of thousands of writes. The simulator needs one image at one point. The prototype adds `MemVfs::crash_now(lands)`: every synced operation, and the first `lands(n)` of the n pending operations, in their recorded order. It reuses `split_at_syncs` and `apply`. The image is a new `MemVfs` with an empty record, so the next crash starts from it. In 100 clean seeds of 4 clients, 100 commits and 5 crashes, each of the 161 crashes in a sync found 8 to 13 pending writes, the writes of that commit. Of the 270 crashes after a sync, 269 found none and one found one. The final power losses found none. So every acknowledged commit was synced, and the image only cuts the commit that syncs. ZOE-088 asks for images that "keep only what was synced, with the unsynced writes reordered or cut". `crash_now` can take a chooser of subsets and torn writes, as `crash_images` does. Reordered writes of several commits make a damaged database file, and the three known faults of redb with a damaged file (no checksum check on read, an unbounded walk, a double panic that aborts the process) can then hang or abort a run. So reorder only the writes of the commit that syncs, as the core's own crash simulation does.

## What zoe-server and zoe-client route through traits

The simulator must run the production code, not a model (ZOE-087). The prototype runs the session and the core; the build card must also run the connection code and `zoe-client`. These are the points where they reach tokio, the time or the threads today:

| Code | Today | Under the simulator |
| --- | --- | --- |
| `protocol::serve_connection` | takes a `tokio::net::TcpStream` | takes any `AsyncRead + AsyncWrite`; the sim gives `SimStream` |
| `serve::accept` | `TcpListener::accept`, then `tokio::spawn` | a listener and a spawn of the runtime trait |
| the writer task | `tokio::spawn` | the spawn of the runtime trait |
| the session | `spawn_blocking` and a `std::sync::mpsc` channel of calls | a session thread of the runtime trait, which gives `session::run` its iterator of calls and its reply sink |
| `ruok` | `spawn_blocking` for the status | the same session thread, or a blocking call of the runtime trait |
| the read loop | `tokio::select!` between the next frame and the end of the session | `select!` with `biased;` |
| `NamedStorage::new` | `SystemTime::now()` for the start time of the status | the store clock |
| `zoe-client` (ZOE-064) | async on tokio | the same: tokio's I/O traits and `tokio::sync`, but its connect, its spawns and its sleeps (reconnect, heartbeat, timeouts) come from its runtime trait, never from `tokio::net`, `tokio::spawn` or `tokio::time` |

Two rules make this work under an executor of our own:

- **`tokio::sync` and `tokio::io` stay.** tokio documents its synchronization primitives as runtime agnostic, except the methods that end in `_timeout`, and the prototype runs `frame::read`, `frame::write` and `AsyncWriteExt::shutdown` on `SimStream` from its own executor. So the code keeps `AsyncRead`, `AsyncWrite`, channels and `Notify`.
- **Every `select!` is `biased;`.** By default `select!` polls a random branch first. Outside a tokio runtime its generator has no seed: the probe gave three different orders in three processes, and `biased;` gave the same order each time. A seeded order needs `--cfg tokio_unstable` and a tokio runtime, as turmoil does.

Each crate states what it needs in its own small trait: the server needs a listener, a spawn and a session thread; the client needs a connect, a spawn and a sleep. Production implements them with tokio, and the simulator with its executor. No new crate is needed. `zoe-core` stays free of tokio: its seam is `Clock` and `Vfs`, which are synchronous.

## Comparison

| | turmoil 0.7.2 | madsim 0.2.34 | an executor of our own |
| --- | --- | --- | --- |
| What it simulates | the network (`turmoil::net` in place of `tokio::net`), the time (each host is a current-thread tokio runtime with paused time), a filesystem (`turmoil::fs`), crash and bounce of hosts | all of tokio (madsim-tokio), the network, the time and the randomness that `std` reads, kill and restart of nodes | what ZOE needs: TCP connections, the clock, the blocking pool, power loss on `MemVfs` |
| How it builds | a dependency; a seeded order of the tasks needs `RUSTFLAGS="--cfg tokio_unstable"`, which rebuilds the whole graph | `RUSTFLAGS="--cfg madsim"` on the whole graph, in its own target directory; every crate that uses tokio uses madsim-tokio | a feature `sim` that only test builds turn on; no new dependency |
| `spawn_blocking` | real threads: 4 tasks ended in 18 different orders in 200 runs of one seed (15 with `tokio_unstable`) | a task of the simulation: 1 order in 50 runs; it cannot run on a thread of its own, because madsim forbids threads | one real thread per session, as in production, one at a time |
| A lock wait of the core (a `Condvar` wait of 300 ms) | 300 ms of real time; the simulated time does not move | returns at once (the whole probe took 0.11 s). The likely cause: madsim gives `std` the simulated time, so the absolute deadline of the wait is in the past for the kernel. Every lock wait would time out at once. By construction, a wait with no timeout, as in group commit, would block the only thread of the simulation | through `Clock::waiter`: 10 s of lock timeout on the simulated clock in milliseconds of real time |
| `std::thread::spawn` | works, not simulated | panics: "attempt to spawn a system thread in simulation", unless `MADSIM_ALLOW_SYSTEM_THREAD` is set | used, under the baton |
| `std::time` | real | simulated (60 s of sleep moved `Instant` and `SystemTime` by 60 s) | not used: the core reads `Clock` |
| `select!` | 0 of 20 runs equal under one seed; 20 of 20 with `tokio_unstable` | equal (`check_determinism` passes) | random unless `biased;` |
| Disk and crashes | `turmoil::fs` with a sync probability, not the `Vfs` of the core | none | `MemVfs` and `crash_now` |
| What ZOE must still build | the core seam, the baton for the blocking pool, `biased;` or `tokio_unstable`, the power loss on `MemVfs` | the core seam, and a way to run blocking threads that madsim forbids; the power loss on `MemVfs` | the executor, the network, the baton, the core seam |
| Cold build of the probe crate (CPU) | 40 s (tokio alone: 15 s) | 65 s | no new crate; +1.5 s of CPU on the library of `zoe-server` |

The frameworks give a ready executor and network. They do not run a synchronous core that blocks. ZOE needs the core seam and a scheduler of blocking threads with any of the three, and with turmoil and madsim it also takes a second build of the whole graph under a global `cfg`. The executor, the timers, the baton and the network of the prototype are 573 lines of code (`world.rs`, `net.rs`, `rng.rs`). That is about what a framework would save, and a framework would still need the baton.

## Costs

### Code

| Part | Lines | Of which code |
| --- | --- | --- |
| `sim/mod.rs`: the driver, the crash plan, the power loss, the check | 626 | 511 |
| `sim/world.rs`: the shared state, the timers, the wakers, the baton | 427 | 328 |
| `sim/client.rs`: the raw clients (ZOE-064 replaces them) | 355 | 308 |
| `sim/net.rs`: the connections | 254 | 216 |
| `sim/disk.rs`: `SimClock`, `SimWaiter`, `SimVfs` | 209 | 150 |
| `sim/server.rs`: the connection glue (a generic `serve_connection` replaces it) | 153 | 132 |
| `sim/rng.rs`: SplitMix64 | 44 | 29 |
| `zoe-core`: `Clock`, `Waiter`, the lock wait, `crash_now` | +171, −40 | |
| `zoe-server`: `session::run` and the feature | +25, −13 | |

"Of which code" leaves out blank lines and comments.

### Build time

The builds ran inside `devenv shell`, with 22 jobs, while other agents built and tested on the machine (load average 18 to 39). CPU is user plus system time. A cold test build of the whole graph of `zoe-server` varied from 87 s to 144 s of CPU between two runs of the same build, so the comparison below isolates the parts.

| Build | CPU | Wall |
| --- | --- | --- |
| the library of `zoe-server` alone, from scratch, without incremental compilation, without `sim` (median of 5) | 2.4 s | 1.28 s |
| the same with `sim` (median of 5) | 3.9 s | 2.01 s |
| the acceptance test from scratch: the library with `sim`, the `zoe` binary and the test binary | 6.4 s | 3.0 s |
| every test binary of `zoe-server` after a change to `sim/rng.rs` | 8.9 s | 2.4 s |
| the probe crate with tokio alone, cold: the baseline of the two below | 14.3 s, 15.2 s | 8.1 s, 10.6 s |
| the probe crate with turmoil, cold | 39.9 s, 40.0 s | 12.1 s, 22.2 s |
| the same with `--cfg tokio_unstable` | 41.2 s, 39.6 s | 20.7 s, 16.3 s |
| the probe crate with madsim and `--cfg madsim`, cold | 65.8 s, 65.1 s | 14.8 s, 13.1 s |

- The simulator adds about 1.5 s of CPU and 0.7 s of wall time to the compile of the `zoe-server` library, and no crate.
- In a test build, `cargo test` compiles the library of `zoe-server` once, with `sim`. `nm` finds 0 symbols of `zoe_server::sim` in the `zoe` binary of `cargo build -p zoe-server`, and 1,490 in the binary of the acceptance test.
- turmoil adds about 25 s of CPU over tokio alone, and madsim about 51 s. With either, a seeded run needs a global `--cfg` (`tokio_unstable` or `madsim`), so the whole graph builds a second time, in a target directory of its own: a cold test build of `zoe-server` and its dependencies, 87 s to 144 s of CPU here, again at each change of the toolchain or of `Cargo.lock`.

### Test time

All the times are real time, on the machine of the Method, with other jobs running.

| Run | Debug | Release |
| --- | --- | --- |
| The acceptance test, 5 tests | 6.6 s | — |
| One seed: 3 clients, 50 commits, 2 crashes | 210 ms | — |
| One seed: 4 clients, 100 commits, 5 crashes | 406 ms (100 seeds) | 54.8 ms (1,000 seeds) |
| One seed with the deliberate bug: 3 clients, 50 commits, 3 crashes | 193 ms (200 seeds) | — |
| The stall: 60 s of simulated time | 42 ms | — |

So 10,000 seeds of this size take about 550 s of one core in release. The kill loop of ZOE-088 runs bigger seeds (200 commits, blobs), so it takes more. The seeds are independent: each run has its own driver and threads, and the 5 tests of the acceptance test run in parallel in one process.

## Results

- **Same seed, same run.** The acceptance test compares 20 seeds run twice. The 20 seeds of `print_trace_hashes` gave the same hash, `f16df567245c5686`, in three debug processes and in a release build. The core and redb do not depend on the order of a hash map: the lock table and the commit queue only look up their maps, and redb hashes page numbers with a fixed multiplier, not `RandomState`.
- **Different seeds, different runs.** The 20 seeds of the test give more than one trace; each trace carries the simulated time of each event.
- **No acknowledged commit is lost.** Over the 20 seeds of the test (4 clients, 100 commits, 5 crashes): 1,922 acknowledged commits, 89 crashes (29 in a sync, 60 after one), 548 lock waits, 1,150 conflicts, 62 lost frames that broke a connection, and 0 lost commits. Over 1,000 seeds in release: 96,085 acknowledged, 4,374 crashes, 26,812 lock waits, and 0 lost. A crash plan whose sync never comes does not run, so a seed of 5 planned crashes has 4.4 on average.
- **The deliberate bug is caught, and its seed reproduces.** All 200 seeds catch it. Seed 0 acknowledges 49 commits and loses 15; it crashes after a sync with 22 of 108 pending writes landed, and its final power loss lands 18 of 39. The run of the same seed again gives the same trace and the same 15.
- **Simulated time costs no real time.** See the stall in "The blocking pool under simulation".

## Recommendation

Build the simulator as an executor of our own in `zoe-server`, behind the feature `sim`, with the core seam of the prototype. In order:

1. **The core seam** (`zoe-core`): `Clock::monotonic`, `Clock::waiter` and `Waiter`, with the lock manager waiting through them, and `MemVfs::crash_now` with a chooser. The core invariant "lock waits use `Instant`" becomes "lock waits use the monotonic time and the waiter of the store clock". `just tsan` and the lock tests gate it.
2. **The runtime traits** (`zoe-server`, `zoe-client`): the rows of the table above, with tokio as the production implementation, and `biased;` in every `select!`.
3. **The simulator** (`zoe-server`, `src/sim/`): the driver, the baton, `SimStream`, `SimClock`, `SimVfs`, the faults and the report, with `zoe-client` as the client.

### The shape of the `sim` API for ZOE-087

The skeleton of ZOE-087 already has this shape, and the prototype confirms it:

- `zoe_server::sim::Simulation::new(seed)`, then `clients(n)`, `commits(n)`, `crashes(n)`, `faults(Faults)`, and later `blobs(bool)` for ZOE-088; `run()` gives a `Report`.
- `Faults` is a struct with public fields and a `Default`, so a test writes `Faults { loss: 0.01, ..Faults::default() }`: `delay`, `loss`, `partitions`, `closes`, `stall_client_for`, `ack_before_durable`, and `power_loss` for ZOE-088.
- A crash of the prototype is a power loss: the synced writes survive, and a prefix of the others. A process kill keeps the page cache, so it keeps every write. Then the bug of the prototype, a late sync, is never caught: only a bug that acknowledges before the write itself is. The skeleton of ZOE-087 expects `crashes(3)` with `ack_before_durable` and the default faults to lose a commit. The skeleton of ZOE-088 sets `power_loss: true`, so its default looks false. The planner makes them agree: `power_loss` true by default, or set in the durability test of ZOE-087.
- `Report` has `trace: Vec<String>`, `acknowledged`, `lost`, `stale_reads`, `crashes`, and for ZOE-088 `crash_phases` and `verify`. Each trace line starts with the simulated time in microseconds, then the actor (`c<n>`, `s`, `net`, `server`) and the event.
- `ZOE_SIM_SEED` runs one seed, and a failing seed prints `ZOE_SIM_SEED=<seed>`.
- Inside: `world` (the executor, the timers, the baton), `net` (`SimStream` and the listener), `disk` (`SimClock`, `SimWaiter`, `SimVfs`), and the driver. The server runs its own generic `serve_connection`, and the clients are `zoe-client`.

## Decisions for the human

1. **The way.** An executor of our own, not turmoil or madsim.
2. **The core seam.** `Clock` gets two provided methods and a new trait `Waiter` beside it. No existing signature changes, and the implementations of `Clock` in other crates keep compiling. It changes the lock wait of the core, which is not a red zone but a path of every vote.
3. **The scope fence of ZOE-087.** It names `crates/zoe-core/src/clock.rs` and `crates/zoe-core/src/vfs.rs`. The seam also needs `crates/zoe-core/src/lock/mod.rs` and the export in `crates/zoe-core/src/lib.rs`. Widen the fence, or cut a core card for the seam before ZOE-087.
4. **`biased;`.** Every `select!` in code that the simulator runs must be `biased;`. Add the rule to the `CLAUDE.md` of `zoe-server` and `zoe-client`.
5. **`power_loss` in the skeletons.** The durability test of ZOE-087 needs a crash that loses unsynced writes, and the skeleton of ZOE-088 sets `power_loss: true` as if its default were false. See the API shape above.

## Not tested

- The real `serve_connection` and `accept` under the simulator: the prototype mirrors them.
- `zoe-client`, which is not built yet: the prototype has raw clients.
- Batches of two or more finishes, a sync that takes simulated time, and the follower wait of group commit under the simulator.
- Crash images with reordered or torn writes: the prototype lands a prefix of the pending writes.
- Blobs, pack, invalidations, `getInvalidations`, stale reads, read-only mode and named storages.
- Partitions and explicit closes of connections: the prototype has delays and lost frames.
- The cost of the new waiter on the production lock wait: one allocation for each `acquire` that waits or not. No benchmark of the lock manager exists yet (B3).
- turmoil and madsim on the ZOE code: only the probes ran, on the behaviours above.

## The acceptance test

`crates/zoe-server/tests/simulation_spike.rs` passes on `card/ZOE-050`, unchanged: 5 tests in 6.6 s. On the same branch, `just check`, `just test` and `just tsan` pass: TSan runs the core with the lock wait through `Waiter`, reports no race, and reports the race of the canary test.
