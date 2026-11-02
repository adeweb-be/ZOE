"""The reader of ZOE: a ZODB record decodes in Rust, in place of ZODB's
`ObjectReader` (ADR 0057).

`Reader` is ZODB's `ObjectReader` with `setGhostState` and `getState` in
Rust (`zoe._zoe.Reader`). `Connection.setstate` keeps its lines: the load
through the storage, the bookkeeping and the blob. The reader decodes the
record, resolves its persistent references in the pickle cache and calls
`__setstate__`. The state equals the state of `ObjectReader.getState`. A
record that the Rust path does not decode goes to `ObjectReader` whole.

`Connection` is ZODB's `Connection` with a `Reader`. `DB.klass` is the
class that `DB.open` makes, so `install(db)` sets it, and gives a `Reader`
to the connections that `db` already has. Zope reaches the same attribute
with `connection-class zoe.reader.Connection` in a `<zodb_db>` section,
but only for the connections that the database opens after the first one.

`counts()` gives the counts of the readers of the process: the records
that the Rust path decoded (`decoded`), the records that took the Python
path by reason (`fallback`), and the opcodes that sent a record there
(`opcodes`).
"""

from ZODB.Connection import Connection as _Connection
from ZODB.serialize import ObjectReader

from zoe._zoe import Reader as _Reader
from zoe._zoe import _reader_counts as counts

__all__ = ["Connection", "Reader", "counts", "install"]


class Reader(_Reader, ObjectReader):
    """`ObjectReader` with `setGhostState` and `getState` in Rust."""


class Connection(_Connection):
    """ZODB's `Connection` with a `Reader`."""

    def __init__(self, db, *args, **kwargs):
        super().__init__(db, *args, **kwargs)
        self._reader = Reader(self, self._cache, self._db.classFactory)


def install(db):
    """Give the reader to the connections of `db`.

    Each new connection of `db` is a `Connection`, when `db.klass` is
    ZODB's `Connection`. Each connection that `db` already has gets a
    `Reader` with the class factory of its reader. A storage whose
    `native_reader` is false, the key `native-reader off` of `<zoe>` and
    of `<zoeclient>`, gets nothing.
    """
    if not getattr(db.storage, "native_reader", True):
        return
    if db.klass is _Connection:
        db.klass = Connection

    def give(conn):
        if not isinstance(conn._reader, Reader):
            conn._reader = Reader(conn, conn._cache, conn._reader._factory)

    # The lock of the database covers its pools while the loop runs.
    db._connectionMap(give)
