"""The ids of `portal_uidgenerator` from a sequence of the store.

`portal_uidgenerator` is the `UniqueIdGeneratorTool` of Products.CMFUid.
Its `__call__` adds 1 to the int `_uid_counter` of the tool. CMFEditions
calls it for the history id of each object that it versions. So two
transactions that create content at the same time write the tool, and one
of them gets a `ConflictError`.

`install()` replaces `__call__`. When the storage of the tool is a
`zoe.Storage` or a `zoe.zeo.ClientStorage` that gives sequences, the id is
`base + value`. `value` comes from the sequence of the tool, whose name is
`SEQUENCE`, a colon and the oid of the tool in hex, so each tool of a store
has its own sequence. The transaction that asks for the id does not write
the tool. Otherwise the stock `__call__` runs.

The tool keeps the range of the wiring in the attribute `_zoe_ids`, a
tuple `(base, floor, mark, end)`. `floor` is the first id of the range,
`mark` the largest value that a reservation of the range took, and `end`
the last id of the range. An id `base + value` is valid when
`value > mark`, `floor < base + value <= end` and `_uid_counter == end`.
`_uid_counter` is then the end of the ids that the wiring reserved, so the
stock counter never gives an id that the wiring gave, also after the
wiring stops.

When an id is not valid, a reservation writes the tool in a transaction of
its own, on a connection of its own, and commits. It never writes the tool
in the transaction of the caller. A reservation in a range raises `end` to
`id + BLOCK - 1` and `mark` to the value, so the tool gets one write for
each `BLOCK` ids.

The values of a store strictly increase, and a caller takes its value
after the start of its snapshot. So in one store, the value of a caller is
always above the `mark` that its snapshot shows. A value at or below that
`mark` comes from another store: a copy of the site through `iterator` and
`restore`, whose sequences start again at 1. Then, and for the first id of
a site, and when `_uid_counter` is not the end of the range (the stock
counter gave ids), the reservation starts a new range. The new range
starts at `max(_uid_counter, end) + 1`, above every id of the site. The
reservation then takes `BLOCK` more values of the sequence and makes the
last one the `mark`. So after a copy, the first `BLOCK` values of the new
store are all at or below the `mark`, and each one starts a new range.

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

__all__ = [
    "BLOCK",
    "SEQUENCE",
    "database_opened",
    "install",
    "next_id",
    "sequence_name",
]

# The start of the name of the sequence of a tool.
SEQUENCE = "zoe.plone.portal_uidgenerator"

# The number of ids that one reservation adds to the range, and the number
# of values that a new range takes before its mark.
BLOCK = 100

# The attribute of the tool that holds the range: (base, floor, mark, end).
STATE = "_zoe_ids"

# The note of a reservation transaction.
NOTE = "zoe.plone: reserve ids of portal_uidgenerator"

# The largest number of times that a reservation retries after a conflict.
RETRIES = 100

# One reservation at a time in this process.
_reservation = threading.Lock()


def sequence_name(tool):
    """Return the name of the sequence of `tool`."""
    return f"{SEQUENCE}:{tool._p_oid.hex()}"


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


def _state(tool):
    """Return the range of `tool`, or None when it has none."""
    state = getattr(tool, STATE, None)
    if isinstance(state, tuple) and len(state) == 4:
        return state
    return None


def _reserve(tool, sequence, value, seen):
    """Reserve an id for `value` in the tool, and return the id.

    `seen` is the mark that the snapshot of the caller shows, 0 when it
    shows no range. The reservation reads the tool on a new connection, in
    a transaction of its own, and commits there.
    """
    with _reservation:
        db = tool._p_jar.db()
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        try:
            for _ in range(RETRIES):
                manager.begin()
                fresh = connection.get(tool._p_oid)
                counter = _counter(fresh)
                state = _state(fresh)
                base, floor, mark, end = state or (0, 0, 0, 0)
                uid = base + value
                if (
                    state is not None
                    and value > seen
                    and counter == end
                    and uid > floor
                ):
                    if uid <= end:
                        # Another reservation made the id valid.
                        manager.abort()
                        return uid
                    # The id is above the range: raise the end.
                    mark = max(mark, value)
                    end = uid + BLOCK - 1
                else:
                    # Start a new range above every id of the site: above
                    # the stock counter and above the end of the old range.
                    uid = max(counter, end) + 1
                    last = value
                    for _ in range(BLOCK):
                        last = sequence.next()
                    base, floor, mark = uid - value, uid, last
                    end = base + last + BLOCK
                fresh._uid_counter = end
                setattr(fresh, STATE, (base, floor, mark, end))
                manager.get().note(NOTE)
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
    """Return the next id of `tool` from the sequence of the tool.

    Return None when the stock counter must give the id: the tool has no
    database yet, the tool is new or changed in this transaction, its
    storage is not a ZOE storage, or the storage does not give sequences.
    """
    # The tool comes with an acquisition wrapper. Read its own attributes.
    tool = getattr(tool, "aq_base", tool)
    jar = tool._p_jar
    if jar is None:
        return None
    # A ghost has the serial z64 until it loads. Load it.
    tool._p_activate()
    if tool._p_changed or tool._p_serial == z64:
        # The transaction writes the tool anyway, or the tool is new and a
        # reservation cannot read it.
        return None
    storage = jar.db().storage
    if not _zoe_storage(storage):
        return None
    sequence = storage.sequence(sequence_name(tool))
    try:
        value = sequence.next()
    except Unsupported:
        # zoe.zeo.ClientStorage against a stock ZEO server.
        return None
    state = _state(tool)
    if state is None:
        return _reserve(tool, sequence, value, 0)
    base, floor, mark, end = state
    uid = base + value
    if value > mark and floor < uid <= end and _counter(tool) == end:
        return uid
    return _reserve(tool, sequence, value, mark)


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
