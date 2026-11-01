"""The helpers that the acceptance tests share, beside those of `support`.

pytest puts this directory on `sys.path` when it imports a test module of
the directory, so a test module imports this module by its name.
"""

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")


def current_record(db, oid):
    """Return the pickle of the current record of `oid` in the storage of
    `db`."""
    return db.storage.loadBefore(oid, MAX_TID)[0]
