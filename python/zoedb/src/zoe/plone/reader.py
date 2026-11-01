"""The reader of ZOE in a Zope instance (ADR 0057).

`configure.zcml` subscribes `database_opened` to `IDatabaseOpenedWithRoot`,
which Zope sends once at its start, with the main database. When the
storage of that database is a ZOE storage, `zoe.Storage` or
`zoe.zeo.ClientStorage`, the handler calls `zoe.reader.install`, which
gives the reader to every connection of the database, the connection that
the pool holds from the start included. `native-reader off` in `<zoe>` or
`<zoeclient>` turns it off: `install` then does nothing. A database on
another storage keeps ZODB's `ObjectReader`.
"""

import sys

__all__ = ["database_opened", "is_zoe_storage"]


def is_zoe_storage(storage):
    """Tell if `storage` is a `zoe.Storage` or a `zoe.zeo.ClientStorage`."""
    import zoe

    if isinstance(storage, zoe.Storage):
        return True
    # A `zoe.zeo.ClientStorage` exists only once its module is imported.
    zeo = sys.modules.get("zoe.zeo")
    return zeo is not None and isinstance(storage, zeo.ClientStorage)


def database_opened(event):
    """Give the reader of ZOE to the connections of the main database, when
    its storage is a ZOE storage."""
    db = event.database
    if is_zoe_storage(db.storage):
        import zoe.reader

        zoe.reader.install(db)
