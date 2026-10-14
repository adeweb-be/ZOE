# Several transactions at once in one client process, and the order of the invalidations

## Question

A ZEO connection runs one transaction at a time. How does one client process run several transactions at the same time, and which rule keeps the contract of ZODB for invalidations? The spike compares two designs: (a) a pool of connections with one ordered stream of invalidations, and (b) several transactions on one connection, told apart by the transaction id that `storea` and `vote` already carry.

## Method

- **Code.** The prototypes are on the branch [`poc/ZOE-151-concurrent-commits`](https://github.com/adeweb-be/ZOE/tree/poc/ZOE-151-concurrent-commits), which starts from `poc/ZOE-077-server-performance` (#132). **That branch is a proof of concept and is not for merge.** The rule is in `zoeclientx`, the POC storage on `zoe-client` through PyO3 (`bench/poc/ZOE-077/rustclient/python/zoeclientx/__init__.py`). `pool N` gives it N connections, and each transaction takes a free one. `rule off` is the pool of ZOE-077, unchanged. `rule on` is the rule below. Version 1 of the rule is at commit `02c0e70`, version 2 at commit `d766dd7`; the results are at commit `a6e9ec2`.
- **Versions.** ZODB 6.3, ZEO 6.2, the forks of `persistent` (6.8+zoe.1), `BTrees` and `zodbpickle` from the lock of `bench/shootout`, zope.testing 6.2 for the test helpers of ZODB. CPython 3.13.15 and the free-threaded CPython 3.14.7 (3.14t) of devenv.
- **Server.** `zoe serve` of the POC branch, release build, one new store on tmpfs for each run. The race tests run it with no toggle. The cost runs add `ZOE_POC_STORECOMPRESS=1` and `ZOE_POC_TIDONLY=1`, the "zoe best" of #132, part 4, so the numbers compare with that comment.
- **Race tests.** `racex.py` runs the race tests of ZODB, `ZODB.tests.racetest.RaceTests`, with no change. It only gives them a storage: `_storage` and `_new_storage_client` open a `zoeclientx` storage with the pool and the rule of the run, on one new `zoe serve` for each run. ZODB fixes the iterations of each test (500 for `test_race_loadopen_vs_local_invalidate`, 8 workers of 100 for `test_race_load_vs_external_invalidate`), so more rounds means more runs. `race-matrix.sh` runs the matrix. Each run records the interpreter, `sys._is_gil_enabled()` at its end, the modules that turned the GIL back on while they loaded, and the version of `persistent`.
- **A model of ours.** ZODB's model `T2ObjectsInc` writes both of its objects in each commit. A client that commits with a stale cache then gets a conflict, and the conflict hides the fault. So `racex.py` also runs ZODB's own checker, `_check_race_load_vs_external_invalidate`, with a model of the POC, `T2ObjectsIncOrTouch`: half of its steps change only a third object, and commit with no conflict. Its rows say "POC model". It is not a test of ZODB.
- **Cost.** `ab-concurrent.sh` runs `driver.py` of #132: zodbshootout, threads, 100 objects of 1 KB, 3 values, 1 warmup, 1 loop. The pool has as many connections as the concurrency. With the rule on, the pool opens one more connection, the listener.
- **Machine.** A developer workstation, not the reference machine: Intel Core Ultra 7 155H (22 hardware threads), 30 GB, stores on tmpfs. Another agent ran builds and benchmarks on it at the same time. A lock file kept the machine quiet for the cost runs: each round of the cost table took the lock exclusive, and every other heavy job, the race runs included, took it shared. The race runs did not need a quiet machine and ran during other work.

## The rule

The client process delivers the tids in one order, and moves `lastTransaction()` only behind that order.

1. **One listener.** One connection of the pool, the listener, reads the invalidations and does not commit. `zoe serve` and the ZEO server send it the invalidation of every commit of every other connection, in tid order, the commits of the pool included. Its stream is the order of the client process.
2. **A tid of another client** goes to the DB as `invalidate(tid, oids)`, in the order of the stream.
3. **A tid of the pool** never goes to the DB as an invalidation. The client runs the `f(tid)` of its `tpc_finish` in its place in the stream, as `ZEO.ClientStorage` runs `f(tid)` on its I/O thread.
4. **`lastTransaction()`** moves to a tid only after the client delivered it, by `invalidate` or by `f(tid)`. So the DB got the invalidations of each tid up to `lastTransaction()`.
5. **A finish of tid T** returns, and its `f(T)` runs, only after the client delivered every tid below T. The listener gets T itself, after every earlier tid, so the finish waits for T on the listener.
6. **The hold.** While a finish of the pool waits for its reply, the listener cannot tell if a new tid is the tid of that finish. It holds the tid until it knows. It waits only for the finishes that were sent before the tid arrived: a finish sent later commits a later tid. Version 1 of the prototype waited while any finish waited for its reply. Under steady commits of the pool, a finish is nearly always in flight, so version 1 can hold a tid of another client for a long time. This follows from the code; the spike did not measure it. Version 2 removes it, and is as safe: a tid that it releases cannot be the tid of a finish that is still in flight.
7. **One connection.** With one connection, the reply of a finish and the invalidations arrive on one stream. The client must deliver them in that order: the reader of the connection puts the reply of a finish in the same ordered stream as the invalidations, and `f(tid)` runs from that stream.

Three faults that the rule removes, all three in the POC of ZOE-077:

- The POC listener moved `lastTransaction()` to a tid of the pool as soon as the finish named it, before the committing thread ran `f(tid)`. A new ZODB connection then started its snapshot after that tid without its invalidations.
- The finish of the POC returned and ran `f(tid)` without a wait for the listener. Every MVCC instance of the DB moved its start to that tid, with the invalidations of an earlier tid of another client still in the channel of the listener.
- The POC on one connection has the second fault too: `zoe-client` gives the invalidations to a channel that a Python thread reads, and the reply of the finish to the committing thread. So one connection does not give the order by itself either.

## Evidence

The race tests of ZODB, `test_race_loadopen_vs_local_invalidate` and `test_race_load_vs_external_invalidate`, ran unchanged, on CPython 3.13 and on 3.14t, with version 2 of the rule and without the rule, together with the POC model. The matrix made 3,000 runs on each interpreter, 6,000 runs in all, each on a new `zoe serve` and a new store (`results/race-3.13.jsonl`, `results/race-3.14t.jsonl`, summed in `results/race-summary.txt`). Runs and failures:

| Test | Pool | 3.13, rule on | 3.13, rule off | 3.14t, rule on | 3.14t, rule off |
| --- | --- | --- | --- | --- | --- |
| `test_race_loadopen_vs_local_invalidate` | 4 | 0 of 600 | **27 of 100** | 0 of 600 | **43 of 100** |
| `test_race_loadopen_vs_local_invalidate` | 16 | 0 of 100 | | 0 of 100 | |
| `test_race_loadopen_vs_local_invalidate` | 1 | 0 of 100 | 0 of 100 | 0 of 100 | 0 of 100 |
| `test_race_load_vs_external_invalidate` | 4 | 0 of 600 | 0 of 100 | 0 of 600 | 0 of 100 |
| `test_race_load_vs_external_invalidate` | 16 | 0 of 100 | | 0 of 100 | |
| `test_race_load_vs_external_invalidate` | 1 | 0 of 100 | 0 of 100 | 0 of 100 | 0 of 100 |
| POC model on ZODB's checker | 4 | 0 of 600 | **98 of 100** | 0 of 600 | **81 of 100** |
| POC model on ZODB's checker | 16 | 0 of 100 | | 0 of 100 | |
| POC model on ZODB's checker | 1 | 0 of 100 | **68 of 100** | 0 of 100 | **2 of 100** |

Pool 1 with the rule on is one committing connection and the listener. Pool 1 with the rule off is the POC on one connection.

- **With the rule, no run failed:** 0 failures in 3,200 runs of the two race tests of ZODB, and 0 in 1,600 runs of the POC model, over both interpreters. Version 1 of the rule (`results/race-*-v1.jsonl`) passed 1,078 more runs with a pool of 4, on both interpreters, with 0 failures.
- **Without the rule, the pool of ZOE-077 fails** `test_race_loadopen_vs_local_invalidate`: 27 of 100 runs on 3.13 and 43 of 100 runs on 3.14t. A typical failure on 3.14t: `verify: obj1 (73) != obj2 (72)`, with `obj2._p_serial` older than `obj1._p_serial` while `loadBefore` gives both at the newer serial. The verifier started its snapshot after the commit of the modifier, but its cache still held the old `obj2`. This is the first fault of the rule: the POC listener moved `lastTransaction()` before the committing thread ran `f(tid)`.
- **Without the rule, `test_race_load_vs_external_invalidate` passes** in every run. Its model writes both objects in each commit, so a client with a stale cache gets a `ConflictError` at the vote, and the test ignores it. The fault is there, but the model cannot show it.
- **The POC model shows it.** Half of its steps change only `obj3`, so they commit with a stale cache. Without the rule, it fails in 98 of 100 runs on 3.13 and 81 of 100 on 3.14t with a pool of 4. A typical failure: the client's own commit of `obj3` at tid `…8abd` came after another client's commit of `obj1` and `obj2` at tid `…cdc4`, and the cache of the client still held `obj2` from before `…cdc4`. This is the second fault: the finish ran `f(tid)` before the listener delivered the earlier tid.
- **The POC on one connection fails the POC model too:** 68 of 100 runs on 3.13 and 2 of 100 on 3.14t. This is the third fault: its finish and its invalidations go to two threads. With the rule (one committing connection and a listener), 0 of 100 failed on each interpreter.

**The invariants of the spike.** Every run used the fork of `persistent`, 6.8+zoe.1 (each JSON line records the version), with the forks of `BTrees` and `zodbpickle`. On 3.14t, every run recorded `sys._is_gil_enabled()` false at its end, and no module turned the GIL back on while it loaded: the list of such modules is empty in each 3.14t run. The race tests ran unchanged; ZODB fixes their iterations, so the spike ran each test many times instead.

## Cost

The threads cells of zodbshootout on 3.14t, ops/s, the mean of 3 interleaved rounds (`results/ab-concurrent.jsonl`, summed in `results/cost-summary.txt`). The server is "zoe best" of #132. The pool has as many connections as the concurrency; with the rule, the listener adds one.

| Scenario | Concurrency | One connection | Pool without the rule | Pool with the rule |
| --- | --- | --- | --- | --- |
| `add` | 16 | 27,783 | 99,404 | 95,724 |
| `add` | 64 | 23,131 | 37,138 | 35,554 |
| `update` | 16 | 21,954 | 55,476 | 62,684 |
| `update` | 64 | 21,413 | 61,005 | 61,967 |

The spread of the rounds (lowest to highest) and the cores of client CPU:

| Cell | One connection | Pool, rule off | Pool, rule on |
| --- | --- | --- | --- |
| `add`, 16 | 27,262 to 28,498; 0.6 cores | 97,767 to 100,803; 2.0 cores | 93,682 to 97,304; 2.0 cores |
| `add`, 64 | 21,083 to 26,731; 0.9 cores | 36,345 to 37,562; 4.9 cores | 30,553 to 38,852; 5.0 cores |
| `update`, 16 | 20,827 to 23,635; 0.9 cores | 49,321 to 67,430; 2.6 cores | 50,234 to 70,398; 2.6 cores |
| `update`, 64 | 20,931 to 22,121; 1.0 cores | 58,787 to 63,001; 4.0 cores | 61,687 to 62,407; 4.6 cores |

- **The pool lifts the cap of one connection** by 1.5 to 3.6 times: one connection stays at 21,000 to 28,000 ops/s in every cell, the latency of one commit at a time, as #132 found (27,652 and 23,355 there at concurrency 16).
- **The rule costs little.** With the rule, `add` is 3.7 % lower at concurrency 16 and 4.3 % lower at 64, inside or near the spread of the rounds. `update` shows no cost: its rounds spread by more than the difference. A finish of the pool waits for one more delivery, the tid on the listener, and the pool holds one more socket.
- **`add` at concurrency 64 is lower than at 16 in every configuration.** The likely cause is in the POC, not in the rule: `new_oid` holds one lock across the `new_oids(100)` call on the first connection, and only `add` asks for new oids. It was not measured.
- **The numbers of #132 hold.** The pool without the rule gives 99,404 (`add`) and 55,476 (`update`) at concurrency 16, against 101,136 and 67,400 in #132, part 4. The rounds of `update` at 16 spread from 49,321 to 70,398 in both pool configurations.

Method notes: a developer workstation, not the reference machine. Each round held the exclusive lock of the benchmark lock file for its 12 runs, so no other heavy job of the machine ran during a round. Another agent ran builds and benchmarks between the rounds.

## Options

| | One connection, one transaction at a time | (a) A pool of connections with the rule | (b) Several transactions on one connection |
| --- | --- | --- | --- |
| Transactions at once in a process | 1 | up to the size of the pool | up to a limit of the session |
| Order of the invalidations | one stream, if the client delivers the finish and the invalidations in stream order (rule, point 7) | the stream of the listener, and the hold of point 6 | one stream, as with one connection |
| Wire protocol | ZEO 6, unchanged | ZEO 6, unchanged | an extension, declared in `register`: a change of the wire protocol |
| Against a stock ZEO server | works | works | falls back to (a) or to one transaction at a time |
| Change of `zoe serve` | none | none | a session that holds several open transactions, each on its own thread, with a lane of calls for each |
| Sockets for each process | 1 | one for each transaction at once, plus the listener; they can open on demand | 1 |
| Extra wait of a commit | none | the delivery of its tid by the listener, after the reply | none |
| What caps the threads cells | the latency of one commit | the server and the client process | the reader of one connection, and its window of calls |

**One connection.** It is the status quo of `ZEO.ClientStorage` and of the POC with `pool 1`. It caps the threads cells at the latency of one commit (see Cost). It is correct only when the reply of a finish and the invalidations go through one ordered stream (point 7).

**(a) A pool of connections.** Each transaction takes a free connection for its whole two-phase commit, so the commits of a process run in parallel on the server. It needs no change of the server or of the protocol, and it works on ZEO too. Its costs: a socket for each transaction at once, and a listener; the hold of point 6; and one more handoff before a finish returns. Connections that open on demand avoid the cost of 16 or 64 connections when a DB opens, which `read_cold` and `read_warm` pay, because they open a new DB for each loop (#132).

**(b) Several transactions on one connection.** The transaction id of `storea`, `vote` and `tpc_finish` tells the transactions apart. The order comes for free: the server publishes the commits of one store in commit order, and one connection gets its replies and its invalidations on one stream. The costs:

- A change of the wire protocol: an extension, which only the ADR can propose and the human decide. A stock ZEO server refuses a second `tpc_begin` with another id on a connection with `StorageTransactionError` ("Multiple simultaneous tpc_begin requests from one client"). `tpc_begin` has no reply, so the server closes the connection. The client still needs (a) or one transaction at a time there.
- `zoe serve` keeps one open transaction for each session today (`Transaction::open` in `crates/zoe-server/src/protocol/session.rs`): a second `tpc_begin` with another id leaves the open transaction as it is, and the calls of the other transaction fail. The core transaction stays on the thread that began it (`WrongThread`), so each open transaction of a session needs its own thread and its own lane of calls.
- The invalidations of a commit of the connection. ZEO never sends a client the invalidations of its own commits, and (b) keeps that: a commit of one transaction of the connection gives no invalidation to the connection. The client invalidates its other ZODB connections itself, through `f(tid)` and `_invalidate_finish` of ZODB's `MVCCAdapter`, as it does today. So (b) needs no new message for it.
- One reader and one window of calls on the server for all the transactions of a process. The window of a connection is 4 calls today (`QUEUE`), and a slow call of one transaction can delay the calls of the others. #132 found that a wider window does not change the numbers on one connection, but with (b) one connection carries the load of the whole process.

So (b) saves sockets, and it brings a protocol change, a second path in the client and a new state in the server.

## Recommendation

**Build (a), a pool of connections with the rule, in the native client of F28 (ZOE-155).** It keeps the contract of ZODB in every run of the race tests, it needs no change of the server or of the wire protocol, it works against a stock ZEO server, and it removes the cap of one commit at a time in the threads cells (see Cost). The ADR draft [0033](../adr/0033-concurrent-transactions-in-a-client.md) states the decision for the human.

For ZOE-155:

- Put the rule in Rust, not in Python. The reader task of the listener knows the order. It hands each tid in turn to one dispatcher, which runs `invalidate` or `f(tid)` and then moves `lastTransaction()`. With one connection, the reader puts the reply of a finish in the same stream (point 7). Then the order does not depend on which Python thread wakes first.
- Take the hold of version 2 (point 6): wait only for the finishes sent before the tid arrived.
- Open the connections of the pool on demand, up to a bound, so a DB that opens costs the listener and one connection.
- Treat a lost or lagged listener as a lost connection: fail the finishes that wait for it, `invalidateCache`, and connect again with `getInvalidations`.
- Run ZODB's race tests on both interpreters in the acceptance test of the client, and add a model whose steps commit without a conflict, as the POC model does: with ZODB's model alone, `test_race_load_vs_external_invalidate` passes on a client that breaks the contract.
- The client cache of ZOE-152 takes its invalidations from the same dispatcher, in the same order.

**Do not build (b) now.** It would save sockets only, at the price of a change of the wire protocol, a second path in the client, and sessions of `zoe serve` with several open transactions. If the sockets of many client processes become a limit, an extension can come later, on a card of its own and with the human's decision.
