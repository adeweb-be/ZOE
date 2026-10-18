"""B5 counts the bytes that each server writes to its store files.

The kernel counter `write_bytes` of `/proc/<pid>/io` charges whole
page-cache folios, so it overstates the bytes that reach a store. B5 counts
the store bytes of a step from the samples of the server process:

- `zoe serve`: the growth of `wchar` (the bytes given to write calls), minus
  the growth of `socket_bytes_sent` (the bytes that the TCP sockets of the
  process sent), minus the growth of `other_files_bytes` (the size of the
  files of the process that are not the store: the resolution log,
  `zoe-serve.log`, `server.out`).
- `zeo`: the growth of `store_file_bytes`, the size of `Data.fs`. FileStorage
  only appends to it, and its `wchar` holds the writes to `Data.fs.tmp` too.

`sampler.summarize` gives the result as `store_write_bytes` of the process.
The report gives "store KB written/op" beside the kernel counter, which
keeps its value under a name that says it is the page-cache accounting.
"""

import json
import os
import sys

import pytest

from plonebench import analysis, report, sampler
from plonebench.ledger import OK, Record

KB = 1024


def process(**counters):
    entry = {
        "cpu": 1.0,
        "user": 1.0,
        "system": 0.0,
        "threads": 4,
        "rss": 1000,
        "run_delay": 0.0,
        "read_bytes": 0,
        "voluntary": 0,
        "involuntary": 0,
    }
    entry.update(counters)
    return entry


def samples_of(server_entries):
    return [
        {
            "t": float(t),
            "cpu": {"user": float(t), "idle": float(t)},
            "disk": None,
            "processes": {"server": entry},
        }
        for t, entry in enumerate(server_entries)
    ]


def zoe_serve_samples():
    """Three samples of `zoe serve`, one second apart."""
    return samples_of(
        [
            process(
                wchar=0,
                socket_bytes_sent=0,
                other_files_bytes=10_000,
                write_bytes=0,
            ),
            process(
                wchar=5_000_000,
                socket_bytes_sent=400_000,
                other_files_bytes=10_500,
                write_bytes=40_000_000,
            ),
            process(
                wchar=12_000_000,
                socket_bytes_sent=1_000_000,
                other_files_bytes=11_000,
                write_bytes=90_000_000,
            ),
        ]
    )


def zeo_samples():
    """Three samples of the ZEO server, one second apart. Its `wchar` holds
    the writes to `Data.fs.tmp` and to `Data.fs`."""
    return samples_of(
        [
            process(
                wchar=0,
                socket_bytes_sent=0,
                other_files_bytes=2_000,
                store_file_bytes=100_000_000,
                write_bytes=0,
            ),
            process(
                wchar=700_000,
                socket_bytes_sent=200_000,
                other_files_bytes=2_100,
                store_file_bytes=100_150_000,
                write_bytes=500_000,
            ),
            process(
                wchar=1_400_000,
                socket_bytes_sent=400_000,
                other_files_bytes=2_200,
                store_file_bytes=100_300_000,
                write_bytes=1_000_000,
            ),
        ]
    )


def test_zoe_serve_store_bytes_are_wchar_without_sockets_and_other_files():
    server = sampler.summarize(zoe_serve_samples(), 0.0, 2.0)["processes"]["server"]
    assert server["store_write_bytes"] == 12_000_000 - 1_000_000 - 1_000
    # The kernel counter keeps its value.
    assert server["write_bytes"] == 90_000_000

    window = sampler.summarize(zoe_serve_samples(), 1.0, 2.0)
    server = window["processes"]["server"]
    assert server["store_write_bytes"] == 7_000_000 - 600_000 - 500
    assert server["write_bytes"] == 50_000_000


def test_zeo_store_bytes_are_the_growth_of_data_fs():
    server = sampler.summarize(zeo_samples(), 0.0, 2.0)["processes"]["server"]
    assert server["store_write_bytes"] == 300_000
    assert server["write_bytes"] == 1_000_000

    window = sampler.summarize(zeo_samples(), 1.0, 2.0)
    assert window["processes"]["server"]["store_write_bytes"] == 150_000


def test_no_store_bytes_without_their_counters():
    # macOS has no wchar: the store bytes stay absent, never 0.
    samples = samples_of([process(write_bytes=0), process(write_bytes=4096)])
    server = sampler.summarize(samples, 0.0, 1.0)["processes"]["server"]
    assert "store_write_bytes" not in server
    assert server["write_bytes"] == 4096


@pytest.mark.skipif(sys.platform == "darwin", reason="macOS has no wchar")
def test_the_sample_of_a_process_has_wchar(tmp_path):
    before = sampler.read_process(os.getpid())["wchar"]
    with open(tmp_path / "file", "wb") as file:
        file.write(b"x" * 1_000_000)
    after = sampler.read_process(os.getpid())["wchar"]
    assert after - before >= 1_000_000


def step_result(ok: int, server_resources: dict) -> dict:
    records = [
        Record(
            op=f"op{i}",
            kind="edit",
            user=0,
            role="edit",
            account="a",
            instance=0,
            step=0,
            start=float(i),
            end=float(i) + 0.5,
            outcome=OK,
            attempts=1,
            requests=2,
        )
        for i in range(ok)
    ]
    summary = analysis.summarize_steps(records, {}, [(4, 60.0)], [(0.0, 60.0)])[0]
    summary["resources"] = {"processes": {"server": server_resources}}
    return summary


def write_result(directory, stack: str, ok: int, server_resources: dict):
    directory.mkdir()
    result = {
        "run_id": f"run-edit-{stack}",
        "stack": {"stack": {"name": stack}, "settings": {}},
        "scenario": {
            "name": "edit",
            "description": "Editors save documents.",
            "mix": {"edit": 1.0},
            "background": [],
            "interface": ["classic"],
        },
        "steps": [[4, 60.0]],
        "trim_seconds": 0.0,
        "think": "0,0",
        "client_retries": 0,
        "workload_model": "closed",
        "measured": {"start": 0.0, "end": 60.0},
        "fixture": {
            "name": "small",
            "equivalence": {"objects": 10, "equivalent": True},
        },
        "environment": {
            "commit": "0" * 40,
            "dirty": False,
            "machine": {
                "cpu": "cpu",
                "logical_cpus": 4,
                "memory_bytes": 2**34,
                "kernel": "7.0",
            },
            "software": {
                "zope_python": "3.13",
                "eggs": "eggs",
                "zoedb": "0",
                "zoe": "zoe 0",
                "locust": "2",
            },
        },
        "steps_results": [step_result(ok, server_resources)],
        "verification": {"verdict": "pass"},
        "server": {},
    }
    (directory / "result.json").write_text(json.dumps(result))
    return directory


def column(header: str, text: str) -> int:
    cells = [c.strip() for c in header.strip("|").split("|")]
    matches = [i for i, c in enumerate(cells) if text.lower() in c.lower()]
    assert len(matches) == 1, (text, cells)
    return matches[0]


def test_the_report_gives_the_store_bytes_beside_the_kernel_counter(tmp_path):
    runs = [
        write_result(
            tmp_path / "zeo",
            "zeo",
            100,
            {
                "cpu_seconds": 1.0,
                "write_bytes": 500 * KB * 100,
                "store_write_bytes": 156 * KB * 100,
            },
        ),
        write_result(
            tmp_path / "zoe-native",
            "zoe-native",
            200,
            {
                "cpu_seconds": 1.0,
                "write_bytes": 11_067 * KB * 200,
                "store_write_bytes": 1_035 * KB * 200,
            },
        ),
        # A run on macOS: no store bytes.
        write_result(
            tmp_path / "zoe-native-qi",
            "zoe-native-qi",
            50,
            {"cpu_seconds": 1.0, "write_bytes": 4 * KB * 50},
        ),
    ]
    text, summary = report.build(runs)

    zeo = summary["edit|zeo"]["steps"][0]
    native = summary["edit|zoe-native"]["steps"][0]
    qi = summary["edit|zoe-native-qi"]["steps"][0]
    assert zeo["store_write_kb_per_op"] == 156.0
    assert native["store_write_kb_per_op"] == 1_035.0
    assert qi["store_write_kb_per_op"] is None
    # The kernel counter keeps its value.
    assert zeo["server_write_kb_per_op"] == 500.0
    assert native["server_write_kb_per_op"] == 11_067.0
    assert qi["server_write_kb_per_op"] == 4.0

    lines = text.splitlines()
    header = next(line for line in lines if line.startswith("| stack |"))
    store = column(header, "store KB written/op")
    kernel = column(header, "page cache")
    assert store != kernel

    def cells(stack):
        row = next(line for line in lines if line.startswith(f"| {stack} |"))
        return [c.strip() for c in row.strip("|").split("|")]

    assert float(cells("zeo")[store]) == 156.0
    assert float(cells("zeo")[kernel]) == 500.0
    assert float(cells("zoe-native")[store]) == 1_035.0
    assert float(cells("zoe-native")[kernel]) == 11_067.0
    assert cells("zoe-native-qi")[store] == "–"
    assert float(cells("zoe-native-qi")[kernel]) == 4.0
