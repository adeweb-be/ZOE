"""The result of the native gate holds the outcome of each scenario of
each cell: the ratio, its interval and the verdict, as `decide` gives them
on the pairs. So a reader of the result, such as the dashboard, does not
apply the rule again.

The tests run no server and no benchmark: they replace the worker
processes and the pairs of a cell.
"""

import contextlib
import io
import json
import math
import pathlib

import jsonschema
import pytest

from shootout import native

SCHEMA = json.loads(native.SCHEMA.read_text())
SAMPLE = pathlib.Path(__file__).parent / "data" / "publish" / "native.json"


def pairs_of(ratio: float, count: int) -> list[dict]:
    """Return count pairs whose ratios t_zeo / t_zoe spread around ratio."""
    pairs = []
    for number in range(count):
        spread = ratio * (1 + 0.01 * (number % 3 - 1))
        pairs.append(
            {
                "first": native.order(number)[0],
                "seconds": {"zeo": 0.001 * spread, "zoe-native": 0.001},
            }
        )
    return pairs


CASES = {
    "pass": pairs_of(1.5, 10),
    "fail": pairs_of(0.7, 10),
    "undecided": pairs_of(1.0, 12),
    "one-pair": pairs_of(1.2, 1),
}


def expected(pairs):
    return native.decide(native.ratios(pairs), native.LEVEL, native.MIN_PAIRS)


@pytest.mark.parametrize("case", sorted(CASES))
def test_the_outcome_is_the_verdict_of_decide(case):
    pairs = CASES[case]
    v = expected(pairs)

    outcome = native.outcome(pairs)

    assert outcome["verdict"] == v.verdict
    for key in ("ratio", "low", "high"):
        value = getattr(v, key)
        if math.isfinite(value):
            assert outcome[key] == value, key
        else:
            assert outcome[key] is None, key
    # The values are JSON numbers: no NaN and no infinity.
    json.dumps(outcome, allow_nan=False)


def test_one_pair_has_no_high_bound():
    outcome = native.outcome(CASES["one-pair"])

    assert outcome["high"] is None
    assert outcome["verdict"] is None
    # The one ratio is 1.2 less 1 %.
    assert outcome["ratio"] == pytest.approx(1.188)


def test_a_cell_writes_the_outcome_of_each_scenario(monkeypatch, tmp_path):
    pairs = {
        "add": CASES["pass"],
        "update": CASES["fail"],
        "read_hot": CASES["one-pair"],
    }

    class Group:
        def __init__(self, config, cell, records):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

        def populate(self):
            pass

    monkeypatch.setattr(native, "zconfig", lambda *args: contextlib.nullcontext(""))
    monkeypatch.setattr(native, "_Group", Group)
    monkeypatch.setattr(native, "admin_address", lambda directory: "127.0.0.1:0")
    monkeypatch.setattr(
        native, "_pairs", lambda groups, cell, scenario, admin: (5, pairs[scenario])
    )
    monkeypatch.setattr(
        native.gil, "read", lambda path: [{"gil_enabled": True, "gil_enabled_by": []}]
    )

    entry = native._cell(native.CELLS[0], list(pairs), tmp_path, io.StringIO())

    for scenario in entry["scenarios"]:
        assert {k: scenario[k] for k in ("ratio", "low", "high", "verdict")} == (
            native.outcome(pairs[scenario["name"]])
        )
    assert [s["verdict"] for s in entry["scenarios"]] == ["pass", "fail", None]
    document = json.loads(SAMPLE.read_text())
    document["cells"] = [entry]
    jsonschema.validate(document, SCHEMA)


def test_a_result_without_the_outcome_stays_valid():
    document = json.loads(SAMPLE.read_text())
    assert all(
        "verdict" not in scenario
        for cell in document["cells"]
        for scenario in cell["scenarios"]
    )

    jsonschema.validate(document, SCHEMA)


def test_the_report_agrees_with_the_outcome(tmp_path):
    document = json.loads(SAMPLE.read_text())
    for cell in document["cells"]:
        for scenario in cell["scenarios"]:
            scenario.update(native.outcome(scenario["pairs"]))
    jsonschema.validate(document, SCHEMA)
    out = io.StringIO()

    native.report(document, out)

    rows = {line.split()[0]: line.split()[-1] for line in out.getvalue().splitlines()}
    verdicts = {"pass": "pass", "fail": "FAIL", None: "UNDECIDED"}
    for scenario in document["cells"][0]["scenarios"]:
        assert rows[scenario["name"]] == verdicts[scenario["verdict"]]
