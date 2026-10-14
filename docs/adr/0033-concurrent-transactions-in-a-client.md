# 0033. Several transactions at once in one client process

Status: proposed. The spike `docs/spikes/concurrent-commits.md` gives the evidence. The human decides. The native client of F28 builds the choice.

## Context

A ZEO connection runs one transaction at a time. `ZEO.ClientStorage` holds a commit lock from `tpc_begin` to `tpc_finish`, and the POC client of ZOE-077 on one connection does the same. So in one client process, the commits of all the threads wait for one another. In the threads cells of zodbshootout, the cell then measures the time of one commit, not the server. On free-threaded CPython 3.14t, the POC client with a pool of connections, where each transaction takes its own connection, gave 1.5 to 3.6 times the throughput of one connection in the threads cells `add` and `update` at concurrency 16 and 64, with the rule below or without it.

A pool must keep the contract of ZODB for invalidations. ZODB 5.6 and later ask that `lastTransaction()` gives a tid only after the DB got the invalidations of that tid and of every tid before it. ZODB 6.3 also moves the start of the next snapshot of each ZODB connection to the tid of each invalidation that it gets, and to the tid of its own commit through the callback `f(tid)` of `tpc_finish`. `ZEO.ClientStorage` keeps the contract because one I/O thread reads one stream: the server sends the invalidations of earlier commits before the reply of a finish, and the I/O thread runs `f(tid)` and the invalidations in that order.

With a pool, the reply of a finish on one connection can come before another connection delivers the invalidations of an earlier tid. The POC pool of ZOE-077 also moved `lastTransaction()` to a tid of its own before `f(tid)` ran. With either fault, a ZODB connection starts a snapshot after a tid while its cache still holds objects that an earlier commit changed. The race tests of ZODB fail on it.

Two designs were compared:

- (a) a pool of connections, with one ordered stream of invalidations;
- (b) several transactions on one connection, told apart by the transaction id that `storea` and `vote` already carry. It is a change of the wire protocol: an extension that a stock ZEO server does not have.

## Decision

**The native client runs several transactions at once through a pool of connections (design (a)).** Each transaction takes a free connection of the pool for its whole two-phase commit. The pool opens a connection when a transaction finds none free, up to a bound, so a DB that opens costs one connection, not the bound. Loads may use any connection of the pool. The pool needs no change of the server and works against `zoe serve` and against a stock ZEO 6 server.

**One listener connection orders the invalidations.** One connection of the pool, the listener, reads the invalidations and does not commit. The server sends it the invalidations of every commit of the other connections, the pool's own commits included, in tid order. That stream is the order of the client process.

**The rule.** The client delivers each tid in the order of the listener's stream, one at a time:

1. A tid of another client goes to the DB as `invalidate(tid, oids)`.
2. A tid of the pool never goes to the DB as an invalidation. The client runs the `f(tid)` of its `tpc_finish` in its place in the stream.
3. `lastTransaction()` moves to a tid only after the client delivered it. So it never passes a tid whose invalidations or whose `f(tid)` did not run.
4. A `tpc_finish` of tid T returns, and its `f(T)` runs, only after the client delivered every tid below T.
5. While a finish of the pool waits for its reply, the listener holds a tid that it does not know: the tid can be the one of that finish. It holds the tid only until the finishes that were sent before the tid arrived have their replies. A finish sent later cannot commit an earlier tid, so the listener never waits for it.

**One connection keeps the rule too.** With a pool of one connection, the reply of a finish and the invalidations come on one stream. The client delivers them in that order: the reader of the connection gives the reply of a finish to the same ordered stream as the invalidations, and `f(tid)` runs from that stream. A finish that returns on another thread while the invalidations wait in a channel breaks the contract, also with one connection.

**A lost listener is a lost connection.** When the listener connection closes, the client fails the finishes that wait for it, clears the invalidation state of the DB (`invalidateCache`), and connects the listener again with `getInvalidations`, as `ZEO.ClientStorage` does after a reconnection. A listener that falls behind its channel does the same.

**Design (b) is not built.** It would change the wire protocol, and a change of the wire protocol needs the human. It is kept as a possible extension for a later phase (see Alternatives).

## Consequences

- On 3.14t, the threads cells of zodbshootout measure the server and the storage, not the latency of one commit.
- Each client process holds more sockets on the server: one for each transaction that runs at the same time, plus the listener. With connections that open on demand, a process with few concurrent transactions holds few.
- A commit of the pool waits for its own tid on the listener before `tpc_finish` returns: one more delivery after the reply. In the spike, the rule cost about 4 % in the `add` cells and nothing measurable in the `update` cells. A foreign tid waits for at most the finishes that were in flight when it arrived.
- The client cache (ZOE-152) applies its invalidations and the writes of the pool's own commits in the same order as the rule, before `lastTransaction()` moves.
- The rule lives in the client. `zoe serve` and a stock ZEO server already send each connection the invalidations of every other connection in tid order, and the client relies on it.
- The POC pool of ZOE-077 broke the contract, and so did the POC on one connection. Its numbers stay valid for the cost of a pool, not for its correctness.

## Alternatives

- **Several transactions on one connection (design (b)).** One stream gives the order for free, and a process holds one socket. But it needs an extension of the protocol, declared in `register`, and a session of `zoe serve` that holds several open transactions, each on its own thread, with a lane of calls for each. A commit of one transaction of the connection gives no invalidation to the connection, so the client invalidates its other ZODB connections itself, through `f(tid)`, as today. Against a stock ZEO server, the client needs (a) anyway, or one transaction at a time. So (b) adds a second path to the client and a change of the wire protocol, for a gain in sockets only.
- **One connection for each process, one transaction at a time.** It keeps the contract with no rule beyond one ordered stream, but it caps the threads cells at the latency of one commit.
- **A pool with one stream for each connection, merged by tid.** Each connection gets the invalidations of every commit but its own, so each stream with its own finishes is complete. Merging them in tid order needs to know that no stream holds an earlier tid, which needs a hold on every stream. One listener is simpler.
- **A listener that also commits.** It saves one socket. Its own finishes give no tid on its stream, so its reader must put them in the stream itself, as with one connection. It is possible in the Rust client, but it mixes the two cases.
