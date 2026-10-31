"""Acceptance test of the native merges of BTrees and `Length`.

ZOE merges a conflict on a BTree bucket, a set, a BTree or a tree set
that holds one bucket, or a `Length`, at the vote, in Rust. The client
runs no resolution for these classes.

FileStorage with ZODB's resolver is the reference. Each case runs the same
three transactions on FileStorage and on ZOE: one creates the object, two
change it from the same snapshot, and the second of them has a conflict.
The test checks, for each case:

- the outcome is the same: both merge, or both raise `ConflictError`;
- the record that ZOE stores is the record that FileStorage stores, byte
  for byte;
- the second connection sees the merged state after its commit, so the
  vote returned the oid as resolved;
- ZOE calls `tryToResolveConflict` in the client for no case;
- through `zoe serve`, `zoe_conflicts_resolved_total{path="native"}` of
  `GET /metrics` counts each merged case.

A random generator with a fixed seed makes the cases: each key family of
BTrees, keys and values of each type that the family takes, inserts,
changes and deletes on each side. A bucket whose values are of mixed
types falls through to the client: the result is the same as today, and
the client resolution runs.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import dataclasses
import random
import threading

import pytest
from support import TIMEOUT, metrics, native
from support import client as zeo_client

import zoe
import zoe.testing
from zoe.testing import ZoeServer

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The seed of the generator of the cases.
SEED = 4203
# The cases of each family and each kind of object.
CASES = 3
# The families of BTrees: the type of the keys, then the type of the values.
FAMILIES = (
    "OO", "OI", "OU", "OL", "OQ",
    "IO", "II", "IU", "IF",
    "UO", "UI", "UU", "UF",
    "LO", "LL", "LQ", "LF",
    "QO", "QL", "QQ", "QF",
)  # fmt: skip
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")


@dataclasses.dataclass(frozen=True)
class Case:
    """One conflict: the object, its first state and the two changes.

    `kind` is `mapping` (a bucket or a BTree), `set` (a set or a tree
    set) or `length`. `base` is a list of pairs, a list of keys or an
    int. `committed` and `new` are the changes of the first and the
    second transaction: `("set", key, value)`, `("delete", key)`,
    `("add", key)`, `("remove", key)` or `("change", delta)`.
    """

    module: str
    cls: str
    kind: str
    base: object
    committed: tuple
    new: tuple

    def make(self):
        """Return a new object in the first state."""
        import importlib

        cls = getattr(importlib.import_module(self.module), self.cls)
        return cls(dict(self.base) if self.kind == "mapping" else self.base)

    def apply(self, obj, changes):
        """Make the changes on obj."""
        for change in changes:
            match change:
                case ("set", key, value):
                    obj[key] = value
                case ("delete", key):
                    del obj[key]
                case ("add", key):
                    obj.add(key)
                case ("remove", key):
                    obj.remove(key)
                case ("change", delta):
                    obj.change(delta)


def snapshot(obj):
    """Return the state of obj as plain data."""
    if hasattr(obj, "change"):
        return obj()
    if hasattr(obj, "items"):
        return list(obj.items())
    return list(obj.keys())


def keys_of(kind, rng):
    """Return a function that draws a key of the type `kind`."""
    if kind == "I":
        return lambda: rng.randint(-(2**31), 2**31 - 1)
    if kind == "U":
        return lambda: rng.randint(0, 2**32 - 1)
    if kind == "L":
        return lambda: rng.randint(-(2**63), 2**63 - 1)
    if kind == "Q":
        return lambda: rng.randint(0, 2**64 - 1)
    # Object keys: one type for each case, so that Python orders them.
    return objects_of(rng.choice(("str", "bytes", "int", "float", "tuple")), rng)


def values_of(kind, rng):
    """Return a function that draws a value of the type `kind`."""
    if kind == "F":
        # A float of 32 bits holds these values exactly.
        return lambda: rng.randint(-(2**20), 2**20) / 4
    if kind in "IULQ":
        return keys_of(kind, rng)
    # Object values: one type for each case, so that BTrees compares them.
    return objects_of(rng.choice(("str", "bytes", "int", "float", "tuple")), rng)


def objects_of(kind, rng):
    """Return a function that draws a Python object of the type `kind`."""
    letters = "abcdefghijklmnopqrstuvwxyzé"
    if kind == "str":
        return lambda: "".join(rng.choices(letters, k=rng.randint(0, 8)))
    if kind == "bytes":
        return lambda: rng.randbytes(rng.randint(0, 8))
    if kind == "int":
        return lambda: rng.randint(-(2**70), 2**70)
    if kind == "float":
        return lambda: rng.uniform(-1e6, 1e6)
    return lambda: (rng.randint(-100, 100), "".join(rng.choices(letters, k=2)))


def changes(rng, kind, state, fresh, value):
    """Return one to three changes of state, a dict or a set.

    `fresh` is a list of keys that the first state does not hold, which
    both sides draw from, so that two inserts of one key happen.
    """
    state = dict(state) if kind == "mapping" else set(state)
    result = []
    for _ in range(rng.randint(1, 3)):
        existing = sorted(state, key=repr)
        operation = rng.choice(("insert", "insert", "change", "delete"))
        if operation == "change" and kind == "set":
            operation = "insert"
        if operation != "insert" and not existing:
            operation = "insert"
        if operation == "insert":
            key = rng.choice(fresh)
            if key in state:
                continue
        else:
            key = rng.choice(existing)
        if kind == "mapping":
            if operation == "delete":
                del state[key]
                result.append(("delete", key))
            else:
                state[key] = value()
                result.append(("set", key, state[key]))
        elif operation == "delete":
            state.discard(key)
            result.append(("remove", key))
        else:
            state.add(key)
            result.append(("add", key))
    return tuple(result)


def distinct(draw, count, taken):
    """Return `count` distinct keys of draw that are not in taken."""
    keys = []
    while len(keys) < count:
        key = draw()
        if key not in keys and key not in taken:
            keys.append(key)
    return keys


def generated_cases():
    """Return the cases, the same list at each call."""
    rng = random.Random(SEED)
    cases = []
    for family in FAMILIES:
        module = f"BTrees.{family}BTree"
        for cls, kind in (
            (f"{family}Bucket", "mapping"),
            (f"{family}BTree", "mapping"),
            (f"{family}Set", "set"),
            (f"{family}TreeSet", "set"),
        ):
            for _ in range(CASES):
                key = keys_of(family[0], rng)
                value = values_of(family[1], rng)
                # A BTree and a tree set keep these sizes in one bucket.
                drawn = distinct(key, rng.randint(0, 10), ())
                fresh = distinct(key, 4, drawn)
                if kind == "mapping":
                    base = [(k, value()) for k in drawn]
                else:
                    base = drawn
                start = dict(base) if kind == "mapping" else set(base)
                committed = changes(rng, kind, start, fresh, value)
                new = changes(rng, kind, start, fresh, value)
                cases.append(Case(module, cls, kind, base, committed, new))
    for _ in range(2 * CASES):
        deltas = [
            tuple(("change", rng.randint(-5, 5)) for _ in range(rng.randint(1, 3)))
            for _ in range(2)
        ]
        cases.append(
            Case("BTrees.Length", "Length", "length", rng.randint(0, 99), *deltas)
        )
    return cases


class Calls:
    """Count the calls of `tryToResolveConflict`, in every thread."""

    def __init__(self):
        self.count = 0
        self._lock = threading.Lock()

    def install(self, monkeypatch):
        from ZODB.ConflictResolution import ConflictResolvingStorage

        original = ConflictResolvingStorage.tryToResolveConflict

        def counted(storage, *args, **kwargs):
            with self._lock:
                self.count += 1
            return original(storage, *args, **kwargs)

        monkeypatch.setattr(ConflictResolvingStorage, "tryToResolveConflict", counted)
        return self


@dataclasses.dataclass
class Result:
    """The end of one case on one storage."""

    outcome: str
    record: bytes
    state: object
    seen: object
    calls: int


def run(storage, cases, calls):
    """Run each case on storage, and close it. Return a list of Result."""
    import transaction
    import ZODB
    from ZODB.POSException import ConflictError

    db = ZODB.DB(storage)
    try:
        first_tm = transaction.TransactionManager()
        second_tm = transaction.TransactionManager()
        first = db.open(transaction_manager=first_tm)
        second = db.open(transaction_manager=second_tm)
        results = []
        for number, case in enumerate(cases):
            name = f"case-{number}"
            first_tm.begin()
            first.root()[name] = case.make()
            first_tm.commit()

            first_tm.begin()
            second_tm.begin()
            case.apply(first.root()[name], case.committed)
            case.apply(second.root()[name], case.new)
            first_tm.commit()
            before = calls.count
            try:
                second_tm.commit()
            except ConflictError:
                second_tm.abort()
                outcome = "conflict"
            else:
                outcome = "merged"
            used = calls.count - before

            second_tm.begin()
            seen = snapshot(second.root()[name])
            second_tm.abort()
            first_tm.begin()
            obj = first.root()[name]
            state = snapshot(obj)
            record = tuple(db.storage.loadBefore(obj._p_oid, MAX_TID))[0]
            first_tm.abort()
            results.append(Result(outcome, record, state, seen, used))
        first.close()
        second.close()
        return results
    finally:
        db.close()


class Server(ZoeServer):
    """A `zoe serve` process with an admin port, to read `/metrics`."""

    def __init__(self, path):
        super().__init__(str(path), args=["--admin-listen", "127.0.0.1:0"])

    def __enter__(self):
        return self.start(TIMEOUT)

    def native_merges(self):
        """Return `zoe_conflicts_resolved_total{path="native"}`."""
        text = metrics(self)
        prefix = 'zoe_conflicts_resolved_total{storage="1",path="native"} '
        for line in text.splitlines():
            if line.startswith(prefix):
                return int(line.split()[-1])
        raise AssertionError(f"/metrics has no {prefix.strip()}:\n{text}")


def client(mode, server):
    """Open a client of mode on server."""
    if mode == "zeo-client":
        return zeo_client(server)
    return native(server)


def run_on_zoe(mode, path, cases, calls):
    """Run the cases on ZOE in mode. Return the results and the native
    merges that the server counted, or None in-process."""
    if mode == "in-process":
        return run(zoe.Storage(str(path)), cases, calls), None
    with Server(path) as server:
        before = server.native_merges()
        results = run(client(mode, server), cases, calls)
        return results, server.native_merges() - before


@pytest.fixture(scope="module")
def cases():
    return generated_cases()


@pytest.fixture(scope="module")
def reference(cases, tmp_path_factory):
    """The results of the cases on FileStorage with ZODB's resolver."""
    from ZODB.FileStorage import FileStorage

    path = tmp_path_factory.mktemp("reference") / "Data.fs"
    calls = Calls()
    with pytest.MonkeyPatch.context() as monkeypatch:
        calls.install(monkeypatch)
        return run(FileStorage(str(path)), cases, calls)


def describe(case):
    return f"{case.cls} {case.base!r}: {case.committed!r} then {case.new!r}"


def test_the_cases_merge_and_refuse(cases, reference):
    # The generator gives both outcomes, so the test checks both paths.
    outcomes = {result.outcome for result in reference}
    assert outcomes == {"merged", "conflict"}
    # Each family gives a bucket, a BTree, a set and a tree set.
    assert len({case.cls for case in cases}) == 4 * len(FAMILIES) + 1


@pytest.mark.parametrize("mode", MODES)
def test_native_merges_equal_the_merges_of_zodb(
    mode, cases, reference, tmp_path, monkeypatch
):
    calls = Calls().install(monkeypatch)
    results, native = run_on_zoe(mode, tmp_path / "store", cases, calls)

    problems = []
    for case, ours, theirs in zip(cases, results, reference, strict=True):
        if ours.outcome != theirs.outcome:
            problems.append(f"{describe(case)}: {ours.outcome}, not {theirs.outcome}")
        elif ours.record != theirs.record:
            problems.append(f"{describe(case)}: the record is not FileStorage's")
        elif ours.seen != theirs.state:
            problems.append(
                f"{describe(case)}: the second connection sees an old state"
            )
        if ours.calls:
            problems.append(
                f"{describe(case)}: the client ran tryToResolveConflict "
                f"{ours.calls} time(s)"
            )
    shown = "\n".join(problems[:10])
    assert not problems, f"{len(problems)} of {len(cases)} cases:\n{shown}"

    if native is not None:
        merged = sum(result.outcome == "merged" for result in reference)
        assert native == merged


@pytest.mark.parametrize("mode", MODES)
def test_values_of_mixed_types_fall_through_to_the_client(mode, tmp_path, monkeypatch):
    # BTrees cannot compare 1 and "x". ZODB's resolver gets a SystemError
    # from the merge of BTrees and raises ConflictError. Rust does not
    # decide, so the conflict goes on to the client as today.
    case = Case(
        "BTrees.OOBTree",
        "OOBucket",
        "mapping",
        [("a", 1), ("b", 2)],
        (("set", "a", "x"),),
        (("set", "b", 3),),
    )
    from ZODB.FileStorage import FileStorage

    with pytest.MonkeyPatch.context() as patch:
        reference_calls = Calls().install(patch)
        (theirs,) = run(FileStorage(str(tmp_path / "Data.fs")), [case], reference_calls)
    assert theirs.outcome == "conflict"

    calls = Calls().install(monkeypatch)
    (ours,), native = run_on_zoe(mode, tmp_path / "store", [case], calls)
    assert ours.outcome == theirs.outcome
    assert ours.record == theirs.record
    assert ours.calls >= 1
    if native is not None:
        assert native == 0
