"""Acceptance test of the resident memory of a catalog mirror build.

A store holds a registered synthetic catalog of 200,000 entries: 37
columns and 31 indexes, as a Plone 6 site has them. A child process opens
the store with the page cache at 16 MiB and waits until the mirror is
ready. Its peak resident memory (`VmHWM`) grows over that of a child that
opens the same store with `catalog_memory=0` by at most 2 times the bytes
that the mirror counts (`zoe_catalog_mirror_bytes`). The test prints the
growth of `RssAnon` too, and does not judge it.

The build runs one thread for each CPU that the process can use, so each
child takes at most 16 CPUs. Only Linux has `/proc/self/status`.
"""

import json
import subprocess
import sys
import textwrap

import pytest
import transaction
import ZODB
from BTrees.IOBTree import IOBTree
from BTrees.OIBTree import OIBTree
from DateTime import DateTime
from Products.PluginIndexes.BooleanIndex.BooleanIndex import BooleanIndex
from Products.PluginIndexes.DateIndex.DateIndex import DateIndex
from Products.PluginIndexes.DateRangeIndex.DateRangeIndex import DateRangeIndex
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex
from Products.PluginIndexes.UUIDIndex.UUIDIndex import UUIDIndex
from Products.ZCatalog.Catalog import Catalog
from zope.i18nmessageid import Message

import zoe
import zoe.kv

pytestmark = [
    pytest.mark.usefixtures("no_zoe_environment"),
    pytest.mark.skipif(sys.platform != "linux", reason="Linux only: /proc"),
]

ENTRIES = 200_000
CACHE_SIZE = 16 * 2**20
# The most build threads: the most CPUs of each child.
THREADS = 16
# The longest wait for the mirror, in seconds.
TIMEOUT = 300

TYPES = ("Document", "Folder", "News Item", "File", "Event", "Image", "Link")
STATES = ("published", "private", "pending")
FIELDS = (
    "Creator",
    "getId",
    "id",
    "meta_type",
    "portal_type",
    "review_state",
    "sortable_title",
    "Type",
    "location",
    "mime_type",
    "in_reply_to",
    "sync_uid",
    "total_comments",
    "exclude_from_nav",
    "getEventType",
)
KEYWORDS = (
    "Subject",
    "object_provides",
    "allowedRolesAndUsers",
    "getRawRelatedItems",
    "commentators",
)
DATES = ("Date", "created", "modified", "effective", "expires", "start", "end")
BOOLEANS = ("is_folderish", "is_default_page")
COLUMNS = (
    "CreationDate",
    "Creator",
    "Date",
    "Description",
    "EffectiveDate",
    "ExpirationDate",
    "ModificationDate",
    "Subject",
    "Title",
    "Type",
    "UID",
    "author_name",
    "cmf_uid",
    "commentators",
    "created",
    "effective",
    "end",
    "exclude_from_nav",
    "expires",
    "getIcon",
    "getId",
    "getObjSize",
    "getRemoteUrl",
    "id",
    "in_response_to",
    "is_folderish",
    "last_comment_date",
    "listCreators",
    "location",
    "meta_type",
    "mime_type",
    "modified",
    "portal_type",
    "review_state",
    "start",
    "sync_uid",
    "total_comments",
)
INTERFACES = tuple(f"package.content.interfaces.I{kind}" for kind in TYPES)
COMMON = tuple(
    f"package.behaviors.interfaces.I{name}"
    for name in ("Content", "Item", "Container", "Relations", "Versioning")
) + tuple(
    f"package.dexterity.interfaces.I{name}"
    for name in ("Ownership", "Publication", "Locking")
)
# 2026-01-01 00:00 UTC, in seconds.
START = 1767225600
EXPIRES = DateTime(4102444800, "GMT+2")


def minutes(seconds):
    """The value of a DateIndex for a time in seconds: its minutes, as
    `DateIndex._convert` gives them."""
    return seconds // 60


def entry(i):
    """The values of the indexes and the columns of the entry `i`."""
    kind = TYPES[i % len(TYPES)]
    seconds = START + i * 60
    modified = DateTime(seconds, "GMT+2")
    created = DateTime(seconds - 30 * 86400, "GMT+2")
    creator = f"editor-{i % 53}"
    roles = (
        ("Anonymous",)
        if i % 3 == 0
        else ("Manager", "Site Administrator", f"user:{creator}", f"group:{i % 97}")
    )
    fields = {
        "Creator": creator,
        "getId": f"item-{i}",
        "id": f"item-{i}",
        "meta_type": "Dexterity Item",
        "portal_type": kind,
        "review_state": STATES[i % len(STATES)],
        "sortable_title": f"the title of item {i:010d}",
        "Type": kind,
        "location": "",
        "mime_type": "text/plain" if i % 7 else "application/pdf",
        "total_comments": 0,
        "exclude_from_nav": i % 11 == 0,
    }
    keywords = {
        "Subject": [f"tag-{(i * k) % 211}" for k in range(1, 1 + i % 4)],
        "object_provides": [INTERFACES[i % len(TYPES)], *COMMON],
        "allowedRolesAndUsers": list(roles),
    }
    dates = {
        "Date": minutes(seconds),
        "created": minutes(seconds - 30 * 86400),
        "modified": minutes(seconds),
        "effective": minutes(seconds),
        "expires": minutes(4102444800),
    }
    booleans = {"is_folderish": kind == "Folder", "is_default_page": False}
    columns = {
        "CreationDate": created.ISO(),
        "Creator": creator,
        "Date": modified,
        "Description": (
            f"A description of the item {i}, of the kind {kind}, in the unit {i % 97}."
            if i % 3
            else ""
        ),
        "EffectiveDate": modified.ISO(),
        "ExpirationDate": "None",
        "ModificationDate": modified.ISO(),
        "Subject": tuple(keywords["Subject"]),
        "Title": f"The title of item {i} in folder {i % 977}",
        "Type": Message(kind, domain="plone"),
        "UID": f"{i:032x}",
        "created": created,
        "effective": modified,
        "exclude_from_nav": fields["exclude_from_nav"],
        "expires": EXPIRES,
        "getIcon": False,
        "getId": fields["getId"],
        "getObjSize": f"{i % 900} KB",
        "id": fields["id"],
        "is_folderish": booleans["is_folderish"],
        "listCreators": (creator,),
        "location": "",
        "meta_type": "Dexterity Item",
        "mime_type": fields["mime_type"],
        "modified": modified,
        "portal_type": kind,
        "review_state": fields["review_state"],
        "total_comments": 0,
    }
    return fields, keywords, dates, booleans, columns


def write_catalog(path):
    """Write a store at `path` with a registered catalog of `ENTRIES`
    entries. The trees get the values that the indexes of Products.ZCatalog
    keep, without the reverse trees `_index`, which the mirror does not
    read. Return the oid of the catalog."""
    db = ZODB.DB(zoe.Storage(path, catalog_memory=0))
    try:
        conn = db.open()
        catalog = Catalog()
        for name in FIELDS:
            catalog.addIndex(name, FieldIndex(name))
        for name in KEYWORDS:
            catalog.addIndex(name, KeywordIndex(name))
        for name in DATES:
            catalog.addIndex(name, DateIndex(name))
        for name in BOOLEANS:
            catalog.addIndex(name, BooleanIndex(name))
        catalog.addIndex("UID", UUIDIndex("UID"))
        catalog.addIndex(
            "effectiveRange",
            DateRangeIndex(
                "effectiveRange", since_field="effective", until_field="expires"
            ),
        )
        for name in COLUMNS:
            catalog.addColumn(name)
        unindex = {name: {} for name in catalog.indexes}
        paths, uids, data = {}, {}, {}
        for i in range(ENTRIES):
            rid = i + 1
            fields, keywords, dates, booleans, columns = entry(i)
            for values in (fields, keywords, dates, booleans):
                for name, value in values.items():
                    unindex[name][rid] = value
            unindex["UID"][rid] = columns["UID"]
            unindex["effectiveRange"][rid] = (dates["effective"], dates["expires"])
            path = f"/site/folder-{i % 977}/item-{i}"
            paths[rid] = path
            uids[path] = rid
            data[rid] = tuple(columns.get(name) for name in catalog.names)
        for name, values in unindex.items():
            index = catalog.indexes[name]
            index._unindex = IOBTree(values)
            index._length.set(len(values))
        catalog.paths = IOBTree(paths)
        catalog.uids = OIBTree(uids)
        catalog.data = IOBTree(data)
        catalog._length.set(ENTRIES)
        conn.root()["catalog"] = catalog
        transaction.commit()
        zoe.kv.Namespace(conn, "durable", "zoe.catalog").set(catalog._p_oid, b"")
        transaction.commit()
        oid = catalog._p_oid
        conn.close()
    finally:
        db.close()
    return oid


# The child: it opens the store, waits for the mirror when it has one, and
# prints its memory and the counts of the mirror.
CHILD = textwrap.dedent(
    """\
    import json
    import os
    import sys
    import time

    import zoe

    path, oid, memory, cache_size, threads, timeout = sys.argv[1:]
    cpus = sorted(os.sched_getaffinity(0))[: int(threads)]
    os.sched_setaffinity(0, cpus)
    options = {"cache_size": int(cache_size)}
    if memory != "default":
        options["catalog_memory"] = int(memory)
    storage = zoe.Storage(path, **options)
    oid = bytes.fromhex(oid)
    out = {"cpus": len(cpus)}
    if memory != "0":
        end = time.monotonic() + float(timeout)
        while True:
            status = storage._catalog_status(oid)
            if status and status["state"] not in ("pending", "building"):
                break
            if time.monotonic() > end:
                break
            time.sleep(0.05)
        out["state"] = status and status["state"]
        mirror = storage._catalog_counts()["mirrors"].get(oid, {})
        out["bytes"] = mirror.get("zoe_catalog_mirror_bytes")
        out["seconds"] = mirror.get("zoe_catalog_mirror_build_seconds")
    else:
        time.sleep(2)
    with open("/proc/self/status") as status:
        for line in status:
            name, _, value = line.partition(":")
            if name in ("VmHWM", "RssAnon"):
                out[name] = int(value.split()[0]) * 1024
    storage.close()
    print(json.dumps(out))
    """
)


def measure(path, oid, memory):
    """Run the child on the store at `path` and return what it prints."""
    done = subprocess.run(
        [
            sys.executable,
            "-c",
            CHILD,
            path,
            oid.hex(),
            str(memory),
            str(CACHE_SIZE),
            str(THREADS),
            str(TIMEOUT),
        ],
        capture_output=True,
        text=True,
        timeout=TIMEOUT + 60,
    )
    assert done.returncode == 0, done.stderr
    return json.loads(done.stdout)


def test_the_peak_of_a_build_stays_within_two_times_the_mirror(tmp_path):
    path = str(tmp_path / "store")
    oid = write_catalog(path)
    base = measure(path, oid, 0)
    mirror = measure(path, oid, "default")
    assert mirror["state"] == "ready", mirror
    mib = 2**20
    growth = mirror["VmHWM"] - base["VmHWM"]
    anon = mirror["RssAnon"] - base["RssAnon"]
    print(
        f"cpus {mirror['cpus']}, build {mirror['seconds']:.2f} s, "
        f"mirror {mirror['bytes'] / mib:.0f} MiB, "
        f"VmHWM growth {growth / mib:.0f} MiB ({growth / mirror['bytes']:.2f} x), "
        f"RssAnon growth {anon / mib:.0f} MiB ({anon / mirror['bytes']:.2f} x)"
    )
    assert growth <= 2 * mirror["bytes"], (base, mirror)
