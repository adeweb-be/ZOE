"""Acceptance test: the `length` that `zoe serve` sends is the object count.

A stock `ZEO.ClientStorage` gives `len()` from the `length` of `get_info`
at its registration, then from the `info {length, size}` that the server
sends to the client that commits, and to every client of the storage after
a pack. `zoe serve` sends the object count of the store there, and in
`get_size_info`.
"""

import time

import transaction
import ZODB
from persistent.mapping import PersistentMapping

from zoe.testing import ZoeServer

TIMEOUT = 30


def client(server, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", **kw)


def test_the_length_follows_commits_and_packs(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        db = ZODB.DB(client(server))
        try:
            # The database commits the root through the client.
            assert len(db.storage) == 1
            tm = transaction.TransactionManager()
            conn = db.open(transaction_manager=tm)
            for name in "abc":
                conn.root()[name] = PersistentMapping()
            tm.commit()
            assert len(db.storage) == 4
            assert db.storage._call("get_size_info")["length"] == 4

            # A new client gets the count at its registration.
            other = client(server)
            try:
                assert len(other) == 4
            finally:
                other.close()

            del conn.root()["c"]
            tm.commit()
            conn.close()
            assert len(db.storage) == 4
            # The info after the pack gives the new count.
            db.pack(time.time() + 1)
            assert len(db.storage) == 3
        finally:
            db.close()
