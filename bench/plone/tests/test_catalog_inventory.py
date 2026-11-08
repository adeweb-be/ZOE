"""Tests of the read of a query log of the catalog and of its inventory,
with no Zope: the lines that the probe could not record, and an inventory
that holds no value of a query.
"""

import json

from plonebench.catalog import log

MARKER = "marker-9a51c0"


def search(query, **extra):
    line = {
        "v": log.VERSION,
        "t": 0.0,
        "pid": 1,
        "catalog": ["", "Plone", "portal_catalog"],
        "query": {"$": "dict", "v": query},
        "sort": None,
        "reverse": False,
        "limit": None,
        "merge": True,
        "indexes": {"Subject": "KeywordIndex", "path": "ExtendedPathIndex"},
        "changed": False,
        "n": 0,
        "a": 0,
        "replayable": True,
    }
    line.update(extra)
    return line


def test_the_lines_that_the_probe_could_not_record_are_counted(tmp_path):
    path = tmp_path / "catalog-instance0.jsonl"
    lines = [
        search([["Subject", "alpha"]]),
        {"v": log.VERSION, "kind": "unrecorded", "n": 3, "pid": 1},
    ]
    path.write_text("".join(json.dumps(line) + "\n" for line in lines))
    totals = {}
    found = log.read(path, totals)
    assert len(found) == 1
    assert totals == {"unrecorded": 3}
    text = log.format_inventory(log.inventory(found, totals["unrecorded"]))
    assert "Not recorded (the probe could not encode the call): 3 calls." in text


def test_the_inventory_holds_no_value_of_a_query():
    lines = [
        search(
            [
                ["Subject", {"$": "dict", "v": [["query", MARKER], ["range", MARKER]]}],
                ["path", {"$": "dict", "v": [["query", "/a"], ["depth", 7]]}],
                [MARKER, "x"],
                ["sort_on", "getId"],
            ]
        )
    ]
    found = log.inventory(lines)
    text = json.dumps(found) + log.format_inventory(found)
    assert MARKER not in text
    assert "7" not in json.dumps(found["options"])
    options = {(row["type"], row["options"]) for row in found["options"]}
    assert ("KeywordIndex", "query, range") in options
    assert ("ExtendedPathIndex", "depth, query") in options
    assert {row["key"] for row in found["keys"]} == {"other", "sort_on"}
