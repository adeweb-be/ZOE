"""The report of a result of the replay: one row per run, the conflicts per
1,000 transactions by class and by path, and the verdict of the gate."""

import json
from pathlib import Path

from replay.gate import LIMIT, gate
from replay.runner import PATHS, STACKS


class NotAResult(Exception):
    """The file is not a result of the replay."""


def load(path) -> dict:
    """Read a result and check it against the schema. Raise `NotAResult`."""
    import jsonschema

    from replay import SCHEMA

    try:
        document = json.loads(Path(path).read_text())
        jsonschema.validate(document, json.loads(Path(SCHEMA).read_text()))
    except (OSError, ValueError, jsonschema.ValidationError) as error:
        raise NotAResult(f"{path} is not a result of the replay: {error}") from error
    return document


def _rate(count, transactions):
    return 1000 * count / transactions if transactions else 0.0


def lines(result: dict) -> list[str]:
    """Return the lines of the report. The last one is the verdict."""
    out = [
        "stack       speed  rep  transactions  ConflictErrors  per 1k"
        "  acknowledged  seconds"
    ]
    for run in result["runs"]:
        row = (
            f"{run['stack']:<11} {run['speed']:>4}x {run['repetition']:>4}"
            f"  {run['transactions']:>12}  {run['conflict_errors']:>14}"
            f"  {_rate(run['conflict_errors'], run['transactions']):>6.1f}"
            f"  {run['check']['acknowledged']:>12}  {run['seconds']:>7.1f}"
        )
        if not run["counts"]:
            row += f"  does not count: {len(run['check']['missing'])} missing"
        out.append(row)
    out.append("")
    out.append("conflicts per 1,000 transactions, over the runs that count")
    out.append(
        "(a vote that fails hides the conflicts that it resolved before its refusal)"
    )
    for stack in STACKS:
        runs = [r for r in result["runs"] if r["stack"] == stack and r["counts"]]
        if not runs:
            continue
        transactions = sum(r["transactions"] for r in runs)
        totals: dict[str, dict[str, int]] = {}
        for r in runs:
            for name, paths in r["conflicts"].items():
                for path, n in paths.items():
                    totals.setdefault(name, {}).setdefault(path, 0)
                    totals[name][path] += n
        out.append(f"{stack}:")
        out.append("  " + f"{'class':<48}" + "".join(f"{p:>15}" for p in PATHS))
        for name in sorted(totals, key=lambda n: (-sum(totals[n].values()), n)):
            cells = "".join(
                f"{_rate(totals[name].get(p, 0), transactions):>15.1f}" for p in PATHS
            )
            out.append(f"  {name:<48}{cells}")
    verdict = gate(result)
    out.append("")
    rates = ", ".join(
        f"{stack} {'none' if rate is None else f'{rate:.1f}'}"
        for stack, rate in verdict.rates.items()
    )
    detail = f"ConflictErrors per 1,000 transactions: {rates}; limit {LIMIT:.0%} of zeo"
    if verdict.missing:
        absent = ", ".join(f"{stack} at {speed}x" for stack, speed in verdict.missing)
        detail += f"; no run that counts for {absent}"
    word = "passed" if verdict.verdict == "pass" else "failed"
    out.append(f"gate {word}: {detail}")
    return out
