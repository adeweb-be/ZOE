"""The ids of `portal_uidgenerator` from a sequence of the store.

`portal_uidgenerator` is the `UniqueIdGeneratorTool` of Products.CMFUid.
Its `__call__` adds 1 to the int `_uid_counter` of the tool. CMFEditions
calls it for the history id of each object that it versions. So two
transactions that create content at the same time write the tool, and one
of them gets a `ConflictError`.

`install()` replaces `__call__`. When the storage of the tool is a
`zoe.Storage` or a `zoe.zeo.ClientStorage` that gives sequences, the id is
`base + value`, where `value` comes from `storage.sequence(SEQUENCE)`. The
transaction that asks for the id does not write the tool. Otherwise the
stock `__call__` runs.

The tool keeps the range of the wiring in the attribute `_zoe_ids`, a
tuple `(base, floor, end)`. An id `base + value` is valid when
`floor < base + value <= end` and `_uid_counter == end`. `_uid_counter` is
then the end of the ids that the wiring reserved, so the stock counter
never gives an id that the wiring gave, also after the wiring stops.

When an id is not valid, a reservation writes the tool in a transaction of
its own, on a connection of its own, and commits. It never writes the tool
in the transaction of the caller. A reservation in a range raises the end
to `id + BLOCK - 1`, so the tool gets one write for each `BLOCK` ids. A
reservation that starts a new range (the first id, or `_uid_counter` is not
the end of the range) sets the id to `max(_uid_counter, end) + 1`. That
happens when the stock counter gave ids since the last reservation, and
after a copy of the site into a new store, whose sequence starts again at
1. A value that is below the start of the range also starts a new range.
The reservations of one process wait for each other, so they do not
conflict. Reservations of two processes can conflict; the reservation then
reads the tool again and retries.

Only `install()` imports Plone.
"""

import threading

import transaction
from BTrees.Length import Length
from ZODB.POSException import ConflictError, Unsupported
from ZODB.utils import z64

__all__ = ["BLOCK", "SEQUENCE", "database_opened", "install", "next_id"]

# The name of the sequence of the store.
SEQUENCE = "zoe.plone.portal_uidgenerator"

# The number of ids that one reservation adds to the range.
BLOCK = 100

# The attribute of the tool that holds the range: (base, floor, end).
STATE = "_zoe_ids"

# The largest number of times that a reservation retries after a conflict.
RETRIES = 100

# One reservation at a time in this process.
_reservation = threading.Lock()


def _zoe_storage(storage):
    """Return True when `storage` is a ZOE storage."""
    from zoe.storage import Storage

    if isinstance(storage, Storage):
        return True
    try:
        from zoe.zeo import ClientStorage
    except ImportError:
        # zoe.zeo needs ZEO.
        return False
    return isinstance(storage, ClientStorage)


def _counter(tool):
    """Return the `_uid_counter` of `tool` as an int."""
    counter = tool._uid_counter
    # Old sites keep a `Length`. The stock `__call__` reads it the same way.
    if isinstance(counter, Length):
        return counter()
    return counter


def _check(tool, value):
    """Return the id of `value` when the range of `tool` makes it valid.

    Return None when the caller must reserve.
    """
    state = getattr(tool, STATE, None)
    if state is None:
        return None
    base, floor, end = state
    uid = base + value
    if floor < uid <= end and _counter(tool) == end:
        return uid
    return None


def _reserve(tool, value):
    """Reserve the id of `value` in the tool, and return it.

    The reservation reads the tool on a new connection, in a transaction of
    its own, and commits there.
    """
    with _reservation:
        db = tool._p_jar.db()
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        try:
            for _ in range(RETRIES):
                manager.begin()
                fresh = connection.get(tool._p_oid)
                uid = _check(fresh, value)
                if uid is not None:
                    # Another reservation made the id valid.
                    manager.abort()
                    return uid
                counter = _counter(fresh)
                state = getattr(fresh, STATE, None)
                base, floor, end = state or (0, 0, 0)
                uid = base + value
                if state is not None and counter == end and uid > floor:
                    # The id is above the range: raise the end.
                    end = uid + BLOCK - 1
                else:
                    # Start a new range above every id of the site: above
                    # the stock counter and above the end of the old range.
                    uid = max(counter, end) + 1
                    base, floor, end = uid - value, uid, uid + BLOCK - 1
                fresh._uid_counter = end
                setattr(fresh, STATE, (base, floor, end))
                try:
                    manager.commit()
                except ConflictError:
                    manager.abort()
                    continue
                return uid
            raise ConflictError(
                f"no reservation of portal_uidgenerator after {RETRIES} tries"
            )
        finally:
            manager.abort()
            connection.close()


def next_id(tool):
    """Return the next id of `tool` from a sequence of its store.

    Return None when the stock counter must give the id: the tool has no
    database yet, the tool is new or changed in this transaction, its
    storage is not a ZOE storage, or the storage does not give sequences.
    """
    # The tool comes with an acquisition wrapper. Read its own attributes.
    tool = getattr(tool, "aq_base", tool)
    jar = tool._p_jar
    if jar is None:
        return None
    if tool._p_changed or tool._p_serial == z64:
        # The transaction writes the tool anyway, or the tool is new and a
        # reservation cannot read it.
        return None
    storage = jar.db().storage
    if not _zoe_storage(storage):
        return None
    sequence = storage.sequence(SEQUENCE)
    try:
        value = sequence.next()
    except Unsupported:
        # zoe.zeo.ClientStorage against a stock ZEO server.
        return None
    uid = _check(tool, value)
    if uid is None:
        uid = _reserve(tool, value)
    return uid


def install():
    """Replace `UniqueIdGeneratorTool.__call__` with the wired one.

    A second call does nothing.
    """
    from Products.CMFUid.UniqueIdGeneratorTool import UniqueIdGeneratorTool

    stock = UniqueIdGeneratorTool.__call__
    if getattr(stock, "zoe_stock", None) is not None:
        return

    def __call__(self):
        uid = next_id(self)
        if uid is None:
            return stock(self)
        return uid

    __call__.__doc__ = stock.__doc__
    __call__.zoe_stock = stock
    UniqueIdGeneratorTool.__call__ = __call__


def database_opened(event):
    """Install the wiring when Zope opened its database.

    Zope sends `IDatabaseOpenedWithRoot` after it loaded the ZCML.
    """
    install()
