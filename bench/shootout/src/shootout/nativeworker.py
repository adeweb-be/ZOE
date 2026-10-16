"""The worker processes of the native gate.

The driver in `shootout.native` starts each process with the spawn method
and talks to it on a pipe. A process opens one DB on the ZConfig text of
its storage, and runs one thread for each of its workers. In the threads
mode, one process runs every worker of a cell on one shared DB, as one
Zope process does. In the processes mode, each worker has its own process.

The work of each scenario is the work of zodbshootout 0.8 for one
transaction, with its objects and its data (`SpeedTestData`):

- `add`: put new objects in the mapping of the worker, and commit;
- `update`: change each object of the mapping, and commit;
- `read_cold`: with no cache, open a connection and read each object;
- `read_warm`: with no cache, open a connection, change each object,
  commit, read each object, and commit;
- `read_hot`: empty the cache of a primed connection, and read each object
  from the cache of the storage.

Before a batch of `read_cold` or `read_warm`, the process empties the
cache of each connection and the cache of the storage, as zodbshootout
does. That work is not timed.

The driver sends one command at a time. The process answers each one with
`("ok", value)`, or with `("error", traceback)`, and then it stops.
"""

import contextlib
import gc
import queue
import threading
import time
import traceback

import transaction
import ZODB.config
from persistent.mapping import PersistentMapping
from zodbshootout.speedtest import SpeedTestData

# The key of the mappings of the workers in the root.
KEY = "speedtest"


def main(pipe, config: str, workers: list[int], concurrency: int, objects: int, size):
    """Run the commands of the driver until it sends `stop`."""
    db = None
    threads = []
    try:
        db = ZODB.config.databaseFromString(config)
        # Each worker opens up to three connections at once. A larger pool
        # keeps ZODB from warning about them.
        db.setPoolSize(3 * len(workers) + 7)
        data = SpeedTestData(concurrency, objects, size)
        pipe.send(("ok", None))
        while True:
            command, *args = pipe.recv()
            if command == "stop":
                break
            if command == "populate":
                _populate(db, data, concurrency)
                pipe.send(("ok", None))
            elif command == "open":
                threads = [_Worker(db, data, worker) for worker in workers]
                pipe.send(("ok", None))
            elif command == "batch":
                [scenario] = args
                if scenario in ("read_cold", "read_warm"):
                    _clear_caches(db)
                pipe.send(("ok", _batch(threads, scenario)))
            else:
                raise ValueError(f"unknown command {command!r}")
    except BaseException:
        # The driver can be gone already.
        with contextlib.suppress(OSError):
            pipe.send(("error", traceback.format_exc()))
    finally:
        for thread in threads:
            thread.stop()
        if db is not None:
            db.close()
        pipe.close()


def _populate(db, data: SpeedTestData, concurrency: int) -> None:
    """Commit the mapping of each worker, full of objects."""
    manager = transaction.TransactionManager(explicit=True)
    conn = db.open(manager)
    manager.begin()
    conn.root()[KEY] = PersistentMapping(
        {w: PersistentMapping(data.data_to_store()) for w in range(concurrency)}
    )
    manager.commit()
    conn.close()


def _clear_caches(db) -> None:
    """Empty the cache of each connection and the cache of the storage."""
    for conn in db.pool.all:
        conn.cacheMinimize()
    if hasattr(db.storage, "_cache"):
        db.storage._cache.clear()
    gc.collect()


def _batch(threads, scenario: str) -> list[float]:
    """Run one transaction of scenario in each thread. Return their times."""
    for thread in threads:
        thread.inbox.put(scenario)
    times = [thread.outbox.get() for thread in threads]
    if errors := [t for t in times if isinstance(t, str)]:
        raise RuntimeError(errors[0])
    return times


class _Worker:
    """One worker: a thread with its own connections."""

    def __init__(self, db, data: SpeedTestData, number: int):
        self.db = db
        self.data = data
        self.number = number
        self.objects = data.objects_per_txn
        self.inbox = queue.SimpleQueue()
        self.outbox = queue.SimpleQueue()
        # add and update run on one connection with an explicit
        # transaction manager, as in zodbshootout.
        self.manager = transaction.TransactionManager(explicit=True)
        self.conn = db.open(self.manager)
        # read_hot opens its connection at its first transaction.
        self.hot = None
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.inbox.put(None)
        self.thread.join()

    def _run(self) -> None:
        while (scenario := self.inbox.get()) is not None:
            try:
                self.outbox.put(getattr(self, scenario)())
            except BaseException:
                self.outbox.put(traceback.format_exc())
        if self.hot is not None:
            self.hot.close()
        self.conn.close()

    def _mapping(self, conn):
        return conn.root()[KEY][self.number]

    def _check(self, got: int) -> None:
        if got != self.objects:
            raise AssertionError(f"read {got} objects, not {self.objects}")

    def add(self) -> float:
        begin = time.perf_counter()
        self.manager.begin()
        self._mapping(self.conn).update(self.data.data_to_store())
        self.manager.commit()
        return time.perf_counter() - begin

    def update(self) -> float:
        begin = time.perf_counter()
        self.manager.begin()
        got = self.data.write_test_update_values(self._mapping(self.conn).values())
        self.manager.commit()
        elapsed = time.perf_counter() - begin
        self._check(got)
        return elapsed

    def read_cold(self) -> float:
        manager = transaction.TransactionManager()
        begin = time.perf_counter()
        conn = self.db.open(manager)
        got = self.data.read_test_read_values(self._mapping(conn).values())
        elapsed = time.perf_counter() - begin
        loads, _ = conn.getTransferCounts(True)
        conn.close()
        self._check(got)
        if loads < self.objects:
            raise AssertionError(f"loaded {loads} objects, not {self.objects}")
        return elapsed

    def read_warm(self) -> float:
        manager = transaction.TransactionManager()
        begin = time.perf_counter()
        conn = self.db.open(manager)
        values = self._mapping(conn).values()
        self.data.write_test_update_values(values)
        manager.commit()
        got = self.data.read_test_read_values(values)
        manager.commit()
        elapsed = time.perf_counter() - begin
        conn.close()
        self._check(got)
        return elapsed

    def read_hot(self) -> float:
        if self.hot is None:
            # Prime the caches: the connection and the storage hold each
            # object.
            self.hot = self.db.open(transaction.TransactionManager())
            self._check(
                self.data.read_test_read_values(self._mapping(self.hot).values())
            )
        self.hot.cacheMinimize()
        mapping = self._mapping(self.hot)
        begin = time.perf_counter()
        got = self.data.read_test_read_values(mapping.values())
        elapsed = time.perf_counter() - begin
        self._check(got)
        return elapsed
