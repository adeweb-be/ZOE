"""The gate of phase 4: the `ConflictError`s that reach Zope on `zoe serve`
are at most 10 % of those on ZEO."""

import dataclasses
from fractions import Fraction

# The highest share of the ConflictErrors of ZEO that zoe serve may raise.
LIMIT = 0.1


def verdict(zeo, zoe) -> str:
    """Return "pass" when `zoe` is at most `LIMIT` of `zeo`, else "fail".

    With 0 on ZEO, only 0 on zoe serve passes. The comparison is exact, so
    10 % passes."""
    passed = Fraction(zoe) <= Fraction(zeo) * Fraction(str(LIMIT))
    return "pass" if passed else "fail"


@dataclasses.dataclass(frozen=True)
class Gate:
    # ConflictErrors per 1,000 transactions over the runs of a stack that
    # count. None for a stack without a run that counts.
    rates: dict
    # (stack, speed, repetition) of each run that does not count.
    excluded: list
    # (stack, speed) of each speed and stack without a run that counts.
    missing: list
    verdict: str


def gate(result: dict) -> Gate:
    """Return the gate of a result of the replay."""
    from replay.runner import STACKS

    errors = {stack: 0 for stack in STACKS}
    transactions = {stack: 0 for stack in STACKS}
    counted = set()
    excluded = []
    for run in result["runs"]:
        stack = run["stack"]
        if not run["counts"]:
            excluded.append((stack, run["speed"], run["repetition"]))
            continue
        if stack in errors:
            errors[stack] += run["conflict_errors"]
            transactions[stack] += run["transactions"]
            counted.add((stack, run["speed"]))
    missing = [
        (stack, speed)
        for speed in result["speeds"]
        for stack in STACKS
        if (stack, speed) not in counted
    ]
    stacks = {stack for stack, _speed in counted}
    exact = {
        stack: Fraction(1000 * errors[stack], max(1, transactions[stack]))
        if stack in stacks
        else None
        for stack in STACKS
    }
    rates = {
        stack: None if rate is None else float(rate) for stack, rate in exact.items()
    }
    passed = (
        not missing
        and None not in exact.values()
        and verdict(zeo=exact["zeo"], zoe=exact["zoe-native"]) == "pass"
    )
    return Gate(rates, excluded, missing, "pass" if passed else "fail")
