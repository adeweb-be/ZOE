"""Acceptance test of the writes-on-GET report.

A GET should write nothing, but Plone and its add-ons write on some GETs:
a lock of an edit form, a last-login time, a lazy migration. `zoe.plone`
marks each transaction that a GET or a HEAD request commits. `zoe serve`
counts the records of each marked transaction by class, in
`GET /metrics`. A transaction of another request, or of no request, counts
nothing. A write of a KV namespace (ADR 0043) is no record, so it counts
nothing.

The test checks, through `zoe serve` with `ZEO.ClientStorage` and with
`zoe.zeo.ClientStorage` with the native client inside:

- a GET that writes counts each record of each class that it writes, new
  objects too, and a HEAD counts as a GET;
- a GET that writes nothing counts nothing, nor does a GET whose only
  write is a namespace key;
- a POST counts nothing, nor does a transaction of no request.

Each test that checks that nothing counts first runs a GET that writes, so
the count works in that run. Plone is not installed in the environment of
these tests: `as_request` marks a transaction as the subscriber of
`zoe.plone` marks the transaction of a request. `just test` runs it.
"""

import contextlib
import re

import pytest
from support import TIMEOUT, client, metrics, native

from zoe.testing import ZoeServer

# The clients of zoe serve.
MODES = ("zeo-client", "native-client")
MAPPING = "persistent.mapping.PersistentMapping"
LENGTH = "BTrees.Length.Length"
BUCKET = "BTrees.OOBTree.OOBucket"


def as_request(manager, method):
    """Make the current transaction of manager the transaction of a
    request of method, as the subscriber of `zoe.plone` does.

    The card does not name the mark, nor how the server reads it. The
    change of the specification of the card names them. The human approves
    a change of this function with it.
    """
    from zoe.plone import report

    report.mark(manager.get(), method)


# The line of a counter of the report, and its labels.
COUNTER = re.compile(r"^zoe_writes_on_get_total\{(?P<labels>[^}]*)\} (?P<value>\S+)$")
LABEL = re.compile(r'(\w+)="((?:[^"\\]|\\.)*)"')


def counts(text):
    """Return the records of GET transactions by class, from the text of
    `GET /metrics`.

    The card does not name the metric. The change of the specification of
    the card names it. The human approves a change of this function with
    it. Until then, the function reads the counter
    `zoe_writes_on_get_total{storage="1",class="module.Name"}`.
    """
    found = {}
    for line in text.splitlines():
        match = COUNTER.match(line)
        if match is None:
            continue
        labels = dict(LABEL.findall(match["labels"]))
        if labels.get("storage") == "1":
            found[labels["class"]] = int(float(match["value"]))
    return found


class Server(ZoeServer):
    """A `zoe serve` process with an admin port, to read `/metrics`."""

    def __init__(self, path):
        super().__init__(str(path), args=["--admin-listen", "127.0.0.1:0"])

    def __enter__(self):
        return self.start(TIMEOUT)

    def report(self):
        """Return the counts of the report, from `GET /metrics`."""
        return counts(metrics(self))


@contextlib.contextmanager
def served(mode, tmp_path):
    """Run a server and open a database of mode on it. Yield both."""
    import ZODB

    with Server(tmp_path / "store") as server:
        storage = client(server) if mode == "zeo-client" else native(server)
        db = ZODB.DB(storage)
        try:
            yield server, db
        finally:
            db.close()


def prepare(db):
    """Add the objects of the tests, in a transaction of no request."""
    from BTrees.Length import Length
    from persistent.mapping import PersistentMapping

    with db.transaction() as conn:
        root = conn.root()
        root["a"] = PersistentMapping(v=0)
        root["b"] = PersistentMapping(v=0)
        root["n"] = Length(0)
        root["folder"] = PersistentMapping()


def request(server, db, method, change):
    """Commit change(root) in a transaction of a request of method, or of
    no request when method is None. Return the counts that it added."""
    import transaction

    before = server.report()
    manager = transaction.TransactionManager()
    conn = db.open(transaction_manager=manager)
    try:
        manager.begin()
        if method is not None:
            as_request(manager, method)
        change(conn)
        manager.commit()
    finally:
        manager.abort()
        conn.close()
    after = server.report()
    added = {name: after.get(name, 0) - before.get(name, 0) for name in after}
    return {name: count for name, count in added.items() if count}


def write_three_classes(conn):
    from BTrees.OOBTree import OOBucket

    root = conn.root()
    root["a"]["v"] += 1
    root["b"]["v"] += 1
    root["n"].change(1)
    root["folder"][f"new-{len(root['folder'])}"] = OOBucket()


def write_one_mapping(conn):
    conn.root()["a"]["v"] += 1


def read_only(conn):
    assert conn.root()["a"]["v"] >= 0


@pytest.mark.parametrize("mode", MODES)
def test_a_get_that_writes_counts_each_class_that_it_writes(mode, tmp_path):
    with served(mode, tmp_path) as (server, db):
        prepare(db)
        first = request(server, db, "GET", write_three_classes)
        second = request(server, db, "HEAD", write_one_mapping)
        third = request(server, db, "GET", write_three_classes)
    # Two mappings, the folder, the Length and the new bucket.
    assert first == {MAPPING: 3, LENGTH: 1, BUCKET: 1}
    assert second == {MAPPING: 1}
    assert third == first


@pytest.mark.parametrize("mode", MODES)
def test_a_get_that_writes_nothing_counts_nothing(mode, tmp_path):
    with served(mode, tmp_path) as (server, db):
        prepare(db)
        assert request(server, db, "GET", write_one_mapping) == {MAPPING: 1}
        assert request(server, db, "GET", read_only) == {}
        assert request(server, db, "HEAD", read_only) == {}


def test_a_get_whose_only_write_is_a_namespace_key_counts_nothing(tmp_path):
    import zoe.kv

    def write_a_key(conn):
        zoe.kv.Namespace(conn, "ephemeral", "locks").set(b"lock", b"token", ttl=600)

    with served("native-client", tmp_path) as (server, db):
        prepare(db)
        assert request(server, db, "GET", write_one_mapping) == {MAPPING: 1}
        assert request(server, db, "GET", write_a_key) == {}


@pytest.mark.parametrize("mode", MODES)
def test_a_post_counts_nothing(mode, tmp_path):
    with served(mode, tmp_path) as (server, db):
        prepare(db)
        assert request(server, db, "GET", write_one_mapping) == {MAPPING: 1}
        assert request(server, db, "POST", write_three_classes) == {}
        assert request(server, db, None, write_three_classes) == {}
