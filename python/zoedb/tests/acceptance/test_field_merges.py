"""Acceptance test of the field merges of tier 2 and the merge policy.

The merge policy (ADR 0046) is the object `root['zoe.merge_policy']`.
`zoe.policy.install(connection, policy)` writes it, and the caller
commits. For a class that the policy marks mergeable (`tier2` `on`), the
vote merges a conflict field by field, in Rust: a field that one side
changed takes that change, a field that both sides changed to equal values
takes it, and a field that both sides changed to different values takes
the strategy of the policy (`strict`, `lww`, `max`, `sum`, `union` or
`append`). Coupled fields stay strict. A class without an entry never
merges in tier 2.

The test checks, with classes of this module that have no
`_p_resolveConflict`, so that no client resolves a conflict:

- two transactions that change different fields of one object both commit,
  and the second connection then sees the merged state;
- a coupled field, or one field changed differently on both sides, raises
  `ConflictError`, and the first commit stays;
- a class without an entry, a subclass of a class with an entry, and a
  class whose entry has `tier2` `off` never merge; before any policy,
  nothing merges;
- a new version of the policy applies from the next conflict, without a
  restart;
- in shadow mode (`tier2` `shadow`) the conflict is raised, and the
  resolution log holds the merge: a line of tier 2 with the outcome
  `merged` and the version of the policy;
- on generated states, tier 2 gives the outcome and the record, byte for
  byte, of the Python reference of each strategy below.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import contextlib
import dataclasses
import importlib
import io
import json
import random

import pytest
from persistent import Persistent
from support import TIMEOUT, serving

import zoe.testing

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The seed of the generator of the cases, and the number of cases.
SEED = 4205
CASES = 200
# The strategies of the fields of Item. A field that is not here takes the
# default, strict.
FIELDS = {
    "owner": "lww",
    "peak": "max",
    "views": "sum",
    "tags": "union",
    "log": "append",
}
# The coupled fields of Item.
COUPLED = [["start", "end"]]


class Item(Persistent):
    """An object of a class that the policy marks mergeable."""


class Unlisted(Persistent):
    """An object of a class that the policy does not name."""


class Derived(Item):
    """A subclass of Item. The policy does not name it."""


def dotted(cls):
    """Return the name of cls in the class pickle of a record."""
    return f"{cls.__module__}.{cls.__qualname__}"


def entry(tier2="on"):
    """Return the entry of Item in the policy."""
    return {
        "tier2": tier2,
        "default": "strict",
        "fields": dict(FIELDS),
        "coupled": [list(group) for group in COUPLED],
    }


def policy(tier2="on"):
    """Return a policy that names Item only."""
    return {"classes": {dotted(Item): entry(tier2)}}


def install(db, new_policy):
    """Install new_policy in one commit. Return its version."""
    import zoe.policy

    with db.transaction() as conn:
        zoe.policy.install(conn, new_policy)
    with db.transaction() as conn:
        return zoe.policy.read(conn)["version"]


@contextlib.contextmanager
def opened(mode, path, monkeypatch, environment=None):
    """Open a database of mode on the store in path. Close it at the end."""
    import ZODB
    import ZODB.config

    environment = environment or {}
    if mode == "in-process":
        for name, value in environment.items():
            monkeypatch.setenv(name, value)
        storage = ZODB.config.storageFromString(
            f"%import zoe\n<zoe>\n  path {path}\n</zoe>\n"
        )
        db = ZODB.DB(storage)
        try:
            yield db
        finally:
            db.close()
        return
    with serving(path, environment) as address:
        if mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(address, storage="1", wait_timeout=TIMEOUT)
        else:
            storage = zoe.testing.native_client(
                address, storage="1", wait_timeout=TIMEOUT
            )
        db = ZODB.DB(storage)
        try:
            yield db
        finally:
            db.close()


# The Python reference of tier 2 (ADR 0046, section 4). It decodes the
# three records, merges their states, and pickles the result with ZODB's
# pickler. It gives the meaning of each strategy. The human approves a
# change of it with the ADR.

# A field that a state does not hold.
ABSENT = object()
# The answer of a strategy that refuses or gives no answer.
REFUSED = object()


def find_global(module, name):
    return getattr(importlib.import_module(module), name)


def no_reference(ref):
    raise AssertionError(f"a state of the test holds a reference: {ref!r}")


def load(record):
    """Return the class and the state of record."""
    from ZODB._compat import PersistentUnpickler

    unpickler = PersistentUnpickler(find_global, no_reference, io.BytesIO(record))
    return unpickler.load(), unpickler.load()


def dump(cls, state):
    """Return the record that ZODB writes for an object of cls in state."""
    from ZODB.serialize import ObjectWriter

    obj = cls.__new__(cls)
    obj.__setstate__(state)
    return ObjectWriter().serialize(obj)


def same(a, b):
    """Return whether a and b are equal for the model of tier 2.

    Two values are equal when they are of the same kind and hold the same
    items in the same order.
    """
    if a is ABSENT or b is ABSENT:
        return a is b
    if type(a) is not type(b):
        return False
    if isinstance(a, (list, tuple)):
        return len(a) == len(b) and all(map(same, a, b))
    if isinstance(a, dict):
        return len(a) == len(b) and all(
            same(ka, kb) and same(va, vb)
            for (ka, va), (kb, vb) in zip(a.items(), b.items(), strict=True)
        )
    return a == b


def holds(items, item):
    return any(same(item, other) for other in items)


def shares_an_object(state):
    """Return whether two fields of state hold one object that a pickle
    memoizes: then tier 2 gives no answer."""
    owners = {}

    def walk(value, field):
        if isinstance(value, (str, bytes, list, dict)) or (
            isinstance(value, tuple) and value
        ):
            if owners.setdefault(id(value), field) != field:
                raise StopIteration
        if isinstance(value, (list, tuple)):
            for item in value:
                walk(item, field)
        elif isinstance(value, dict):
            for key, item in value.items():
                walk(key, field)
                walk(item, field)

    try:
        for field, value in state.items():
            walk(value, field)
    except StopIteration:
        return True
    return False


def strategy(name, old, committed, new):
    """Return the value of a field that both sides changed to different
    values, or REFUSED."""
    if name == "lww":
        return new
    if name == "max":
        if type(committed) is int and type(new) is int:
            return max(committed, new)
        return REFUSED
    if name == "sum":
        if type(old) is int and type(committed) is int and type(new) is int:
            return committed + new - old
        return REFUSED
    if name == "union":
        if not all(type(value) in (list, tuple) for value in (old, committed, new)):
            return REFUSED
        kept = [item for item in old if holds(committed, item) and holds(new, item)]
        added = [item for item in committed if not holds(old, item)]
        added += [item for item in new if not holds(old, item)]
        items = []
        for item in kept + added:
            if not holds(items, item):
                items.append(item)
        return type(new)(items)
    if name == "append":
        kinds = {type(old), type(committed), type(new)}
        if len(kinds) != 1 or kinds - {list, tuple}:
            return REFUSED
        prefix = len(old)
        if not (same(committed[:prefix], old) and same(new[:prefix], old)):
            return REFUSED
        return committed + new[prefix:]
    return REFUSED


def merge(item_entry, old, committed, new, used):
    """Return the merged state, or None for a conflict.

    Add the name of each strategy that decided a field to used.
    """
    if any(map(shares_an_object, (old, committed, new))):
        return None
    fields = item_entry.get("fields", {})
    default = item_entry.get("default", "strict")

    def value(state, field):
        return state.get(field, ABSENT)

    names = list(old)
    names += [field for field in committed if field not in old]
    names += [field for field in new if field not in old and field not in committed]
    ours = {f for f in names if not same(value(old, f), value(committed, f))}
    theirs = {f for f in names if not same(value(old, f), value(new, f))}
    for group in item_entry.get("coupled", ()):
        if ours.intersection(group) and theirs.intersection(group):
            if not all(same(value(committed, f), value(new, f)) for f in group):
                return None
    values = {}
    decided = set()
    for field in names:
        c, n = value(committed, field), value(new, field)
        if field not in theirs:
            values[field] = c
        elif field not in ours:
            values[field] = n
        elif same(c, n):
            values[field] = c
        elif c is ABSENT or n is ABSENT:
            # A delete against a change refuses.
            return None
        else:
            name = fields.get(field, default)
            values[field] = strategy(name, value(old, field), c, n)
            if values[field] is REFUSED:
                return None
            decided.add(name)
    used.update(decided)
    merged = {f: values[f] for f in committed if values[f] is not ABSENT}
    for field in new:
        if field not in committed and values[field] is not ABSENT:
            merged[field] = values[field]
    return merged


def reference(item_entry, old_record, committed_record, new_record, used):
    """Return the record of the reference merge, or None for a conflict."""
    _, old = load(old_record)
    _, committed = load(committed_record)
    cls, new = load(new_record)
    merged = merge(item_entry, old, committed, new, used)
    return None if merged is None else dump(cls, merged)


# The cases.


@dataclasses.dataclass(frozen=True)
class Case:
    """Two transactions that change one object of cls from one snapshot.

    `base` is the first state. `committed` and `new` are the changes of the
    first and the second transaction, each a tuple of `(kind, field,
    value)`.
    """

    name: str
    cls: type
    base: dict
    committed: tuple
    new: tuple


def apply(obj, changes):
    """Make changes on obj."""
    for kind, field, value in changes:
        if kind == "set":
            setattr(obj, field, value)
        elif kind == "delete":
            delattr(obj, field)
        elif kind == "add":
            setattr(obj, field, getattr(obj, field) + value)
        elif kind == "add-item":
            items = getattr(obj, field)
            if value not in items:
                setattr(obj, field, items + [value])
        elif kind == "remove-item":
            setattr(obj, field, [item for item in getattr(obj, field) if item != value])
        elif kind == "append":
            setattr(obj, field, getattr(obj, field) + [value])
        elif kind == "rewrite":
            setattr(obj, field, getattr(obj, field)[:-1] + [value])


def word(rng, prefix):
    """Return a new text of 7 characters. No two fields share it."""
    return f"{prefix}{rng.randrange(10**6):06d}"


def base_state(rng):
    tags = []
    while len(tags) < rng.randint(0, 4):
        tag = word(rng, "g")
        if tag not in tags:
            tags.append(tag)
    return {
        "title": word(rng, "t"),
        "description": word(rng, "d"),
        "owner": word(rng, "o"),
        "peak": rng.randint(0, 100),
        "views": rng.randint(0, 1000),
        "tags": tags,
        "log": [(word(rng, "e"), n) for n in range(rng.randint(0, 3))],
        "start": rng.randint(0, 50),
        "end": rng.randint(50, 100),
    }


def side(rng, base):
    """Return one to three changes of base."""
    changes = []
    for _ in range(rng.randint(1, 3)):
        kind = rng.choice(
            ("title", "description", "owner", "owner", "peak", "peak", "views",
             "views", "tag-add", "tag-add", "tag-remove", "log", "log", "rewrite",
             "start", "end", "extra", "delete")
        )  # fmt: skip
        if kind in ("title", "description", "owner"):
            changes.append(("set", kind, word(rng, kind[0])))
        elif kind == "peak":
            changes.append(("set", "peak", rng.randint(0, 200)))
        elif kind == "views":
            changes.append(("add", "views", rng.randint(1, 20)))
        elif kind == "tag-add":
            # A small set of names, so that both sides add one tag at times.
            changes.append(("add-item", "tags", f"n{rng.randrange(3)}tag"))
        elif kind == "tag-remove" and base["tags"]:
            changes.append(("remove-item", "tags", rng.choice(base["tags"])))
        elif kind == "log":
            changes.append(("append", "log", (word(rng, "e"), rng.randint(0, 9))))
        elif kind == "rewrite" and base["log"]:
            changes.append(("rewrite", "log", (word(rng, "r"), 0)))
        elif kind in ("start", "end"):
            changes.append(("set", kind, rng.randint(0, 100)))
        elif kind == "extra":
            changes.append(("set", "extra", word(rng, "x")))
        elif kind == "delete":
            changes.append(("delete", "description", None))
    return tuple(changes)


def effective(base, changes):
    """Return whether changes change the state base."""
    obj = Item()
    obj.__setstate__({field: value for field, value in base.items()})
    try:
        apply(obj, changes)
    except AttributeError:
        return False
    return not same(obj.__getstate__(), base)


def generated_cases():
    """Return the generated cases, the same list at each call."""
    rng = random.Random(SEED)
    cases = []
    while len(cases) < CASES:
        base = base_state(rng)
        committed, new = side(rng, base), side(rng, base)
        if effective(base, committed) and effective(base, new):
            cases.append(Case(f"case-{len(cases)}", Item, base, committed, new))
    return cases


def plain(obj):
    return dict(obj.__getstate__())


@dataclasses.dataclass
class Result:
    """The end of one case."""

    outcome: str
    old: bytes
    committed: bytes
    new: bytes
    stored: bytes
    seen: dict


def race(db, case):
    """Run case on a new object of the root of db. Return a Result."""
    import transaction
    from ZODB.POSException import ConflictError
    from ZODB.serialize import ObjectWriter

    first_tm = transaction.TransactionManager()
    second_tm = transaction.TransactionManager()
    first = db.open(transaction_manager=first_tm)
    second = db.open(transaction_manager=second_tm)
    try:
        first_tm.begin()
        obj = case.cls()
        for field, value in case.base.items():
            setattr(obj, field, value)
        first.root()[case.name] = obj
        first_tm.commit()
        oid = obj._p_oid
        old = db.storage.loadBefore(oid, MAX_TID)[0]

        first_tm.begin()
        second_tm.begin()
        mine = first.root()[case.name]
        theirs = second.root()[case.name]
        apply(mine, case.committed)
        apply(theirs, case.new)
        # The record that the second transaction stores.
        new = ObjectWriter(theirs).serialize(theirs)
        first_tm.commit()
        committed = db.storage.loadBefore(oid, MAX_TID)[0]
        try:
            second_tm.commit()
        except ConflictError:
            second_tm.abort()
            outcome = "conflict"
        else:
            outcome = "merged"
        second_tm.begin()
        seen = plain(second.root()[case.name])
        second_tm.abort()
        stored = db.storage.loadBefore(oid, MAX_TID)[0]
        return Result(outcome, old, committed, new, stored, seen)
    finally:
        first_tm.abort()
        first.close()
        second.close()


def check(result, expected):
    """Return what differs between result and the reference expected."""
    if expected is None:
        if result.outcome != "conflict":
            return "merged, not a conflict"
        if result.stored != result.committed:
            return "the first commit did not stay"
        return None
    if result.outcome != "merged":
        return "a conflict, not a merge"
    if result.stored != expected:
        return "the record is not the record of the reference"
    if result.seen != load(expected)[1]:
        return "the second connection sees another state"
    return None


BASE = {
    "title": "title-0",
    "description": "description-0",
    "owner": "owner-0",
    "peak": 10,
    "views": 100,
    "tags": ["red-0", "blue-0"],
    "log": [("created", 0)],
    "start": 1,
    "end": 9,
}

MERGES = [
    Case(
        "different fields",
        Item,
        BASE,
        (("set", "title", "title-1"),),
        (("set", "description", "description-1"),),
    ),
    Case(
        "one field changed to equal values",
        Item,
        BASE,
        (("set", "title", "title-1"), ("set", "start", 2)),
        (("set", "title", "title-1"), ("set", "description", "description-1")),
    ),
    Case(
        "a field added on one side",
        Item,
        BASE,
        (("set", "title", "title-1"),),
        (("set", "extra", "extra-1"),),
    ),
    Case(
        "each strategy",
        Item,
        BASE,
        (
            ("set", "owner", "owner-1"),
            ("set", "peak", 30),
            ("add", "views", 5),
            ("add-item", "tags", "green-1"),
            ("append", "log", ("edited", 1)),
        ),
        (
            ("set", "owner", "owner-2"),
            ("set", "peak", 20),
            ("add", "views", 7),
            ("remove-item", "tags", "red-0"),
            ("append", "log", ("edited", 2)),
        ),
    ),
]

REFUSALS = [
    Case(
        "a coupled field on each side",
        Item,
        BASE,
        (("set", "start", 2),),
        (("set", "end", 8),),
    ),
    Case(
        "one strict field changed differently",
        Item,
        BASE,
        (("set", "title", "title-1"),),
        (("set", "title", "title-2"),),
    ),
    Case(
        "a delete against a change",
        Item,
        BASE,
        (("delete", "owner", None),),
        (("set", "owner", "owner-2"),),
    ),
    Case(
        "a rewrite of an append-only field",
        Item,
        BASE,
        (("append", "log", ("edited", 1)),),
        (("rewrite", "log", ("rewritten", 2)),),
    ),
]


def different_fields(cls, name):
    return Case(
        name,
        cls,
        BASE,
        (("set", "title", "title-1"),),
        (("set", "description", "description-1"),),
    )


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("case", MERGES, ids=lambda case: case.name)
def test_changes_of_different_fields_both_commit(mode, case, tmp_path, monkeypatch):
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy())
        result = race(db, case)
    expected = reference(entry(), result.old, result.committed, result.new, set())
    assert expected is not None
    assert check(result, expected) is None, check(result, expected)


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("case", REFUSALS, ids=lambda case: case.name)
def test_a_coupled_field_or_a_field_changed_on_both_sides_conflicts(
    mode, case, tmp_path, monkeypatch
):
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy())
        result = race(db, case)
        # The same policy merges changes of two other fields.
        control = race(db, different_fields(Item, "control"))
    assert reference(entry(), result.old, result.committed, result.new, set()) is None
    assert check(result, None) is None, check(result, None)
    assert control.outcome == "merged"


@pytest.mark.parametrize("mode", MODES)
def test_a_class_without_a_policy_entry_never_merges(mode, tmp_path, monkeypatch):
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        # Before any policy, nothing merges.
        outcomes = {"no policy": race(db, different_fields(Item, "before")).outcome}
        install(db, policy())
        outcomes["unlisted"] = race(db, different_fields(Unlisted, "unlisted")).outcome
        outcomes["subclass"] = race(db, different_fields(Derived, "subclass")).outcome
        install(db, policy(tier2="off"))
        outcomes["tier2 off"] = race(db, different_fields(Item, "off")).outcome
        # The same case merges with the entry on, so the case can merge.
        install(db, policy())
        merged = race(db, different_fields(Item, "on")).outcome
    assert outcomes == dict.fromkeys(outcomes, "conflict")
    assert merged == "merged"


@pytest.mark.parametrize("mode", MODES)
def test_a_new_version_of_the_policy_applies_at_the_next_conflict(
    mode, tmp_path, monkeypatch
):
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        first = install(db, policy())
        outcomes = [race(db, different_fields(Item, "first")).outcome]
        second = install(db, {"classes": {}})
        outcomes.append(race(db, different_fields(Item, "second")).outcome)
        third = install(db, policy())
        outcomes.append(race(db, different_fields(Item, "third")).outcome)
    assert outcomes == ["merged", "conflict", "merged"]
    # Each write of the policy adds 1 to its version.
    assert (second, third) == (first + 1, first + 2)


def log_lines(path):
    """Return the lines of the resolution log, as dicts."""
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


@pytest.mark.parametrize("mode", MODES)
def test_in_shadow_mode_the_conflict_is_raised_and_the_log_holds_the_merge(
    mode, tmp_path, monkeypatch
):
    import ZODB.utils

    log = tmp_path / "resolutions.log"
    environment = {"ZOE_RESOLUTION_LOG": str(log)}
    with opened(mode, tmp_path / "store", monkeypatch, environment) as db:
        version = install(db, policy(tier2="shadow"))
        case = different_fields(Item, "shadow")
        result = race(db, case)
        with db.transaction() as conn:
            oid = conn.root()[case.name]._p_oid
    assert check(result, None) is None, check(result, None)
    name = f"0x{ZODB.utils.u64(oid):016x}"
    lines = [line for line in log_lines(log) if line.get("oid") == name]
    merges = [line for line in lines if line.get("tier") == 2]
    assert len(merges) == 1, lines
    (line,) = merges
    assert line["class"] == dotted(Item)
    assert line["mode"] == "shadow"
    assert line["outcome"] == "merged"
    assert line["policy"] == version
    assert "description" in line["keys"]


@pytest.mark.parametrize("mode", MODES)
def test_tier_2_gives_the_record_of_the_python_reference(mode, tmp_path, monkeypatch):
    cases = generated_cases()
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy())
        results = [race(db, case) for case in cases]

    used = set()
    outcomes = set()
    problems = []
    for case, result in zip(cases, results, strict=True):
        expected = reference(entry(), result.old, result.committed, result.new, used)
        outcomes.add("conflict" if expected is None else "merged")
        problem = check(result, expected)
        if problem:
            problems.append(
                f"{case.name}: {case.committed!r} then {case.new!r}: {problem}"
            )
    shown = "\n".join(problems[:10])
    assert not problems, f"{len(problems)} of {len(cases)} cases:\n{shown}"
    # The cases give both outcomes, and each strategy decides a field.
    assert outcomes == {"merged", "conflict"}
    assert used == {"lww", "max", "sum", "union", "append"}
