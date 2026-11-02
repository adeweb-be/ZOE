"""Write the records that the batches of the indexer wrote in the step
windows of a kept B5 store, for `fieldspike decode`.

    interpreter indexer_records.py RUN_DIR OUT.bin
"""

import json
import os
import struct
import sys

import zoe
from ZODB.TimeStamp import TimeStamp

run_dir, out_path = sys.argv[1:3]
result = json.load(open(os.path.join(run_dir, "result.json")))
start = result["measured"]["start"]
trim = result.get("trim_seconds", 5.0)
windows = []
t = start
for users, seconds in result["steps"]:
    windows.append((users, t + trim, t + seconds))
    t += seconds
store = zoe.Storage(os.path.join(run_dir, "store", "zoe"), read_only=True)
n = 0
with open(out_path, "wb") as out:
    for txn in store.iterator():
        when = TimeStamp(txn.tid).timeTime()
        if not any(lo <= when < hi for _, lo, hi in windows):
            continue
        if b"zoe.plone: indexing queue" not in (txn.description or b""):
            continue
        for rec in txn:
            if not rec.data:
                continue
            h = json.dumps({"tid": txn.tid.hex()}).encode()
            out.write(struct.pack("<I", len(h)) + h + struct.pack("<I", 1))
            out.write(struct.pack("<I", len(rec.data)) + rec.data)
            n += 1
store.close()
print("records", n)
