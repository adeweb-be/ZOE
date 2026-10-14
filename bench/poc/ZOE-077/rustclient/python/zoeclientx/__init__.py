"""A ZODB storage on zoe-client, for the ZOE-077 POC. Not for merge.

It has no client cache, no blobs, no undo and no conflict resolution. It is
enough for zodbshootout: `%import zoeclientx`, then a `<zoeclientx>` section
with `server HOST:PORT`.

With `pool N`, the storage opens up to N connections. Each transaction
takes a free connection, so N transactions of one process can commit at the
same time. The storage opens a new connection only when a transaction finds
no free one, so a process with one thread keeps one connection. The first
connection reads the invalidations. The POC does not order the
invalidations of the other connections against the commits of this process:
a real client needs that.
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
    def __init__(self, server, name="zoeclientx", pool=1):
        self._server = server
        self._name = name
        self._client = ZClient(server)
        self._clients = [self._client]
        self._pool = pool
        self._pool_lock = threading.Lock()
        self._free = queue.SimpleQueue()
        self._free.put(self._client)
        self._ltid = self._client.last_transaction()
        # The tids of the commits of this process on the other connections
        # of the pool. The first connection gets their invalidations too.
        self._own = set()
        self._own_cond = threading.Condition()
        self._ltid_lock = threading.Lock()
        self._db = None
        self._txns = {}
        self._oids = []
        self._oid_lock = threading.Lock()
        self._closed = threading.Event()
        self._thread = threading.Thread(target=self._invalidations, daemon=True, name="zoeclientx")
        self._thread.start()

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
        clients = self._clients
        client = clients[threading.get_ident() % len(clients)]
        try:
            return client.load_before(oid, tid)
        except KeyError:
            raise POSKeyError(oid) from None

    def load(self, oid, version=""):
        record = self.loadBefore(oid, maxtid)
        if record is None:
            raise POSKeyError(oid)
        return record[:2]

    def _take(self):
        try:
            return self._free.get_nowait()
        except queue.Empty:
            pass
        with self._pool_lock:
            if len(self._clients) < self._pool:
                client = ZClient(self._server)
                self._clients = self._clients + [client]
                return client
        return self._free.get()

    def tpc_begin(self, transaction, tid=None, status=" "):
        client = self._take()
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
        return ZoeClientStorage(self.config.server, self.config.name, self.config.pool)
