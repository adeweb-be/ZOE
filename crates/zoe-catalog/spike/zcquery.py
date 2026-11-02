"""The results of ZCTextIndex on catalog-1m, for the query set of the measures.

Usage: python zcquery.py VENDOR DATA_FS QUERIES OUT PATHS_OUT

Opens the packed Data.fs of the fixture read-only, runs each query of
QUERIES through the `QueryParser` and the `OkapiIndex` of its index in
`portal_catalog`, and writes the count, the sum of the scores and the 200
first (rid, score) in the order of `IIBucket.byValue(0)`. PATHS_OUT gets the
path of each rid, so a comparison maps the rids of the fixture to the
entries of the plan.
"""

import json
import sys
import time

sys.argv, args = sys.argv[:2], sys.argv[2:]
import zcsave  # noqa: E402  (installs the classes of plone_lexicon)

from Products.ZCTextIndex.QueryParser import QueryParser  # noqa: E402
from ZODB import DB  # noqa: E402
from ZODB.FileStorage import FileStorage  # noqa: E402


def main():
    data_fs, queries, out, paths_out = args
    db = DB(FileStorage(data_fs, read_only=True), cache_size=400_000)
    conn = db.open()
    app = conn.root()["Application"]
    tool = zcsave.child(zcsave.child(app, "Plone"), "portal_catalog")
    catalog = zcsave.state(tool)["_catalog"]
    with open(paths_out, "w") as f:
        for rid, path in catalog.paths.items():
            f.write(f"{rid}\t{path}\n")
    with open(out, "w") as f:
        for line in open(queries):
            q = json.loads(line)
            okapi = catalog.indexes[q["index"]].index
            t = time.perf_counter()
            tree = QueryParser(okapi._lexicon).parseQuery(q["query"])
            result = tree.executeQuery(okapi)
            ordered = result.byValue(0) if result is not None else []
            seconds = time.perf_counter() - t
            f.write(
                json.dumps(
                    {
                        "query": q["query"],
                        "index": q["index"],
                        "count": len(ordered),
                        "sum": sum(score for score, _ in ordered),
                        "first": [[rid, score] for score, rid in ordered[:200]],
                        "seconds": seconds,
                    }
                )
                + "\n"
            )
            f.flush()
    conn.close()
    db.close()


if __name__ == "__main__":
    main()
