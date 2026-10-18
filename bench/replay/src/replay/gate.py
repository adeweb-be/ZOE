"""The measure of the success criterion of B6: the `ConflictError`s that
reach Zope on `zoe serve`, as a share of those on ZEO. It is a measure, not
a gate."""

import dataclasses
from fractions import Fraction


@dataclasses.dataclass(frozen=True)
class Gate:
    # ConflictErrors per 1,000 transactions over the runs of a stack that
    # count. None for a stack without a run that counts.
    rates: dict
    # (stack, speed, repetition) of each run that does not count: its check
    # failed, or it replayed no transaction.
    excluded: list
    # (stack, speed) of each speed and stack without a run that counts.
    missing: list
    # The rate of zoe-native over the rate of zeo. None when a stack has no
    # run that counts, or when zeo has no ConflictError.
    ratio: float | None


def gate(result: dict) -> Gate:
    """Return the measure of a result of the replay."""
    from replay.runner import STACKS

    errors = {stack: 0 for stack in STACKS}
    transactions = {stack: 0 for stack in STACKS}
    counted = set()
    excluded = []
    for run in result["runs"]:
        stack = run["stack"]
        # A run that replayed no transaction says nothing of the conflicts.
        if not run["counts"] or not run["transactions"]:
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
        stack: Fraction(1000 * errors[stack], transactions[stack])
        if stack in stacks
        else None
        for stack in STACKS
    }
    rates = {
        stack: None if rate is None else float(rate) for stack, rate in exact.items()
    }
    zeo, zoe = exact["zeo"], exact["zoe-native"]
    ratio = None if zeo is None or zoe is None or zeo == 0 else float(zoe / zeo)
    return Gate(rates, excluded, missing, ratio)
