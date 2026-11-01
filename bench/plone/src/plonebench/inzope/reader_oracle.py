"""The oracle of the reader spike ZOE-274 on a FileStorage. run.py runs it:

    interpreter run.py ZOPE_CONF reader_oracle.py DATA_FS OUT

It opens DATA_FS read-only in a database of its own, with the class
factory of the database of Zope, and two connections: one with
`zoe.reader.Reader`, one with ZODB's `ObjectReader`. For each current
record, the Rust path of the reader decodes the state, or says why the
record takes the Python path. `ObjectReader.getState` decodes it in the
other connection. The two states must pickle to the same bytes, with each
persistent object as its oid and its class. Each persistent object of the
state of the reader must be the object of the pickle cache of its
connection for its oid. It writes the counts as JSON to OUT.
"""

import io
import json
import sys
import time
from collections import Counter, defaultdict

import transaction
import ZODB
import zoe.reader
from ZODB._compat import PersistentPickler
from ZODB.FileStorage import FileStorage
from ZODB.serialize import ObjectReader
from ZODB.utils import get_pickle_metadata, oid_repr

data_fs, out = sys.argv[1:3]
factory = app._p_jar.db().classFactory  # noqa: F821 - run.py gives app

storage = FileStorage(data_fs, read_only=True)
db = ZODB.DB(storage, cache_size=10_000_000)
db.classFactory = factory
rust = db.open(transaction.TransactionManager())
python = db.open(transaction.TransactionManager())
rust._reader = zoe.reader.Reader(rust, rust._cache, factory)
python._reader = ObjectReader(python, python._cache, factory)


def dump(state):
    """The bytes of `state`, with each persistent object as its oid and
    its class, and the persistent objects in it."""
    seen = []

    def persistent_id(obj):
        if isinstance(obj, type):
            return None
        if getattr(obj, "_p_jar", None) is None:
            return None
        seen.append(obj)
        return (obj._p_oid, type(obj))

    file = io.BytesIO()
    pickler = PersistentPickler(persistent_id, file, 3)
    pickler.dump(state)
    return file.getvalue(), seen


records = 0
decoded = 0
equal = 0
fallbacks = Counter()
fallback_classes = defaultdict(Counter)
mismatches = []
not_cached = []
uncomparable = []
start = time.monotonic()
zoe.reader.reset_counts()
for oid in storage._index.keys():
    pickle, _ = storage.load(oid)
    records += 1
    module, name = get_pickle_metadata(pickle)
    klass = f"{module}.{name}"
    state, reason = rust._reader._decode(pickle)
    if reason is not None:
        fallbacks[reason] += 1
        fallback_classes[reason][klass] += 1
        continue
    decoded += 1
    expected = python._reader.getState(pickle)
    try:
        got, refs = dump(state)
        want, _ = dump(expected)
    except Exception as error:  # noqa: BLE001 - the oracle reports it
        uncomparable.append([oid_repr(oid), klass, repr(error)])
        continue
    if got == want:
        equal += 1
    else:
        mismatches.append([oid_repr(oid), klass])
    for obj in refs:
        if rust._cache.get(obj._p_oid) is not obj:
            not_cached.append([oid_repr(oid), klass, oid_repr(obj._p_oid)])
    del state, expected, refs
    if records % 5000 == 0:
        rust.cacheMinimize()
        python.cacheMinimize()

result = {
    "records": records,
    "decoded": decoded,
    "equal": equal,
    "fallbacks": dict(fallbacks),
    "fallback_classes": {k: dict(v.most_common()) for k, v in fallback_classes.items()},
    "mismatches": mismatches[:50],
    "mismatch_count": len(mismatches),
    "not_cached": not_cached[:50],
    "not_cached_count": len(not_cached),
    "uncomparable": uncomparable[:50],
    "uncomparable_count": len(uncomparable),
    "seconds": time.monotonic() - start,
    "reader_counts": zoe.reader.counts(),
}
with open(out, "w") as file:
    json.dump(result, file, indent=1)
print(
    json.dumps({k: v for k, v in result.items() if not isinstance(v, list)}, indent=1)
)
