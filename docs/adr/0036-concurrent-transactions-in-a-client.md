# 0036. Several transactions at once in one client process

Status: accepted. The human accepted design (a). The spike `docs/spikes/concurrent-commits.md` gives the evidence. The native client of F28 builds the choice.

## Context

A ZEO connection runs one transaction at a time. `ZEO.ClientStorage` holds a commit lock from `tpc_begin` to `tpc_finish`, and the POC client of ZOE-077 on one connection does the same. So in one client process, the commits of all the threads wait for one another. In the threads cells of zodbshootout, the cell then measures the time of one commit, not the server. On free-threaded CPython 3.14t, the POC client with a pool of connections, where each transaction takes its own connection, gave 1.5 to 3.6 times the throughput of one connection in the threads cells `add` and `update` at concurrency 16 and 64, with the rule below or without it.

A pool must keep the contract of ZODB for invalidations. ZODB 5.6 and later ask that `lastTransaction()` gives a tid only after the DB got the invalidations of that tid and of every tid before it. ZODB 6.3 also moves the start of the next snapshot of each ZODB connection to the tid of each invalidation that it gets, and to the tid of its own commit through the callback `f(tid)` of `tpc_finish`. `ZEO.ClientStorage` keeps the contract because one I/O thread reads one stream: the server sends the invalidations of earlier commits before the reply of a finish, and the I/O thread runs `f(tid)` and the invalidations in that order.

With a pool, the reply of a finish on one connection can come before another connection delivers the invalidations of an earlier tid. The POC pool of ZOE-077 also moved `lastTransaction()` to a tid of its own before `f(tid)` ran. With either fault, a ZODB connection starts a snapshot after a tid while its cache still holds objects that an earlier commit changed. The race tests of ZODB fail on it.

Two designs were compared:

- (a) a pool of connections, with one ordered stream of invalidations;
- (b) several transactions on one connection, told apart by the transaction id that `storea` and `vote` already carry. It is a change of the wire protocol: an extension that a stock ZEO server does not have.

## Decision

**The native client runs several transactions at once through a pool of connections (design (a)).** Each transaction takes a free connection of the pool for its whole two-phase commit. The pool opens a connection when a transaction finds none free, up to a bound, so a DB that opens costs one connection, not the bound. Loads may use any connection of the pool. A load reply that fills the client cache is ordered by the connection that carried it (see "Loads and the client cache"). The pool needs no change of the server and works against `zoe serve` and against a stock ZEO 6 server.

**One listener connection orders the invalidations.** One connection of the pool, the listener, reads the invalidations and does not commit. The server sends it the invalidations of every commit of the other connections, the pool's own commits included, in tid order. That stream is the order of the client process.

**The rule.** The client delivers each tid in the order of the listener's stream, one at a time:

1. A tid of another client goes to the DB as `invalidate(tid, oids)`.
2. A tid of the pool never goes to the DB as an invalidation. The client runs the `f(tid)` of its `tpc_finish` in its place in the stream.
3. `lastTransaction()` moves to a tid only after the client delivered it. So it never passes a tid whose invalidations or whose `f(tid)` did not run. `lastTransaction()` is the last tid that the dispatcher delivered, from the `register` tid of the listener on. It is not `Client::last_transaction` of `zoe-client`: the reader raises that value before it sends the invalidation (`crates/zoe-client/CLAUDE.md`), which is the first fault of the POC. It is not the maximum over the connections of the pool either, as the POC takes it at the open.
4. A `tpc_finish` of tid T returns, and its `f(T)` runs, only after the client delivered every tid below T.
5. While a finish of the pool waits for its reply, the listener holds a tid that it does not know: the tid can be the one of that finish. It holds the tid only until the finishes that were sent before the tid arrived have their replies. A finish sent later cannot commit an earlier tid, so the listener never waits for it. A finish counts as in flight from before its frame goes out, as the POC counts it before `txn.finish()`. A finish that counts only after the send lets the listener deliver the pool's own tid as a foreign invalidation: the finish then sees `lastTransaction()` at T at once and returns without its `f(T)`.

**The failure paths leave the hold.** A finish that fails, a finish whose connection is lost, and a `Transaction` of `zoe-client` that drops after its `tpc_finish` frame went out must each stop counting as in flight, or the listener holds every later unknown tid. The POC does it for the first two, in the `except` path of `_finish_in_order`. The listener then delivers their tid, if the server committed it, as the tid of another client: `invalidate(tid, oids)`, which is safe. `tpc_abort` sends no finish and never holds a tid.

**One connection keeps the rule too.** With a pool of one connection, the reply of a finish and the invalidations come on one stream. The client delivers them in that order: the reader of the connection gives the reply of a finish to the same ordered stream as the invalidations, and `f(tid)` runs from that stream. A finish that returns on another thread while the invalidations wait in a channel breaks the contract, also with one connection.

**Loads and the client cache.** With a pool, a "current" `loadBefore` reply is ordered only against the stream of the connection that carried it, not against the listener. `zoe serve` puts the reply of a load on a connection after the invalidation of each commit that the reply shows, and before the invalidation of each later commit, on that connection. So a current reply enters the client cache only when `applied` of the cache (ADR 0034) is not above the position of the connection that carried it: the last tid that this connection had read from its own stream, its invalidations and the replies of its own finishes, when the reply arrived. The reader of the connection takes that position when the reply arrives, not the caller when it stores. This is the reading of "the tid up to which it had read its stream" of ADR 0034 for a pool. The process's `lastTransaction()` is not that position: with it, a load answered on one connection before a commit, and stored after the listener delivered that commit, would enter the cache stale, the race of ZEO issue 166. Another form is possible: only the listener carries the loads that fill the cache. It costs the parallel loads of the pool, so the pool takes the position of each connection. The POC of the spike has no client cache, so its evidence does not cover this case.

**A lost listener is a lost connection.** When the listener connection closes, the client clears the invalidation state of the DB (`invalidateCache`) and connects the listener again: a new `register` tid, `getInvalidations`, and a new hold, with no finish counted. ZOE-159, the card of the reconnects, owns this. A listener that falls behind its channel does the same. What happens to a finish that waits for the listener is open for ZOE-155. If the finish has no reply, it fails, as a finish on a lost connection fails today. If the finish already has its reply, the commit is durable, and two choices exist: fail it, which reaches ZODB from `tpc_finish`, the second phase that the transaction manager does not expect to fail; or run `f(T)` after `invalidateCache` and return T, which keeps the contract because the DB treats every object as stale. The second choice looks better; ZOE-155 decides it with a test.

**Design (b) is not built.** It would change the wire protocol, and a change of the wire protocol needs the human. It is kept as a possible extension for a later phase (see Alternatives).

## Consequences

- On 3.14t, the threads cells of zodbshootout measure the server and the storage, not the latency of one commit.
- Each client process holds more sockets on the server: one for each transaction that runs at the same time, plus the listener. With connections that open on demand, a process with few concurrent transactions holds few.
- A commit of the pool waits for its own tid on the listener before `tpc_finish` returns: one more delivery after the reply. In the spike, the rule cost about 4 % in the `add` cells and nothing measurable in the `update` cells. A foreign tid waits for at most the finishes that were in flight when it arrived.
- The client cache (ZOE-152) applies its invalidations and the writes of the pool's own commits in the same order as the rule, before `lastTransaction()` moves. It stores a current load reply only against the position of the connection that carried it (see "Loads and the client cache"). The spike did not test a cache.
- The rule lives in the client. `zoe serve` and a stock ZEO server already send each connection the invalidations of every other connection in tid order, and the client relies on it.
- The POC pool of ZOE-077 broke the contract, and so did the POC on one connection. Its numbers stay valid for the cost of a pool, not for its correctness.

## Alternatives

- **Several transactions on one connection (design (b)).** One stream gives the order for free, and a process holds one socket. But it needs an extension of the protocol, declared in `register`, and a session of `zoe serve` that holds several open transactions, each on its own thread, with a lane of calls for each. A commit of one transaction of the connection gives no invalidation to the connection, so the client invalidates its other ZODB connections itself, through `f(tid)`, as today. Against a stock ZEO server, the client needs (a) anyway, or one transaction at a time. So (b) adds a second path to the client and a change of the wire protocol, for a gain in sockets only.
- **One connection for each process, one transaction at a time.** It keeps the contract with no rule beyond one ordered stream, but it caps the threads cells at the latency of one commit.
- **A pool with one stream for each connection, merged by tid.** Each connection gets the invalidations of every commit but its own, so each stream with its own finishes is complete. Merging them in tid order needs to know that no stream holds an earlier tid, which needs a hold on every stream. One listener is simpler.
- **A listener that also commits.** It saves one socket. Its own finishes give no tid on its stream, so its reader must put them in the stream itself, as with one connection. It is possible in the Rust client, but it mixes the two cases.
