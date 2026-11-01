"""The registry backend of `zoe.plone` (ADR 0050).

`move(registry)` moves the records of a registry of plone.registry, its
`_fields` and `_values`, to a durable namespace of its own in the store:
`zoe.plone.registry:` followed by a random id. The registry then keeps a
`MovedRecords` in `_records`: the id, the write counter, and views of the
rows. Its stock `OOBTree`s stay in the store, out of its reach. The rows
are in `zoe.plone.registry_rows`.

The option is opt-in: `ZOE_PLONE_BACKENDS=registry`, or the ZCML file
`zoe.plone:registry.zcml`, calls `install()`. `install()` lets `move`,
`copy_rows` and `reset` run. A moved registry reads and writes in every
process, with or without the option.

- **A read** checks the rows first, once in each transaction: a registry
  in a storage without namespaces, without the marker of its rows, with
  the marker of another oid, or with a marker of a lower counter is a copy
  without its rows, and raises `Unsupported`. When the store holds a newer
  serial of the registry, a move back committed after the snapshot, and
  the read raises `ReadConflictError`. Then a read takes the writes of the
  current transaction, then the rows of a cache of the process. The cache
  is labelled with the counter of the marker. Each transaction decodes a
  row once.
- **A write** stages the row. The first write of a transaction sets the
  counter of the registry and of the marker to 1 more than the higher of
  the counter of the registry and the counter of the stored marker. So it
  writes the registry object: plone.protect sees the write, and two writes
  of one registry conflict. After an undo, a counter does not repeat.
- **A field changed in place** is staged again before the commit and
  before each savepoint. A rollback to a savepoint drops the decoded rows.
  A rollback can keep the staged rows and give back the registry with its
  old counter. So before the commit, a transaction with staged rows, the
  old counter and a staged marker of another counter raises the counter.
- **The guard** of a stock registry in a store with namespaces calls
  `readCurrent` on the roots of its trees when code reads its records, and
  the move empties those roots. So a write that began before a move
  conflicts, and a write of an attribute of the registry object does not.
  `configure.zcml` installs the guard whenever `zoe.plone` loads.

`move_back(registry)` puts the records back into new `OOBTree`s and deletes
the rows. `copy_rows(source, copy)` gives a copy of a moved registry the
rows of its source, under a new id. `reset(registry)` gives a copy whose
source is gone empty `OOBTree`s: the import of the profiles of the site
then gives back the records with their default values. On a ZEO client
without namespaces, `reset` raises `Unsupported`: the client cannot read
the rows that the server can hold.

`zoe.plone.registry_guard` holds the guard, and `zoe.plone.registry_ops`
the moved records and the operations. This module gives their names, so
the dotted names of the ZCML files and the class of the moved records
stay valid.
"""

# The public names, and the private names that the tests of the backend
# read.
from zoe.plone.registry_guard import _holds_its_bucket as _holds_its_bucket
from zoe.plone.registry_guard import install_guard, uninstall_guard
from zoe.plone.registry_ops import (
    MovedRecords,
    copy_rows,
    install,
    move,
    move_back,
    reset,
    uninstall,
)
from zoe.plone.registry_ops import _cache as _cache
from zoe.plone.registry_ops import _state as _state

__all__ = [
    "MovedRecords",
    "copy_rows",
    "install",
    "install_guard",
    "move",
    "move_back",
    "reset",
    "uninstall",
    "uninstall_guard",
]
