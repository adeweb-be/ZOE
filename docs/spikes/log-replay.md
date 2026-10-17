# Spike: the log of a B5 run, and how the replay rebuilds its concurrency

Card ZOE-208, step 1; B6 of section 11 of the specification and the success criterion on `ConflictError`s. The harness is `bench/replay`; `bench/README.md` ("The log replay") describes it.

**Verdict.** The probe records of a B5 run give the start of every transaction of its kept store: in the run of this spike, each of the 1,720 transactions after the fixture matched the request that committed it. A transaction starts at the start of its request and commits at its tid. At 1× on `zeo`, the replay then refuses 164 commits, and in the run 168 of the same 1,720 transactions needed more than one attempt: by these totals, the replay rebuilds the first-attempt failures of the run, without the retries of Zope; the finding does not match each refused tid with a retried request. On `main`, both stacks raise 95.3 `ConflictError`s per 1,000 transactions. Two properties limit what the replay measures, and the second one needs the human. First, a vote that refuses one oid hides the oids that it resolved before the refusal, on both stacks. Second, the schedule of the acceptance test scales every event by the speed, so the order of the events, and every overlap, is the same at 1×, 4× and 16×: the counts do not change with the speed.

## Question

Step 1 of the card: the log is the transactions of a store that a B5 run kept (`--keep-store`), read through the iterator, with the start of each transaction from the probe records of the run. How does the replay rebuild the concurrency of the run from them, and does it on one run?

## Method

- `main` at `3b5164f`. One B5 run: `just bench plone run --stack zeo --scenario edit --steps 4:60,16:60 --keep-store`, on the small fixture. It kept `store/filestorage/Data.fs` and the probe records of both instances, `probe/instance0.jsonl` and `probe/instance1.jsonl`. Its verification passed.
- `just bench replay RUN` read the log and replayed it on both stacks at 1×, 4× and 16×, one repetition, in the interpreter of the zoeclient Plone instance, with `bench/replay` at `2ece33d` and a clean tree, as each result records. `zoe serve` was the release build of `main`; the native client was the package `zoe` of the zoeclient instance, built from `main`. A debug run replayed the first 300 transactions and compared, at each store, the serial that the transaction read with the serial on the server.
- Machine: Apple M3 Pro, CPython 3.13, both servers with `fsync(2)`.

## The log

- **The base.** The store holds 1,743 transactions. The 23 before the first request of the probes are the fixture and the start of the instances; each run restores them into its new store first, with their tids.
- **The commit** of a transaction is the time of its tid. The ZEO server gives the tid at the vote, under the commit lock, so it is the time of the commit to within the time of one vote.
- **The start.** A probe record of a request gives its wall-clock start `t`, its time `seconds`, its number of attempts and its number of `tpc_finish` calls. Zope begins the transaction of an attempt just after the probe sees the request, and ZODB reads every object of the transaction at the snapshot of that start. A tid belongs to the request that committed, that started before the tid and that ended first after it. Each of the 1,720 tids matched one: a tid comes a median of 4 ms before the end of its request (95th percentile 15 ms). 917 windows of a committing request hold only its own tid; the others also hold the tids of requests that overlap it, which is why the match takes the request that ends first. The transaction starts at `t`, the start of the first attempt of its request, also when the request took several attempts. The replay does not retry, so the attempt whose `ConflictError` reaches Zope is the first one, and the window from `t` to the tid holds every commit that the first attempt could meet, and the commits after the failure of that attempt too. A first version of the replay took the start of the last attempt instead (the end of the request minus the time of one attempt). At that start, 140 transactions overlap the commit of another one on one of their oids, 136 of them retried in the run; at `t`, 171, with 167 of the 168 retried transactions. The last attempt of a retried request succeeded on ZEO, so its own window holds almost no conflict: the start of the last attempt loses the conflicts of the first one.
- **What commits.** The edit form commits on its GET too: `plone.locking` writes the lock of the Document. So an edit is two transactions, the lock and the save.
- **Durations.** From start to commit, the median is 87 ms, the 95th percentile 465 ms and the longest 1.6 s. The log covers 188 s: the warm-up and the two steps.

## The check on one run

| | the run | the replay at 1× on `zeo` |
| --- | ---: | ---: |
| transactions of the log | 1,720 | 1,720 |
| transactions with more than one attempt (run) or refused (replay) | 168 | 164 |
| `ConflictError`s that Zope saw, retries included | 281 | (no retries) |
| conflicts that the ZEO server resolved | 4,164 | 0 reported |

The refusals of the replay are on the posting buckets of ZCTextIndex (`IOBucket`, 151), on `PersistentMapping` (6), on `OOBucket`s (5) and on `OOBTree`s (2). The run raised more `ConflictError`s than it had retried transactions because a retry starts after the failure and can fail again: 55 requests took three or four attempts. The replay does not retry, so each transaction fails at most once. The transactions of the log that overlap the commit of another one on one of their oids are 171, and 167 of them retried in the run: the overlaps of the replay are the conflicts of the run.

Why the replay reports almost no resolved conflict, while the debug run found 1,873 stores with a stale serial in the first 300 transactions (12,310 stores; `IOBucket` 1,050, `Length` 431, `IIBTree` 238):

- A ZEO server resolves the conflicts of a vote in the order of the stores, and the first refusal fails the vote with `ConflictError`. The oids that it resolved before are in no reply. `zoe serve` does the same: the client resolves the listed conflicts in turn, and the first refusal raises. So a count by path holds the conflicts of the votes that pass, and the refused oid of each vote that fails.
- In this log, nearly every pair of overlapping saves changes one posting bucket of a text index, which ZODB's resolver refuses (`docs/spikes/posting-dicts.md`). So nearly every vote with a conflict fails, and its resolved `Length`s and buckets do not show. Only 4 transactions that did not retry in the run overlap another one.
- The run of B5 counts each call of the resolver in the ZEO server, also in the votes that then fail, and in each retry. Its 4,164 resolutions are of that kind.

## The speeds

The acceptance test fixes the schedule: at the speed s, an event that the log puts t seconds after its first start comes t / s seconds after the start of the run, for the starts and the commits alike. One scale on every event keeps their order, so the same transactions overlap at every speed, and the counts are the same at 1×, 4× and 16×. A faster speed only compresses the time of the run until the server cannot keep up; then the one thread of the replay plays the events late, still in the order of the log. The card says "a faster replay overlaps more transactions": that needs a schedule in which the starts come s times sooner and each transaction keeps its duration. Such a schedule changes the order of the commits, so it contradicts the acceptance test ("a faster replay keeps the order of the log"). This is a question for the human (see below).

## The baseline on `main`

`just bench replay` of the run above, `main` at `3b5164f`, one repetition:

| stack | speed | transactions | `ConflictError`s | per 1,000 | acknowledged | seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| zeo | 1× | 1,720 | 164 | 95.3 | 1,556 | 188.0 |
| zoe-native | 1× | 1,720 | 164 | 95.3 | 1,556 | 188.0 |
| zeo | 4× | 1,720 | 164 | 95.3 | 1,556 | 47.0 |
| zoe-native | 4× | 1,720 | 164 | 95.3 | 1,556 | 47.0 |
| zeo | 16× | 1,720 | 164 | 95.3 | 1,556 | 17.8 |
| zoe-native | 16× | 1,720 | 164 | 95.3 | 1,556 | 15.1 |

Every run counts: each record of each acknowledged transaction loads at its tid. Conflicts per 1,000 transactions, by class, all refused: `zeo`: `IOBucket` 87.8, `PersistentMapping` 3.5, `OOBucket` 2.9, `OOBTree` 1.2. `zoe-native`: `IOBucket` 74.4, `IIBTree` 7.0, `OOBucket` 6.4, `PersistentMapping` 3.5, `IITreeSet` 2.3, `IIBucket` 1.2, `OOBTree` 0.6. The two stacks refuse the same number of commits; the refused oid of a vote is not always the same one, so the classes differ.

The gate fails on `main`: `zoe-native` raises 100 % of the `ConflictError`s of `zeo`, as expected before the merges of phase 4.

## The own change of a transaction

The log holds the record of each transaction as the store committed it, so the record of the second of two overlapping writes already holds the change of the first. The replay stores the change of the transaction instead: from the record of the oid before it in the log to its own record, applied to the state that it read. `_p_resolveConflict` of the class does it first. For a posting bucket of ZCTextIndex it refuses, because ZODB's resolver cannot merge two dicts, and the transaction then stored its own record, which holds the inserts of the other transaction: a bucket refuses two inserts of one key, so the replay made conflicts that the change of the transaction does not make. Where the resolver refuses on a bucket, a set or a BTree of one bucket, the replay puts the keys that the transaction added, changed or deleted, each with its whole value, into the items that it read (`replay.runner.apply_items`). Measured with the start of the last attempt of a request, with `zoe serve` of ZOE-203 and its posting-dict rule on, this took `zoe-native` from 128 refused commits at 1× to 55; with ZODB's resolver alone (`zeo`, and `zoe-native` on `main`), it changed nothing. Each run counts how its records came to be. On `main`, a run builds 106,157 records, with those of the refused commits: 48,955 as their own record (the transaction read the record before it), 35,788 through the resolver of the class, 20,890 through their items, and 524 (0.5 %) as their own record because neither applied: a class without a resolver, such as `PersistentMapping`, or a BTree of more than one bucket.

## A preview of the gate

The same log at 1×, `zoe-native` against `zoe serve` built in release from `card/ZOE-203` at `a5e210d` (sha256 `82ba8b38…`, which the result records) with `--zoe-serve-arg=--posting-dicts --zoe-serve-arg=on`, one repetition, beside `zeo` of the baseline. The native client stays the one of `main`: ZOE-203 does not change it.

| stack | `ConflictError`s | per 1,000 | acknowledged |
| --- | ---: | ---: | ---: |
| zeo | 164 | 95.3 | 1,556 |
| zoe-native, ZOE-203 | 95 | 55.2 | 1,625 |

`zoe-native` raises 58 % of the `ConflictError`s of `zeo`: the gate of 10 % fails with ZOE-203 alone. The merges at the vote, per 1,000 transactions: `IOBucket` 1,276.2, `Length` 507.0, `IIBTree` 359.3, `IIBucket` 111.0, `OOBucket` 49.4, `IITreeSet` 44.2, `IISet` 10.5; 3.5 `IOBucket`s went to the client. The refusals that are left: `IIBTree` 30.8, `IOBucket` 15.1, `OOBucket` 4.1, `PersistentMapping` 3.5, `OOBTree` 1.2, `IITreeSet` 0.6.

## Consequences for the card

- The replay counts what the gate needs: the `ConflictError`s that reach Zope, per 1,000 transactions, the same log on both stacks.
- The counts by path show the conflicts of the votes that pass and the refused oid of each vote that fails. They are not the count of resolutions in the run, and the report says so.
- The posting buckets dominate the refusals, as in B5. With the posting-dict rule of ZOE-203 on, `zoe-native` raises 58 % of the `ConflictError`s of `zeo` on this log; the gate of phase 4 depends on the other cards of the phase, and `IIBTree` leads the refusals that are left.

## Not measured

- More than one repetition, other scenarios (`create-spread`, `mixed`), the medium fixture, Linux.
- Blobs: the replay restores and stores the records of the blobs, not their files.
- A schedule with more overlap at the faster speeds (see the question).

## Questions for the human

1. **The speeds.** With the schedule of the acceptance test, 1×, 4× and 16× replay the same overlaps and give the same counts. Do the speeds stay as they are (one gate at the overlaps of the run), or does a speed s bring the starts s times sooner and keep the duration of each transaction, so that faster speeds overlap more? The second reading changes the acceptance test: the order of the commits then changes with the speed.
