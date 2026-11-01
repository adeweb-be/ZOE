"""A reader of ZODB records in Rust, in place of `Connection._reader`.

`Reader` is ZODB's `ObjectReader` with `setGhostState` and `getState` in
Rust (`zoe._zoe.Reader`). `Connection.setstate` keeps its lines: the load
through the storage, the bookkeeping and the blob. The reader decodes the
record, resolves its persistent references in the pickle cache and calls
`__setstate__`. A record that the Rust path does not decode takes the
Python path of `ObjectReader` whole.

`Connection` is ZODB's `Connection` with a `Reader`. `DB.klass` is the
class that `DB.open` makes, so `install(db)` sets it. Zope reaches the
same attribute with `connection-class zoe.reader.Connection` in a
`<zodb_db>` section.
"""

from ZODB.Connection import Connection as _Connection
from ZODB.serialize import ObjectReader

from zoe._zoe import Reader as _Reader
from zoe._zoe import _reader_counts as counts
from zoe._zoe import _reader_reset as reset_counts

__all__ = ["Connection", "Reader", "counts", "install", "reset_counts"]


class Reader(_Reader, ObjectReader):
    """`ObjectReader` with `setGhostState` and `getState` in Rust."""


class Connection(_Connection):
    """ZODB's `Connection` with a `Reader`."""

    def __init__(self, db, *args, **kwargs):
        super().__init__(db, *args, **kwargs)
        self._reader = Reader(self, self._cache, self._db.classFactory)


def install(db):
    """Make each new connection of `db` a `Connection`, and give a `Reader`
    to the connections that `db` already has."""
    if db.klass is _Connection:
        db.klass = Connection
    for conn in list(db.pool.all):
        if not isinstance(conn._reader, Reader):
            conn._reader = Reader(conn, conn._cache, conn._reader._factory)
