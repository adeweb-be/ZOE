"""Print the indexes and the metadata columns of the catalog of the site
`Plone` of a FileStorage, as one JSON document.

    <scenario>/parts/instance/bin/interpreter catalog_counts.py DATA_FS

The script opens the FileStorage read-only, with no configuration of Zope.
It changes no byte of the file.
"""

import json
import sys

from ZODB import DB
from ZODB.FileStorage import FileStorage

(data_fs,) = sys.argv[1:]
db = DB(FileStorage(data_fs, read_only=True))
connection = db.open()
try:
    site = connection.root()["Application"]["Plone"]
    catalog = site["portal_catalog"]._catalog
    indexes = {
        name: {
            "kind": type(index).__name__,
            "entries": len(index._unindex) if hasattr(index, "_unindex") else None,
        }
        for name, index in catalog.indexes.items()
    }
    print(
        json.dumps(
            {
                "indexes": indexes,
                "metadata": sorted(catalog.schema),
                "size": len(catalog),
            }
        )
    )
finally:
    connection.close()
    db.close()
