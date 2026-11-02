"""Measure what a change of security on a folder writes.

    run.py ZOPE_CONF security_change.py OUT_JSON FOLDER PRINCIPAL INDEX QUEUE

Run it on a copy of a fixture: it commits. FOLDER is the path of a folder
below the site. INDEX is `stock` (the catalog as it is) or `zoe` (the
security index of zoe.plone, which the script puts in place first, in a
transaction of its own). QUEUE is `on` (the queued indexing of zoe.plone)
or `off`.

The script makes two changes on FOLDER, each in its own transaction, as
the forms of Plone do:

- sharing: `manage_setLocalRoles(PRINCIPAL, ["Reader"])`, then
  `reindexObjectSecurity()`;
- transition: the workflow transition `retract`, which calls
  `reindexObjectSecurity()`.

For each change it gives the time from the start of the change to the end
of its commit, the records and the bytes of the commit, and the keys that
the commit writes to the queue. With the queue on, it then runs the
indexer until the queue is empty, and gives its time, its commits, its
records and its bytes. The object cache starts cold.
"""

import json
import sys
import time

import transaction
import zoe.kv
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from zoe.plone import indexing, security
from zope.component.hooks import setSite

out_path, folder_path, principal, index_mode, queue_mode = sys.argv[1:6]
newSecurityManager(None, system)
site = app.Plone
setSite(site)
catalog = site.portal_catalog
storage = app._p_jar.db().storage
result = {"index": index_mode, "queue": queue_mode, "folder": folder_path}


def log(*args):
    print("security_change:", *args, flush=True)


def commits_after(tid):
    """The commits after `tid`, their records and their bytes."""
    commits = 0
    records = 0
    total = 0
    for txn in storage.iterator(tid, None):
        if txn.tid == tid:
            continue
        commits += 1
        for record in txn:
            records += 1
            total += len(record.data or b"")
    return commits, records, total


def queue_keys():
    oid = catalog._p_oid
    queue = zoe.kv.Namespace(catalog._p_jar, "durable", indexing.queue_name(oid))
    keys = 0
    total = 0
    start = None
    while True:
        page = queue.scan(start=start, limit=1000)
        if not page:
            return keys, total
        for key, value in page:
            keys += 1
            total += len(key) + len(value)
        start = page[-1][0] + b"\x00"


if index_mode == "zoe":
    started = time.monotonic()
    security.replace_index(catalog)
    transaction.commit()
    result["replace_seconds"] = round(time.monotonic() - started, 1)
    security.install()
if queue_mode == "on":
    indexing.install()
folder = site.unrestrictedTraverse(folder_path)
result["descendants"] = (
    len(catalog.unrestrictedSearchResults(path="/".join(folder.getPhysicalPath()))) - 1
)
# A cold object cache for each change.
app._p_jar.cacheMinimize()


def measure(label, action):
    before = storage.lastTransaction()
    started = time.monotonic()
    action()
    transaction.commit()
    seconds = time.monotonic() - started
    commits, records, total = commits_after(before)
    found = {
        "seconds": round(seconds, 2),
        "commits": commits,
        "records": records,
        "bytes": total,
    }
    if queue_mode == "on":
        found["keys"], found["key_bytes"] = queue_keys()
        saved = storage.lastTransaction()
        started = time.monotonic()
        indexing.Indexer(site).run()
        seconds = time.monotonic() - started
        commits, records, total = commits_after(saved)
        found["indexer"] = {
            "seconds": round(seconds, 2),
            "commits": commits,
            "records": records,
            "bytes": total,
        }
    result[label] = found
    log(label, found)
    app._p_jar.cacheMinimize()


def share():
    folder = site.unrestrictedTraverse(folder_path)
    folder.manage_setLocalRoles(principal, ["Reader"])
    folder.reindexObjectSecurity()


def retract():
    folder = site.unrestrictedTraverse(folder_path)
    site.portal_workflow.doActionFor(folder, "retract")


measure("sharing", share)
measure("transition", retract)
with open(out_path, "w") as file:
    json.dump(result, file, indent=1, sort_keys=True)
log("done")
app._p_jar.db().close()
