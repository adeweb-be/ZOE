"""A ZODB storage on zoe-client, for the ZOE-077 and ZOE-151 POCs. Not for merge.

It has no client cache, no blobs, no undo and no conflict resolution. It is
enough for zodbshootout and for the race tests of ZODB: `%import
zoeclientx`, then a `<zoeclientx>` section with `server HOST:PORT`.

With `pool N`, each transaction takes a free connection among N, so N
transactions of one process can commit at the same time.

`rule off` (the default) is the pool of ZOE-077: the first connection reads
the invalidations and also commits. It skips a tid of the pool after a wait
of 5 ms, and it does not order the invalidations against the commits of the
other connections.

`rule on` is the candidate rule of ZOE-151:

- One connection, the listener, only reads the invalidations. The N
  connections of the pool commit. A pool of N opens N + 1 connections.
- The listener delivers each tid in the order of its stream. The server
  sends it the invalidations of every commit of the pool too, in tid order.
- A tid of another client goes to the DB as `invalidate(tid, oids)`.
- A tid of the pool never goes to the DB as an invalidation. The listener
  runs the `func(tid)` of its `tpc_finish` instead, so the order is that of
  the stream.
- `lastTransaction()` moves to a tid only after the listener delivered it.
- While a finish of the pool waits for its reply, the listener holds a tid
  that it does not know: the tid can be the one of that finish. It holds
  the tid only until the finishes that were sent before the tid arrived
  have their replies. A finish sent later cannot commit an earlier tid.
- `tpc_finish` returns only after the listener delivered its tid.
"""

import queue
import threading

from ZODB.config import BaseConfig
from ZODB.POSException import ConflictError
from ZODB.POSException import POSKeyError
from ZODB.utils import maxtid

from ._zoeclientx import Conflict
from ._zoeclientx import ZClient


class ZoeClientStorage:
    def __init__(self, server, name="zoeclientx", pool=1, rule=False):
        self._server = server
        self._name = name
        self._rule = bool(rule)
        # The first connection reads the invalidations. With the rule, it
        # does not commit.
        self._client = ZClient(server)
        if self._rule:
            self._committers = [ZClient(server) for _ in range(pool)]
            self._clients = [self._client] + self._committers
        else:
            self._clients = [self._client] + [ZClient(server) for _ in range(pool - 1)]
            self._committers = self._clients
        self._free = queue.SimpleQueue()
        for client in self._committers:
            self._free.put(client)
        self._ltid = max(c.last_transaction() for c in self._clients)
        # Without the rule: the tids of the commits of this process on the
        # other connections of the pool.
        self._own = set()
        self._own_cond = threading.Condition()
        self._ltid_lock = threading.Lock()
        # With the rule: the number of finishes sent, the numbers of the
        # finishes that wait for their reply, the callbacks of the finishes
        # that wait for the listener, and the errors of the callbacks.
        self._cv = threading.Condition()
        self._sent = 0
        self._unreplied = set()
        self._pending = {}
        self._errors = {}
        self._db = None
        self._txns = {}
        self._oids = []
        self._oid_lock = threading.Lock()
        self._closed = threading.Event()
        target = self._listen if self._rule else self._invalidations
        self._thread = threading.Thread(target=target, daemon=True, name="zoeclientx")
        self._thread.start()

    # Without the rule: the POC of ZOE-077.

    def _invalidations(self):
        while not self._closed.is_set():
            got = self._client.next_invalidation(0.2)
            if got is None:
                continue
            tid, oids = got
            if len(self._clients) > 1 and self._is_own(tid):
                self._advance(tid)
                continue
            # Invalidate first, then move the last tid. A connection that
            # sees the new tid then also sees the invalidation.
            if self._db is not None:
                self._db.invalidate(tid, oids)
            self._advance(tid)

    def _is_own(self, tid):
        # The invalidation can come before the reply of the finish. Wait a
        # short time for the finish of this process to name the tid.
        with self._own_cond:
            self._own_cond.wait_for(lambda: tid in self._own, timeout=0.005)
            if tid in self._own:
                self._own.discard(tid)
                return True
            return False

    def _advance(self, tid):
        with self._ltid_lock:
            if tid > self._ltid:
                self._ltid = tid

    # With the rule.

    def _listen(self):
        while not self._closed.is_set():
            got = self._client.next_invalidation(0.2)
            if got is None:
                continue
            tid, oids = got
            with self._cv:
                # Hold the tid while a finish of the pool that was sent
                # before it arrived waits for its reply: the tid can be the
                # one of that finish. A later finish commits a later tid.
                horizon = self._sent
                self._cv.wait_for(
                    lambda: tid in self._pending
                    or not any(n <= horizon for n in self._unreplied)
                )
                func = self._pending.pop(tid, None)
            try:
                if func is not None:
                    func(tid)
                elif self._db is not None:
                    self._db.invalidate(tid, oids)
            except BaseException as error:  # noqa: BLE001 - give it to the finish
                with self._cv:
                    self._errors[tid] = error
            with self._cv:
                if tid > self._ltid:
                    self._ltid = tid
                self._cv.notify_all()

    def _finish_in_order(self, client, txn, func):
        with self._cv:
            self._sent += 1
            number = self._sent
            self._unreplied.add(number)
        try:
            tid = txn.finish()
        except BaseException:
            with self._cv:
                self._unreplied.discard(number)
                self._cv.notify_all()
            raise
        finally:
            self._free.put(client)
        with self._cv:
            self._pending[tid] = func
            self._unreplied.discard(number)
            self._cv.notify_all()
            self._cv.wait_for(lambda: self._ltid >= tid)
            error = self._errors.pop(tid, None)
        if error is not None:
            raise error
        return tid

    # The storage API.

    def registerDB(self, db):
        self._db = db

    def lastTransaction(self):
        return self._ltid

    def getName(self):
        return self._name

    def sortKey(self):
        return f"zoeclientx:{self._server}"

    def getSize(self):
        return 0

    def __len__(self):
        return 0

    def isReadOnly(self):
        return False

    def supportsUndo(self):
        return False

    def new_oid(self):
        with self._oid_lock:
            if not self._oids:
                self._oids = list(reversed(self._client.new_oids(100)))
            return self._oids.pop()

    def loadBefore(self, oid, tid):
        client = self._clients[threading.get_ident() % len(self._clients)]
        try:
            return client.load_before(oid, tid)
        except KeyError:
            raise POSKeyError(oid) from None

    def load(self, oid, version=""):
        record = self.loadBefore(oid, maxtid)
        if record is None:
            raise POSKeyError(oid)
        return record[:2]

    def tpc_begin(self, transaction, tid=None, status=" "):
        client = self._free.get()
        try:
            self._txns[id(transaction)] = (client, client.begin())
        except BaseException:
            self._free.put(client)
            raise

    def store(self, oid, serial, data, version, transaction):
        self._txns[id(transaction)][1].store(oid, serial, data)

    def tpc_vote(self, transaction):
        try:
            self._txns[id(transaction)][1].vote()
        except Conflict as e:
            raise ConflictError(str(e)) from None

    def tpc_finish(self, transaction, func=lambda tid: None):
        client, txn = self._txns.pop(id(transaction))
        if self._rule:
            return self._finish_in_order(client, txn, func)
        try:
            tid = txn.finish()
        finally:
            self._free.put(client)
        if client is not self._client:
            with self._own_cond:
                self._own.add(tid)
                self._own_cond.notify_all()
        # Invalidate the other connections first, then move the last tid.
        func(tid)
        self._advance(tid)
        return tid

    def tpc_abort(self, transaction):
        entry = self._txns.pop(id(transaction), None)
        if entry is not None:
            client, txn = entry
            try:
                txn.abort()
            finally:
                self._free.put(client)

    def close(self):
        self._closed.set()
        self._thread.join()


class StorageConfig(BaseConfig):
    def open(self, database_name="unnamed", databases=None):
        c = self.config
        return ZoeClientStorage(c.server, c.name, c.pool, c.rule)
