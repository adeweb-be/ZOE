"""Dump the records of a B5 run for the spike on field-addressable objects.

Run with the interpreter of the zoeclient scenario, which has zoe and ZODB:

    interpreter dump.py RUN_DIR OUT_DIR

It opens the kept store of RUN_DIR read-only and writes into OUT_DIR:

- txns.jsonl: each transaction of the step windows (the trim left out),
  with its step, its kind (a save or a batch of the indexer) and its
  records (oid, class, bytes);
- content.bin: for each record of a content class in the windows, the
  group [previous record, record];
- conflicts.jsonl and conflicts.bin: each conflict of the probe records on
  an object outside BTrees, with the line of the resolution log of the
  same oid, and the group [read record, committed record, record before
  the retry, record of the retry].

A group of a .bin file is a u32 length and a JSON header, a u32 count, then
each record as a u32 length and its bytes, little-endian.
"""

import glob
import json
import os
import struct
import sys

import zoe
from ZODB.POSException import POSKeyError
from ZODB.TimeStamp import TimeStamp
from ZODB.utils import get_pickle_metadata, p64

run_dir, out_dir = sys.argv[1:3]
os.makedirs(out_dir, exist_ok=True)
result = json.load(open(os.path.join(run_dir, "result.json")))
start = result["measured"]["start"]
trim = result.get("trim", 5.0)
steps = result["steps"]
windows = []
t = start
for users, seconds in steps:
    windows.append((users, t + trim, t + seconds))
    t += seconds

store = zoe.Storage(os.path.join(run_dir, "store", "zoe"), read_only=True)
CONTENT = ("plone.app.contenttypes.content.", "Products.CMFPlone.Portal.")
INDEXER = b"zoe.plone: indexing queue"


def tid_time(tid):
    return TimeStamp(tid).timeTime()


def step_of(when):
    for users, lo, hi in windows:
        if lo <= when < hi:
            return users
    return None


def class_of(data):
    if not data:
        return "deleted"
    module, name = get_pickle_metadata(data)
    return f"{module}.{name}"


def group(out, header, records):
    h = json.dumps(header).encode()
    out.write(struct.pack("<I", len(h)) + h + struct.pack("<I", len(records)))
    for r in records:
        r = r or b""
        out.write(struct.pack("<I", len(r)) + r)


def before(oid, tid):
    try:
        found = store.loadBefore(oid, tid)
    except POSKeyError:
        return None
    return found[0] if found else None


first_tid = p64(0)
content = open(os.path.join(out_dir, "content.bin"), "wb")
txns = open(os.path.join(out_dir, "txns.jsonl"), "w")
for txn in store.iterator():
    when = tid_time(txn.tid)
    if when < windows[0][1] - 1:
        continue
    users = step_of(when)
    if users is None:
        continue
    kind = "indexer" if INDEXER in (txn.description or b"") else "save"
    records = []
    for rec in txn:
        cls = class_of(rec.data)
        records.append([rec.oid.hex(), cls, len(rec.data or b"")])
        if cls.startswith(CONTENT):
            prev = before(rec.oid, txn.tid)
            group(
                content,
                {
                    "tid": txn.tid.hex(),
                    "oid": rec.oid.hex(),
                    "class": cls,
                    "step": users,
                    "kind": kind,
                },
                [prev, rec.data],
            )
    txns.write(
        json.dumps(
            {
                "tid": txn.tid.hex(),
                "t": when,
                "step": users,
                "kind": kind,
                "user": (txn.user or b"").decode("utf-8", "replace"),
                "description": (txn.description or b"").decode("utf-8", "replace"),
                "records": records,
            }
        )
        + "\n"
    )
content.close()
txns.close()

# The conflicts of the probe records, outside BTrees.
log = []
log_path = os.path.join(run_dir, "resolution.jsonl")
if os.path.exists(log_path):
    log = [json.loads(line) for line in open(log_path)]
log_by_oid = {}
for line in log:
    log_by_oid.setdefault(line["oid"], []).append(line)

conflicts = []
for path in sorted(glob.glob(os.path.join(run_dir, "probe", "instance*.jsonl"))):
    for line in open(path):
        request = json.loads(line)
        for error in request.get("errors", []):
            if error.get("error") not in ("ConflictError", "ReadConflictError"):
                continue
            conflicts.append(
                {
                    "t": request.get("t"),
                    "op": request.get("op"),
                    "name": request.get("name"),
                    "attempt": error.get("attempt"),
                    "retry": error.get("retry"),
                    "conflict": error.get("conflict"),
                    "class": error.get("class"),
                    "oid": error.get("oid"),
                }
            )


def history(oid):
    """The tids and the records of oid, oldest first."""
    found = []
    tid = None
    while True:
        try:
            data, serial, _ = store.loadBefore(oid, tid or p64(2**64 - 1)) or (
                None,
                None,
                None,
            )
        except POSKeyError:
            break
        if data is None and serial is None:
            break
        found.append((serial, data))
        tid = serial
        if len(found) > 200:
            break
    return list(reversed(found))


out = open(os.path.join(out_dir, "conflicts.bin"), "wb")
summary = open(os.path.join(out_dir, "conflicts.jsonl"), "w")
txn_meta = {}
for txn in store.iterator():
    txn_meta[txn.tid] = (
        (txn.user or b"").decode("utf-8", "replace"),
        (txn.description or b"").decode("utf-8", "replace"),
    )
for c in conflicts:
    c["step"] = step_of(c["t"]) if c["t"] else None
    cls = c["class"] or ""
    if cls.startswith("BTrees.") or not c["oid"]:
        c["group"] = None
        summary.write(json.dumps(c) + "\n")
        continue
    oid = bytes.fromhex(c["oid"])
    # The line of the resolution log of this oid nearest before the end of
    # the request.
    lines = [x for x in log_by_oid.get(c["oid"], []) if x["time"] <= c["t"] + 1]
    line = max(lines, key=lambda x: x["time"]) if lines else None
    c["log"] = line
    hist = history(oid)
    tids = [s for s, _ in hist]
    if line:
        read, committed = bytes.fromhex(line["read"]), bytes.fromhex(line["committed"])
    else:
        # Without a line: the last record before the end of the request,
        # and the record before it.
        earlier = [s for s in tids if tid_time(s) <= c["t"]]
        committed = earlier[-1] if earlier else None
        read = earlier[-2] if len(earlier) > 1 else None
    records = dict(hist)
    group_records = [records.get(read), records.get(committed)]
    # The retry: the first later record of the same user and description
    # as the failed transaction, when the line gives them.
    retry = None
    if line and committed in tids:
        for s in tids[tids.index(committed) + 1 :]:
            if txn_meta.get(s) == (line["user"], line["description"]):
                retry = s
                break
    if retry is not None:
        i = tids.index(retry)
        group_records += [records.get(tids[i - 1]), records.get(retry)]
    c["read"] = read.hex() if read else None
    c["committed"] = committed.hex() if committed else None
    c["retry_tid"] = retry.hex() if retry else None
    c["group"] = len(group_records)
    group(out, {"oid": c["oid"], "t": c["t"]}, group_records)
    summary.write(json.dumps(c) + "\n")
out.close()
summary.close()
store.close()
print("windows", windows, "conflicts", len(conflicts))
