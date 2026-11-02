"""The reader of ZOE: a ZODB record decodes in Rust, in place of ZODB's
`ObjectReader` (ADR 0057).

`Reader` is ZODB's `ObjectReader` with `setGhostState` and `getState` in
Rust (`zoe._zoe.Reader`). The reader decodes the record, resolves its
persistent references in the pickle cache and calls `__setstate__`. The
state equals the state of `ObjectReader.getState`. A record that the Rust
path does not decode goes to `ObjectReader` whole.

`Connection` is ZODB's `Connection` with a `Reader`, and with `setstate`
in Rust (`zoe._zoe._setstate`): for an ordinary record, one call does the
lines of ZODB's `Connection.setstate`, the load through the storage, the
bookkeeping and the decode of the reader. A closed connection and a blob
go to ZODB's method. `DB.klass` is the class that `DB.open` makes, so
`install(db)` sets it, and makes the connections that `db` already has
connections of the class. Zope reaches the same attribute with
`connection-class zoe.reader.Connection` in a `<zodb_db>` section, but
only for the connections that the database opens after the first one.

`counts()` gives the counts of the readers of the process: the records
that the Rust path decoded (`decoded`), the records that took the Python
path by reason (`fallback`), and the opcodes that sent a record there
(`opcodes`).
"""

from ZODB.Connection import Connection as _Connection
from ZODB.serialize import ObjectReader

from zoe._zoe import Reader as _Reader
from zoe._zoe import _reader_counts as counts
from zoe._zoe import _setstate

__all__ = ["Connection", "Reader", "counts", "install"]


class Reader(_Reader, ObjectReader):
    """`ObjectReader` with `setGhostState` and `getState` in Rust."""


class Connection(_Connection):
    """ZODB's `Connection` with a `Reader` and `setstate` in Rust.

    The class adds no slot and no attribute to the instances of ZODB's
    `Connection`, so `install` can set the class of a connection.
    """

    # An `instancemethod` of CPython: it binds as a function does.
    setstate = _setstate

    def __init__(self, db, *args, **kwargs):
        super().__init__(db, *args, **kwargs)
        self._reader = Reader(self, self._cache, self._db.classFactory)


def install(db):
    """Give the reader and `setstate` in Rust to the connections of `db`.

    Each new connection of `db` is a `Connection`, when `db.klass` is
    ZODB's `Connection`. Each connection that `db` already has gets a
    `Reader` with the class factory of its reader, and becomes a
    `Connection` when its class is ZODB's `Connection`. A storage whose
    `native_reader` is false, the key `native-reader off` of `<zoe>` and
    of `<zoeclient>`, gets nothing.
    """
    if not getattr(db.storage, "native_reader", True):
        return
    if db.klass is _Connection:
        db.klass = Connection

    def give(conn):
        if type(conn) is _Connection:
            conn.__class__ = Connection
        if not isinstance(conn._reader, Reader):
            conn._reader = Reader(conn, conn._cache, conn._reader._factory)

    # The lock of the database covers its pools while the loop runs.
    db._connectionMap(give)
