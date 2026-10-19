"""Acceptance test of the dashboard: the page shows every published family.

The page is `bench/dashboard/`. `just bench publish` copies each of its
files into the checkout of the results repository (ADR 0054), and GitHub
Pages serves the checkout. It has no build step:

- `index.html` renders: the tables, the charts, the method;
- `data.js` is a plain ES module with no access to the DOM. Its function
  `load(read)` gives the data of the page. `read(path)` returns a promise
  of the parsed JSON of a file of the site, by a path relative to the page.
  `load` reads `index.json` of version 2, then each document of the index,
  and returns the model of the page as plain JSON data:

  `documents`: one entry per entry of the index, in the order of the index.
  Each entry holds every key of its entry of the index, and:

  - `method`: these keys of the document, unchanged: `machine`, `python`,
    `software` and `run`; for `plone` also `fixture`, `settings`, `stacks`
    and `scenarios`; for `native` also `rule`.
  - `plone`: `tables`, one per scenario and step, in the order of the
    `scenarios` of the document, then of `step`. A table holds `scenario`,
    `step`, `users` and `rows`: one row per stack that ran the step, in the
    order of the `stacks` of the document. A row holds `stack`, `values`,
    `ratios` and `verification`. `values` holds each name of `COLUMNS`
    with the number of the step (`ok_per_second` is the median).
    `ratios` holds each name of `COLUMNS` with the value divided by the
    value of the row of `zeo` in the same step, or null when either value
    is null or the value of `zeo` is 0. `verification` lists the verdict of
    each run of the scenario and stack, in the order of `runs`.
  - `plone`: `series`, one per stack and scenario of `CURVES` that the
    document holds. A series holds `scenario`, `stack` and `points`: one
    point per step, in the order of `users`, with `users`, `ok_per_second`
    (the median) and `conflicts_per_1000_ops`.
  - `native`: `cells`, one per cell and scenario, in the order of the
    document: `mode`, `concurrency`, `object_size`,
    `objects_per_transaction`, `scenario`, `ratio`, `low`, `high` and
    `verdict`. `just bench native` writes the result of each scenario in
    the document: `ratio`, `low` and `high` (a number or null) and
    `verdict` (`pass`, `fail`, or null when undecided). In the model,
    `ratio`, `low` and `high` are the numbers of the document, or null when
    the document does not hold them. `verdict` is `pass`, `fail`,
    `undecided` when the document holds null, or null when the document
    does not hold the key. The page shows a dash for null.
  - `client`: `rows`, the rows of the document, in its order.

  `shootout`: the charts of zodbshootout, one per machine, Python build,
  cell and scenario of the `shootout` documents. A chart holds `machine`,
  `python`, `mode`, `concurrency`, `object_size`, `scenario` and `lines`:
  one line per storage, with `storage` and `points`, one point per
  document, in the order of `started`, with `started`, `commit`,
  `ops_per_second`, `p50_seconds` and `p99_seconds`. The charts show every
  run.

  `selection`: the documents of the tables, for the selectors of the page.
  One group per family `native`, `client` and `plone`, machine, Python
  build and, for `plone`, fixture. A group holds `family`, `machine`,
  `python`, `fixture` (null except for `plone`), `latest` (the path of
  the newest document of the group) and `earlier` (the paths of the other
  documents, newest first). The newest document has the latest `started`,
  then the last path. The page shows the latest document of a group, and
  a selector gives the earlier ones.

The model holds no number that a document does not hold, except the ratios
of the B5 rows. The page requests only cdnjs, at a pinned version, and its
own files. A link to a repository on GitHub is not a request. `index.html`
loads `data.js`, fits the width of a phone, has a dark mode, and says how
to reproduce the results (`just bench showcase`).

The test publishes the sample documents of `tests/data/publish`, one of
each family, and the documents of `tests/data/dashboard` into an empty
checkout with `just bench publish --main HEAD`. Then it runs `data.js` of
the checkout under node, which devenv gives (`dashboard_model.mjs`). The
documents of `tests/data/dashboard` give:

- a second run of zeo and a run of zoe-native, for the charts of
  zodbshootout;
- the native gate on a second Python build, with the result of each
  scenario: a pass, a fail and two undecided scenarios. The sample of the
  native gate in `tests/data/publish` has no result, so its cells show a
  dash;
- a second run of the client benchmark, so a group has an earlier
  document;
- B5 on `medium`, with the curves of `edit` and `create-spread` at 1, 4,
  8, 16 and 32 users.

They hold numbers about a synthetic corpus, and the first commit of the
repository. The test runs no benchmark.
"""

import json
import math
import pathlib
import re
import shutil
import subprocess
import sys
from html.parser import HTMLParser

import pytest

from shootout import native

BENCH = pathlib.Path(__file__).parents[2]
REPOSITORY = BENCH.parent
DASHBOARD = BENCH / "dashboard"
TESTS = pathlib.Path(__file__).parent
SAMPLES = TESTS / "data" / "publish"
EXTRA = TESTS / "data" / "dashboard"
DRIVER = TESTS / "dashboard_model.mjs"
FAMILIES = ("shootout", "native", "client", "plone")
BASELINE = "zeo"
CURVES = ("edit", "create-spread")
# The families of the tables. The page shows the latest document of each
# group, and a selector gives the earlier ones.
SELECTED = ("native", "client", "plone")
# The numbers of a row of a B5 table, by their name in a step.
COLUMNS = (
    "ok_per_second",
    "p50_seconds",
    "p95_seconds",
    "p99_seconds",
    "conflicts_per_1000_ops",
    "first_attempt_share",
    "exhausted",
    "zope_cpu_ms_per_op",
    "server_cpu_ms_per_op",
    "store_write_kb_per_op",
    "objects_loaded_per_second",
    "objects_stored_per_second",
)
METHOD = {
    "shootout": ("machine", "python", "software", "run"),
    "native": ("machine", "python", "software", "run", "rule"),
    "client": ("machine", "python", "software", "run"),
    "plone": (
        "machine",
        "python",
        "software",
        "run",
        "fixture",
        "settings",
        "stacks",
        "scenarios",
    ),
}
# The files of the page, which the browser loads.
PAGE_SUFFIXES = (".html", ".js", ".mjs", ".css")
CDNJS = re.compile(
    r"https://cdnjs\.cloudflare\.com/ajax/libs/[A-Za-z0-9._-]+/\d+\.\d+\.\d+[^/]*/"
)
# The absolute URLs that are not requests: links to the repositories and
# their commits, and the namespaces of SVG and XML.
LINKS = ("https://github.com/adeweb-be/", "http://www.w3.org/")
ABSOLUTE = re.compile(r"""https?://[^\s"'`<>()\\]+""")
# The literal targets of a request in a script or a style sheet.
REQUESTS = re.compile(
    r"""(?:\bfrom|\bimport\s*\(|\bfetch\s*\(|\bWorker\s*\(|@import)\s*["'`]([^"'`]+)["'`]"""
    r"""|\burl\(\s*["']?([^"')\s]+)"""
)


def run(*args, **kwargs):
    return subprocess.run(
        args, capture_output=True, text=True, check=False, cwd=REPOSITORY, **kwargs
    )


def output(done):
    return done.stdout + done.stderr


@pytest.fixture(scope="module")
def site(tmp_path_factory):
    """A checkout of the results repository after a publish of every sample."""
    root = tmp_path_factory.mktemp("checkout")
    (root / "README.md").write_text("# Benchmark results\n")
    (root / "index.json").write_text(
        json.dumps({"index_version": 1, "results": []}, indent=2) + "\n"
    )
    publish = ("publish", "--main", "HEAD", SAMPLES, EXTRA, root)
    done = run(sys.executable, "-m", "shootout", *map(str, publish))
    assert done.returncode == 0, output(done)
    return root


@pytest.fixture(scope="module")
def page(site):
    """What `load` of data.js asked for, and the model that it returned."""
    node = shutil.which("node")
    assert node, "node is missing: run the test inside devenv shell"
    done = run(node, str(DRIVER), str(site), timeout=60)
    assert done.returncode == 0, output(done)
    return json.loads(done.stdout)


@pytest.fixture(scope="module")
def index(site):
    return json.loads((site / "index.json").read_text())


@pytest.fixture(scope="module")
def docs(site, index):
    """Each document of the index, by its path."""
    return {
        entry["path"]: json.loads((site / entry["path"]).read_text())
        for entry in index["results"]
    }


def entries(page, family):
    return [d for d in page["model"]["documents"] if d["family"] == family]


def holds(got, expected):
    """True when got holds each key of expected with the same value."""
    return all(key in got and got[key] == value for key, value in expected.items())


def close(got, expected):
    if expected is None:
        return got is None
    return got is not None and math.isclose(got, expected, rel_tol=1e-9)


def value(step, name):
    return step["ok_per_second"]["median"] if name == "ok_per_second" else step[name]


def finite(number):
    return number if math.isfinite(number) else None


def ratio(number, base):
    if number is None or base is None or base == 0:
        return None
    return number / base


def numbers(data, skip=lambda keys: False, keys=()):
    """Yield (keys, number) for each number in data, but not under skip."""
    if skip(keys):
        return
    if isinstance(data, dict):
        for key, item in data.items():
            yield from numbers(item, skip, (*keys, key))
    elif isinstance(data, list):
        for position, item in enumerate(data):
            yield from numbers(item, skip, (*keys, position))
    elif isinstance(data, int | float) and not isinstance(data, bool):
        yield keys, data


def page_files():
    return sorted(
        path
        for path in DASHBOARD.rglob("*")
        if path.is_file() and path.suffix in PAGE_SUFFIXES
    )


class Tags(HTMLParser):
    """The attributes of each start tag of an HTML page."""

    def __init__(self):
        super().__init__()
        self.tags = []

    def handle_starttag(self, tag, attrs):
        self.tags.append((tag, dict(attrs)))


def tags(path):
    parser = Tags()
    parser.feed(path.read_text())
    return parser.tags


def relative(target):
    """True for a path relative to the page, inside the site."""
    return (
        "://" not in target
        and not target.startswith(("/", "data:", "javascript:"))
        and ".." not in target.split("/")
    )


def own(target):
    """True for a target that the page itself holds: a relative path, a
    data: URI or a fragment."""
    return relative(target) or target.startswith("data:")


def test_publish_copies_the_page_into_the_checkout(site):
    for name in ("index.html", "data.js", "README.md"):
        assert (DASHBOARD / name).is_file(), f"bench/dashboard/{name} is missing"
    for path in DASHBOARD.rglob("*"):
        if path.is_file():
            copy = site / path.relative_to(DASHBOARD)
            assert copy.read_bytes() == path.read_bytes(), copy
    # The README of the results repository gives the layout of version 2.
    readme = (DASHBOARD / "README.md").read_text()
    for text in (
        "index.json",
        "results/shootout/",
        "results/native/",
        "results/client/",
        "results/plone/",
        "just bench publish",
    ):
        assert text in readme, text


def test_the_model_holds_every_document_of_the_index(page, index):
    results = index["results"]
    assert index["index_version"] == 2
    assert {entry["family"] for entry in results} == set(FAMILIES)
    documents = page["model"]["documents"]
    assert [d["path"] for d in documents] == [e["path"] for e in results]
    for document, entry in zip(documents, results, strict=True):
        assert holds(document, entry), entry["path"]
    # load reads the index, then each document, by relative paths only.
    requested = page["requested"]
    assert requested[0] == "index.json"
    assert sorted(requested[1:]) == sorted(e["path"] for e in results)
    assert all(relative(path) for path in requested), requested


def test_the_method_holds_the_run_of_each_document(page, docs):
    for document in page["model"]["documents"]:
        source = docs[document["path"]]
        method = document["method"]
        for key in METHOD[document["family"]]:
            assert method[key] == source[key], (document["path"], key)


def test_a_b5_table_per_step_holds_its_numbers_and_their_ratio_to_zeo(page, docs):
    campaigns = entries(page, "plone")
    assert {d["method"]["fixture"]["name"] for d in campaigns} == {
        "small-ba01ff255313",
        "medium-8ecaa36fc3c4",
    }
    for document in campaigns:
        source = docs[document["path"]]
        order = [stack["name"] for stack in source["stacks"]]
        expected = []
        for scenario in source["scenarios"]:
            steps = [s for s in source["steps"] if s["scenario"] == scenario["name"]]
            for number in sorted({s["step"] for s in steps}):
                ran = {s["stack"]: s for s in steps if s["step"] == number}
                expected.append((scenario["name"], number, ran))
        tables = document["tables"]
        assert [(t["scenario"], t["step"]) for t in tables] == [
            (scenario, number) for scenario, number, _ in expected
        ], document["path"]
        for table, (scenario, number, ran) in zip(tables, expected, strict=True):
            where = (document["path"], scenario, number)
            assert table["users"] == next(iter(ran.values()))["users"], where
            stacks = [name for name in order if name in ran]
            assert [row["stack"] for row in table["rows"]] == stacks, where
            base = ran.get(BASELINE)
            for row in table["rows"]:
                step = ran[row["stack"]]
                verdicts = [
                    r["verification"]
                    for r in source["runs"]
                    if r["scenario"] == scenario and r["stack"] == row["stack"]
                ]
                assert row["verification"] == verdicts, (*where, row["stack"])
                for name in COLUMNS:
                    cell = (*where, row["stack"], name)
                    assert row["values"][name] == value(step, name), cell
                    zeo = value(base, name) if base else None
                    assert close(row["ratios"][name], ratio(value(step, name), zeo)), (
                        cell,
                        row["ratios"][name],
                    )


def test_b5_charts_users_against_operations_and_conflicts(page, docs):
    found = set()
    for document in entries(page, "plone"):
        source = docs[document["path"]]
        expected = {}
        for step in source["steps"]:
            if step["scenario"] in CURVES:
                expected.setdefault((step["scenario"], step["stack"]), []).append(
                    {
                        "users": step["users"],
                        "ok_per_second": step["ok_per_second"]["median"],
                        "conflicts_per_1000_ops": step["conflicts_per_1000_ops"],
                    }
                )
        series = {(s["scenario"], s["stack"]): s["points"] for s in document["series"]}
        assert set(series) == set(expected), document["path"]
        for key, points in expected.items():
            points.sort(key=lambda point: point["users"])
            got = series[key]
            assert len(got) == len(points), key
            assert all(holds(g, p) for g, p in zip(got, points, strict=True)), key
            found.add(key[0])
    assert found == set(CURVES)


def test_the_native_gate_gives_each_cell_its_ratio_interval_and_verdict(page, docs):
    runs = entries(page, "native")
    assert {d["python"] for d in runs} == {"cp313", "cp314t"}
    verdicts = set()
    for document in runs:
        source = docs[document["path"]]
        rule = source["rule"]
        expected = []
        for cell in source["cells"]:
            for scenario in cell["scenarios"]:
                where = (document["path"], cell["mode"], scenario["name"])
                if "verdict" in scenario:
                    # The result in the document is the result of the rule
                    # of the gate on the pairs.
                    v = native.decide(
                        native.ratios(scenario["pairs"]),
                        rule["level"],
                        rule["min_pairs"],
                    )
                    for name in ("ratio", "low", "high"):
                        assert close(scenario[name], finite(getattr(v, name))), where
                    assert scenario["verdict"] == v.verdict, where
                    verdict = scenario["verdict"] or "undecided"
                else:
                    verdict = None
                expected.append(
                    {
                        "mode": cell["mode"],
                        "concurrency": cell["concurrency"],
                        "object_size": cell["object_size"],
                        "objects_per_transaction": cell["objects_per_transaction"],
                        "scenario": scenario["name"],
                        "ratio": scenario.get("ratio"),
                        "low": scenario.get("low"),
                        "high": scenario.get("high"),
                        "verdict": verdict,
                    }
                )
        cells = document["cells"]
        assert len(cells) == len(expected), document["path"]
        for got, keys in zip(cells, expected, strict=True):
            assert holds(got, keys), (document["path"], keys, got)
            verdicts.add(got["verdict"])
    # A document without the results gives null: the page shows a dash.
    assert verdicts == {"pass", "fail", "undecided", None}


def test_the_client_benchmark_gives_each_row(page, docs):
    runs = entries(page, "client")
    assert runs
    for document in runs:
        rows = docs[document["path"]]["rows"]
        assert len(document["rows"]) == len(rows), document["path"]
        for got, row in zip(document["rows"], rows, strict=True):
            assert holds(got, row), (document["path"], row)


def test_zodbshootout_charts_each_cell_against_the_start_of_the_run(page, index, docs):
    expected = {}
    for entry in index["results"]:
        if entry["family"] != "shootout":
            continue
        source = docs[entry["path"]]
        for scenario in source["scenarios"]:
            key = (
                entry["machine"],
                entry["python"],
                scenario["mode"],
                scenario["concurrency"],
                scenario["object_size"],
                scenario["name"],
            )
            point = {
                "started": source["run"]["started"],
                "commit": source["run"]["commit"],
                "ops_per_second": scenario["ops_per_second"],
                "p50_seconds": scenario["p50_seconds"],
                "p99_seconds": scenario["p99_seconds"],
            }
            expected.setdefault(key, {}).setdefault(source["storage"], []).append(point)
    charts = {
        (
            c["machine"],
            c["python"],
            c["mode"],
            c["concurrency"],
            c["object_size"],
            c["scenario"],
        ): c
        for c in page["model"]["shootout"]
    }
    assert set(charts) == set(expected)
    for key, lines in expected.items():
        got = {line["storage"]: line["points"] for line in charts[key]["lines"]}
        assert set(got) == set(lines), key
        for storage, points in lines.items():
            points.sort(key=lambda point: point["started"])
            assert len(got[storage]) == len(points), (key, storage)
            assert all(
                holds(g, p) for g, p in zip(got[storage], points, strict=True)
            ), (key, storage)
    # Two runs of zeo give a line of two points.
    assert any(len(line["points"]) == 2 for c in charts.values() for line in c["lines"])


def test_the_selection_gives_the_latest_document_of_each_group(page, index):
    groups = {}
    for entry in index["results"]:
        if entry["family"] in SELECTED:
            key = (entry["family"], entry["machine"], entry["python"], entry["fixture"])
            groups.setdefault(key, []).append(entry)
    selection = {
        (g["family"], g["machine"], g["python"], g["fixture"]): g
        for g in page["model"]["selection"]
    }
    assert set(selection) == set(groups)
    for key, members in groups.items():
        members.sort(key=lambda e: (e["started"], e["path"]))
        newest = [e["path"] for e in reversed(members)]
        assert selection[key]["latest"] == newest[0], key
        assert selection[key]["earlier"] == newest[1:], key
    # Two runs of the client benchmark give a group with an earlier document.
    assert any(group["earlier"] for group in selection.values())


def test_the_model_holds_no_number_that_a_document_does_not_hold(page, docs, index):
    held = {number for _, number in numbers(index)}
    for document in docs.values():
        held.update(number for _, number in numbers(document))

    def derived(keys):
        # The ratios of a B5 row. The test of the tables checks them.
        return bool(keys) and keys[-1] == "ratios"

    invented = [
        (keys, number)
        for keys, number in numbers(page["model"], derived)
        if number not in held
    ]
    assert not invented, invented[:10]


def test_the_page_requests_only_cdnjs_and_its_own_files():
    files = page_files()
    assert DASHBOARD / "index.html" in files
    assert DASHBOARD / "data.js" in files
    for path in files:
        text = path.read_text()
        for url in ABSOLUTE.findall(text):
            if url.startswith(LINKS):
                continue
            assert CDNJS.match(url), f"{path.name}: {url} is not cdnjs at a version"
        for groups in REQUESTS.findall(text):
            target = "".join(groups)
            if target.startswith("https://"):
                assert CDNJS.match(target), f"{path.name}: requests {target}"
            else:
                assert own(target), f"{path.name}: requests {target}"
        if path.suffix == ".html":
            for tag, attrs in tags(path):
                for name in ("src", "href", "data", "poster", "srcset"):
                    target = attrs.get(name)
                    if not target or (tag == "a" and name == "href"):
                        continue
                    where = f"{path.name}: <{tag} {name}={target}>"
                    if target.startswith("https://"):
                        assert CDNJS.match(target), where
                    else:
                        assert own(target), where


def test_the_page_reads_its_data_through_data_js_and_fits_a_phone_and_dark_mode():
    html = DASHBOARD / "index.html"
    assert html.is_file(), "bench/dashboard/index.html is missing"
    text = html.read_text()
    assert re.search(r"""["'](\./)?data\.js["']""", text), (
        "index.html does not load data.js"
    )
    viewports = [
        attrs.get("content", "")
        for tag, attrs in tags(html)
        if tag == "meta" and attrs.get("name") == "viewport"
    ]
    assert any("width=device-width" in v for v in viewports), viewports
    assert re.search(r"prefers-color-scheme\s*:\s*dark", text), "no dark mode"
    # The method says how to reproduce the results.
    assert any("just bench showcase" in p.read_text() for p in page_files())
