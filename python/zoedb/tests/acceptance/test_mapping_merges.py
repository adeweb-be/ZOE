"""Acceptance test of the merge of a mapping key by key: the strategy `keys`.

ZODB gives `PersistentMapping` no `_p_resolveConflict`, so ZODB refuses
every conflict on it, also when two transactions change different keys.
The state of a `PersistentMapping` is `{'data': {...}}`: it has one field,
`data`. The merge policy (ADR 0046) can give the field `data` of a mapping
class the strategy `keys`. Then the vote merges a conflict on a mapping of
that class key by key, in Rust, in tier 2:

- a key that only one side changed, added or deleted takes that change;
- a key that both sides changed to equal values takes it;
- a key that both sides changed to different values refuses, and so does
  a key that one side deleted and the other side changed;
- the merged dict holds the keys of the committed dict, in their order,
  then the keys that only the new side added, in the order of the new
  side.

The test checks, with mapping classes that have no `_p_resolveConflict`,
so that no client resolves a conflict:

- two transactions that change different keys of one mapping both commit,
  and the second connection then sees the merged state; two transactions
  that change one key to equal values both commit too;
- one key changed differently on both sides, or deleted on one side and
  changed on the other, raises `ConflictError`, and the first commit
  stays;
- a mapping class without a policy entry never merges, and neither does a
  subclass of a class with an entry, a class whose entry keeps `data`
  strict, or a class whose entry has `tier2` `off`; before any policy,
  nothing merges;
- in shadow mode (`tier2` `shadow`) the conflict is raised, and the
  resolution log holds the merge: a line of tier 2 with the outcome
  `merged`, the version of the policy, and the key of the mapping that the
  new record changed;
- on generated states, tier 2 gives the outcome and the record, byte for
  byte, of the Python reference of the strategy below.

The mapping classes are `Mapping`, a subclass of
`persistent.mapping.PersistentMapping` in this module, and
`Persistence.mapping.PersistentMapping` of Zope. A test of the class of
Zope skips when the package `Persistence` is not importable. The policy
does not name `persistent.mapping.PersistentMapping`: the root of each
database is of that class.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import contextlib
import dataclasses
import importlib
import io
import json
import os
import random
import subprocess
import threading

import pytest
from persistent.mapping import PersistentMapping

import zoe.testing
from zoe.testing import ZoeServer

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for the server.
TIMEOUT = 30
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The mapping classes that a policy names: Mapping, and the class of Zope.
MAPPINGS = ("subclass", "zope")
# The seed of the generator of the cases, and the number of cases.
SEED = 4210
CASES = 200
# The field of the state of a PersistentMapping that holds its items.
FIELD = "data"
# The kinds of the keys of the generated mappings. All the keys of one
# mapping are of one kind.
KEY_KINDS = ("str", "int", "bytes", "tuple")
# The rules of the strategy. The generated cases use each of them.
RULES = {
    "committed side only",
    "new side only",
    "added on the new side",
    "equal changes",
    "different changes",
    "a delete against a change",
}


class Mapping(PersistentMapping):
    """A mapping of a class that the policy marks mergeable."""


class Unlisted(PersistentMapping):
    """A mapping of a class that the policy does not name."""


class Derived(Mapping):
    """A subclass of Mapping. The policy does not name it."""


class Stamp:
    """Stands for a `DateTime`: an object of another class in a value."""

    def __init__(self, micros):
        self.micros = micros

    def __eq__(self, other):
        return type(other) is Stamp and other.micros == self.micros

    def __repr__(self):
        return f"Stamp({self.micros})"


def zope_mapping():
    """Return the PersistentMapping of Zope. Skip the test without it."""
    try:
        from Persistence.mapping import PersistentMapping as ZopeMapping
    except ImportError as error:
        pytest.skip(f"the package Persistence of Zope is not importable: {error}")
    return ZopeMapping


def mapping_class(name):
    """Return the mapping class of a name of MAPPINGS."""
    return Mapping if name == "subclass" else zope_mapping()


def dotted(cls):
    """Return the name of cls in the class pickle of a record."""
    return f"{cls.__module__}.{cls.__qualname__}"


def keys_entry(tier2="on"):
    """Return the entry of a mapping class that merges it key by key.

    The entry gives the field `data` of the state of the class the
    strategy `keys`, and the mode tier2. ADR 0046 and its change for the
    strategy `keys` fix how an entry gives a field its strategy and how a
    policy marks a mapping class. Only this function holds that form. The
    human approves a change of this function with the ADR.
    """
    return {"tier2": tier2, "default": "strict", "fields": {FIELD: "keys"}}


def policy(cls, tier2="on"):
    """Return a policy that merges the mappings of cls key by key."""
    return {"classes": {dotted(cls): keys_entry(tier2)}}


def install(db, new_policy):
    """Install new_policy in one commit. Return its version."""
    import zoe.policy

    with db.transaction() as conn:
        zoe.policy.install(conn, new_policy)
    with db.transaction() as conn:
        return zoe.policy.read(conn)["version"]


@contextlib.contextmanager
def serving(path, environment):
    """Run `zoe serve` on path with environment. Yield its address."""
    command = [
        ZoeServer.binary(), "serve", "--path", str(path), "--listen", "127.0.0.1:0"
    ]  # fmt: skip
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    environ.update(environment)
    process = subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        env=environ,
        text=True,
    )
    try:
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, process.kill)
        timer.start()
        try:
            for line in process.stdout:
                word, _, value = line.strip().partition(" ")
                if word == "listening":
                    host, port = value.rsplit(":", 1)
                    break
            else:
                raise RuntimeError(f"zoe serve stopped with {process.wait()}")
        finally:
            timer.cancel()
        yield (host.strip("[]"), int(port))
    finally:
        process.kill()
        process.wait()
        process.stdout.close()


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


# The Python reference of tier 2 with the strategy `keys` (ADR 0046,
# section 4, and its change for the strategy). It decodes the three
# records, merges their states, and pickles the result with ZODB's pickler.
# It gives the meaning of the strategy. The human approves a change of it
# with the ADR. A mapping class of this test has one field, `data`, so no
# two fields share an object.

# A key or a field that a state does not hold.
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
    items in the same order. Two objects of another class are equal when
    their states are equal.
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
    if isinstance(a, Stamp):
        return same(vars(a), vars(b))
    return a == b


def orderable(key):
    """Return whether the model compares and orders key: a str, an int, a
    bytes, or a tuple of those."""
    if type(key) in (str, int, bytes):
        return True
    return type(key) is tuple and all(map(orderable, key))


def merge_keys(old, committed, new, used):
    """Return the dict that the strategy `keys` gives, or REFUSED.

    Add the name of each rule of RULES that decided a key to used.
    """
    states = (old, committed, new)
    if not all(type(state) is dict and all(map(orderable, state)) for state in states):
        # The strategy gives no answer. The client has no resolver, so the
        # conflict stays.
        return REFUSED
    values = {}
    for key in [*old, *committed, *new]:
        if key in values:
            continue
        o, c, n = (state.get(key, ABSENT) for state in states)
        if same(o, n):
            values[key] = c
            if not same(o, c):
                used.add("committed side only")
        elif same(o, c):
            values[key] = n
            used.add("added on the new side" if o is ABSENT else "new side only")
        elif same(c, n):
            values[key] = c
            used.add("equal changes")
        elif c is ABSENT or n is ABSENT:
            used.add("a delete against a change")
            return REFUSED
        else:
            used.add("different changes")
            return REFUSED
    # The keys of the committed dict in their order, then the keys that
    # only the new side added, in its order.
    merged = {key: values[key] for key in committed if values[key] is not ABSENT}
    for key in new:
        if key not in committed and values[key] is not ABSENT:
            merged[key] = values[key]
    return merged


def merge(old, committed, new, used):
    """Return the merged state, or None for a conflict.

    The field `data` takes the strategy `keys`. Each other field takes the
    default, `strict`.
    """
    names = list(old)
    names += [field for field in committed if field not in old]
    names += [field for field in new if field not in old and field not in committed]
    values = {}
    for field in names:
        o, c, n = (state.get(field, ABSENT) for state in (old, committed, new))
        if same(o, n):
            values[field] = c
        elif same(o, c):
            values[field] = n
        elif same(c, n):
            values[field] = c
        elif c is ABSENT or n is ABSENT or field != FIELD:
            return None
        else:
            values[field] = merge_keys(o, c, n, used)
            if values[field] is REFUSED:
                return None
    merged = {f: values[f] for f in committed if values[f] is not ABSENT}
    for field in new:
        if field not in committed and values[field] is not ABSENT:
            merged[field] = values[field]
    return merged


def reference(old_record, committed_record, new_record, used):
    """Return the record of the reference merge, or None for a conflict."""
    old_class, old = load(old_record)
    committed_class, committed = load(committed_record)
    cls, new = load(new_record)
    if old_class is not committed_class or committed_class is not cls:
        # A change of class gives no answer.
        return None
    merged = merge(old, committed, new, used)
    return None if merged is None else dump(cls, merged)


# The cases.


@dataclasses.dataclass(frozen=True)
class Case:
    """Two transactions that change one mapping from one snapshot.

    `base` is the first dict of the mapping. `committed` and `new` are the
    changes of the first and the second transaction, each a tuple of
    `(kind, key, value)`. `merged` is the dict of the merge, for a case
    that merges and that the test writes by hand. `kind` is the kind of
    the keys of a generated case.
    """

    name: str
    base: dict
    committed: tuple
    new: tuple
    merged: dict | None = None
    kind: str = "str"


def apply(mapping, changes):
    """Make changes on mapping."""
    for kind, key, value in changes:
        if kind == "set":
            mapping[key] = value
        elif kind == "delete":
            if key in mapping:
                del mapping[key]
        elif kind == "move" and key in mapping:
            # Delete the key and set it again: the key moves to the end.
            mapping[key] = mapping.pop(key)


def word(rng, prefix):
    """Return a new text of 7 characters."""
    return f"{prefix}{rng.randrange(10**6):06d}"


def make_key(kind, number):
    """Return the key of kind for number. Two numbers give two keys."""
    text = f"key-{number:03d}"
    if kind == "str":
        return text
    if kind == "bytes":
        return text.encode()
    if kind == "tuple":
        return (text, number)
    # Ints of several sizes, so that the pickles use each of their int
    # opcodes.
    scale = (1, 300, 70_000, 2**40)[number % 4]
    return (number + 1) * scale * (-1 if number % 3 == 2 else 1)


def value(rng):
    """Return a new value of one of the kinds that a mapping holds."""
    kind = rng.randrange(5)
    if kind == 0:
        return word(rng, "v")
    if kind == 1:
        return rng.randint(-(10**6), 10**6)
    if kind == 2:
        return (word(rng, "t"), rng.randint(0, 9))
    if kind == 3:
        return [word(rng, "l"), word(rng, "l")]
    return Stamp(rng.randrange(10**12))


def common(rng):
    """Return a value of a small set, so that both sides give equal values
    at times."""
    return f"same-{rng.randrange(2)}"


def base_state(rng, kind):
    return {make_key(kind, number): value(rng) for number in range(rng.randint(0, 6))}


def side(rng, kind, base):
    """Return one to three changes of the mapping base."""
    keys = list(base)
    changes = []
    for _ in range(rng.randint(1, 3)):
        what = rng.choice(("set", "set", "common", "add", "add", "delete", "move"))
        if what == "set" and keys:
            changes.append(("set", rng.choice(keys), value(rng)))
        elif what == "common" and keys:
            changes.append(("set", rng.choice(keys), common(rng)))
        elif what == "add":
            # A small set of new keys, so that both sides add one key at
            # times.
            key = make_key(kind, 100 + rng.randrange(3))
            changes.append(("set", key, rng.choice((value, common))(rng)))
        elif what == "delete" and keys:
            changes.append(("delete", rng.choice(keys), None))
        elif what == "move" and keys:
            changes.append(("move", rng.choice(keys), None))
    return tuple(changes)


def effective(base, changes):
    """Return whether changes change the dict base."""
    data = dict(base)
    apply(data, changes)
    return not same(data, base)


def generated_cases():
    """Return the generated cases, the same list at each call."""
    rng = random.Random(SEED)
    cases = []
    while len(cases) < CASES:
        kind = KEY_KINDS[len(cases) % len(KEY_KINDS)]
        base = base_state(rng, kind)
        committed, new = side(rng, kind, base), side(rng, kind, base)
        if effective(base, committed) and effective(base, new):
            name = f"case-{len(cases)}"
            cases.append(Case(name, base, committed, new, kind=kind))
    return cases


def plain(obj):
    return dict(obj.__getstate__())


def record(db, oid):
    return db.storage.loadBefore(oid, MAX_TID)[0]


@dataclasses.dataclass
class Result:
    """The end of one case."""

    outcome: str
    old: bytes
    committed: bytes
    new: bytes
    stored: bytes
    seen: dict


def race(db, cls, case):
    """Run case on a new mapping of cls in the root of db. Return a
    Result."""
    import transaction
    from ZODB.POSException import ConflictError
    from ZODB.serialize import ObjectWriter

    first_tm = transaction.TransactionManager()
    second_tm = transaction.TransactionManager()
    first = db.open(transaction_manager=first_tm)
    second = db.open(transaction_manager=second_tm)
    try:
        first_tm.begin()
        mapping = cls()
        for key, item in case.base.items():
            mapping[key] = item
        first.root()[case.name] = mapping
        first_tm.commit()
        oid = mapping._p_oid
        old = record(db, oid)

        first_tm.begin()
        second_tm.begin()
        mine = first.root()[case.name]
        theirs = second.root()[case.name]
        apply(mine, case.committed)
        apply(theirs, case.new)
        # The record that the second transaction stores.
        new = ObjectWriter(theirs).serialize(theirs)
        first_tm.commit()
        committed = record(db, oid)
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
        stored = record(db, oid)
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


# The time of the Stamp of BASE.
T0 = 1_700_000_000_000_000

BASE = {
    "key-a": "value-a0",
    "key-b": "value-b0",
    "key-c": ("tuple-c0", 1),
    "key-d": Stamp(T0),
}

MERGES = [
    Case(
        "different keys",
        BASE,
        (("set", "key-a", "value-a1"),),
        (("set", "key-b", "value-b1"),),
        merged={
            "key-a": "value-a1",
            "key-b": "value-b1",
            "key-c": ("tuple-c0", 1),
            "key-d": Stamp(T0),
        },
    ),
    Case(
        "one key changed to equal values on both sides",
        BASE,
        (("set", "key-a", "value-a1"), ("set", "key-c", ("tuple-c1", 2))),
        (("set", "key-a", "value-a1"), ("set", "key-b", "value-b1")),
        merged={
            "key-a": "value-a1",
            "key-b": "value-b1",
            "key-c": ("tuple-c1", 2),
            "key-d": Stamp(T0),
        },
    ),
    Case(
        "values of other kinds",
        BASE,
        (("set", "key-c", ["list-c1", "list-c2"]),),
        (("set", "key-d", Stamp(T0 + 1)),),
        merged={
            "key-a": "value-a0",
            "key-b": "value-b0",
            "key-c": ["list-c1", "list-c2"],
            "key-d": Stamp(T0 + 1),
        },
    ),
    Case(
        "keys added on each side",
        BASE,
        (("set", "key-e", "value-e1"),),
        (("set", "key-g", "value-g2"), ("set", "key-f", "value-f2")),
        merged={
            "key-a": "value-a0",
            "key-b": "value-b0",
            "key-c": ("tuple-c0", 1),
            "key-d": Stamp(T0),
            "key-e": "value-e1",
            "key-g": "value-g2",
            "key-f": "value-f2",
        },
    ),
    Case(
        "a key deleted on one side and another changed on the other",
        BASE,
        (("delete", "key-c", None),),
        (("set", "key-a", "value-a2"),),
        merged={"key-a": "value-a2", "key-b": "value-b0", "key-d": Stamp(T0)},
    ),
    Case(
        "a key changed on one side and another deleted on the other",
        BASE,
        (("set", "key-a", "value-a1"),),
        (("delete", "key-c", None),),
        merged={"key-a": "value-a1", "key-b": "value-b0", "key-d": Stamp(T0)},
    ),
    Case(
        "one key added on both sides with equal values",
        BASE,
        (("set", "key-e", "value-e1"), ("set", "key-a", "value-a1")),
        (("set", "key-e", "value-e1"), ("set", "key-b", "value-b2")),
        merged={
            "key-a": "value-a1",
            "key-b": "value-b2",
            "key-c": ("tuple-c0", 1),
            "key-d": Stamp(T0),
            "key-e": "value-e1",
        },
    ),
    Case(
        "one key deleted on both sides",
        BASE,
        (("delete", "key-d", None), ("set", "key-a", "value-a1")),
        (("delete", "key-d", None), ("set", "key-b", "value-b2")),
        merged={
            "key-a": "value-a1",
            "key-b": "value-b2",
            "key-c": ("tuple-c0", 1),
        },
    ),
    Case(
        "the order of the committed side",
        BASE,
        (("delete", "key-a", None), ("set", "key-a", "value-a1")),
        (("set", "key-b", "value-b2"),),
        merged={
            "key-b": "value-b2",
            "key-c": ("tuple-c0", 1),
            "key-d": Stamp(T0),
            "key-a": "value-a1",
        },
    ),
]

REFUSALS = [
    Case(
        "one key changed differently on both sides",
        BASE,
        (("set", "key-a", "value-a1"),),
        (("set", "key-a", "value-a2"),),
    ),
    Case(
        "a key deleted on one side and changed on the other",
        BASE,
        (("delete", "key-a", None),),
        (("set", "key-a", "value-a2"),),
    ),
    Case(
        "a key changed on one side and deleted on the other",
        BASE,
        (("set", "key-a", "value-a1"),),
        (("delete", "key-a", None),),
    ),
    Case(
        "one key added on both sides with different values",
        BASE,
        (("set", "key-e", "value-e1"),),
        (("set", "key-e", "value-e2"),),
    ),
    Case(
        "one key changed differently beside keys that merge",
        BASE,
        (("set", "key-a", "value-a1"), ("set", "key-b", "value-b1")),
        (("set", "key-b", "value-b2"), ("set", "key-c", ("tuple-c2", 3))),
    ),
]


def renamed(case, name):
    """Return case on a new mapping of the root, under name."""
    return dataclasses.replace(case, name=name)


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("mapping", MAPPINGS)
@pytest.mark.parametrize("case", MERGES, ids=lambda case: case.name)
def test_changes_of_different_keys_both_commit(
    mode, mapping, case, tmp_path, monkeypatch
):
    cls = mapping_class(mapping)
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy(cls))
        result = race(db, cls, case)
    expected = reference(result.old, result.committed, result.new, set())
    assert expected is not None
    # The reference gives the dict that the case names, in its order.
    assert same(load(expected)[1][FIELD], case.merged), load(expected)[1]
    assert check(result, expected) is None, check(result, expected)


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("mapping", MAPPINGS)
@pytest.mark.parametrize("case", REFUSALS, ids=lambda case: case.name)
def test_a_key_changed_on_both_sides_conflicts(
    mode, mapping, case, tmp_path, monkeypatch
):
    cls = mapping_class(mapping)
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy(cls))
        result = race(db, cls, case)
        # The same policy merges changes of two other keys.
        control = race(db, cls, renamed(MERGES[0], "control"))
    assert reference(result.old, result.committed, result.new, set()) is None
    assert check(result, None) is None, check(result, None)
    assert control.outcome == "merged"


@pytest.mark.parametrize("mode", MODES)
def test_a_mapping_class_without_a_policy_entry_never_merges(
    mode, tmp_path, monkeypatch
):
    different = MERGES[0]
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        # Before any policy, nothing merges.
        outcomes = {"no policy": race(db, Mapping, renamed(different, "none")).outcome}
        install(db, policy(Mapping))
        for name, cls in (
            ("stock", PersistentMapping),
            ("unlisted", Unlisted),
            ("subclass", Derived),
        ):
            outcomes[name] = race(db, cls, renamed(different, name)).outcome
        # An entry that keeps the field data strict.
        install(db, {"classes": {dotted(Mapping): {"tier2": "on"}}})
        outcomes["strict"] = race(db, Mapping, renamed(different, "strict")).outcome
        install(db, policy(Mapping, tier2="off"))
        outcomes["tier2 off"] = race(db, Mapping, renamed(different, "off")).outcome
        # The same case merges with the entry on, so the case can merge.
        install(db, policy(Mapping))
        merged = race(db, Mapping, renamed(different, "on")).outcome
    assert outcomes == dict.fromkeys(outcomes, "conflict")
    assert merged == "merged"


def log_lines(path):
    """Return the lines of the resolution log, as dicts."""
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


def lists(line, key):
    """Return whether a value of the log line holds the text of key.

    The line of a merge of a mapping lists the keys of the mapping that
    the new record changed. The ADR names the field of the line that holds
    them, so the test looks in each value of the line.
    """
    return any(key in json.dumps(value) for value in line.values())


@pytest.mark.parametrize("mode", MODES)
def test_in_shadow_mode_the_conflict_is_raised_and_the_log_holds_the_merge(
    mode, tmp_path, monkeypatch
):
    import ZODB.utils

    log = tmp_path / "resolutions.log"
    environment = {"ZOE_RESOLUTION_LOG": str(log)}
    # The new side of this case changes the key key-b only.
    case = renamed(MERGES[0], "shadow")
    with opened(mode, tmp_path / "store", monkeypatch, environment) as db:
        version = install(db, policy(Mapping, tier2="shadow"))
        result = race(db, Mapping, case)
        with db.transaction() as conn:
            oid = conn.root()[case.name]._p_oid
    assert check(result, None) is None, check(result, None)
    name = f"0x{ZODB.utils.u64(oid):016x}"
    lines = [line for line in log_lines(log) if line.get("oid") == name]
    merges = [line for line in lines if line.get("tier") == 2]
    assert len(merges) == 1, lines
    (line,) = merges
    assert line["class"] == dotted(Mapping)
    assert line["mode"] == "shadow"
    assert line["outcome"] == "merged"
    assert line["policy"] == version
    assert lists(line, "key-b"), line


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("mapping", MAPPINGS)
def test_tier_2_gives_the_record_of_the_python_reference(
    mode, mapping, tmp_path, monkeypatch
):
    cls = mapping_class(mapping)
    cases = generated_cases()
    with opened(mode, tmp_path / "store", monkeypatch) as db:
        install(db, policy(cls))
        results = [race(db, cls, case) for case in cases]

    used = set()
    outcomes = set()
    merged_kinds = set()
    merged_changes = set()
    problems = []
    for case, result in zip(cases, results, strict=True):
        expected = reference(result.old, result.committed, result.new, used)
        if expected is None:
            outcomes.add("conflict")
        else:
            outcomes.add("merged")
            merged_kinds.add(case.kind)
            merged_changes.update(change[0] for change in case.committed + case.new)
        problem = check(result, expected)
        if problem:
            problems.append(
                f"{case.name}: {case.base!r}: {case.committed!r} then "
                f"{case.new!r}: {problem}"
            )
    shown = "\n".join(problems[:10])
    assert not problems, f"{len(problems)} of {len(cases)} cases:\n{shown}"
    # The cases give both outcomes, each rule decides a key, each kind of
    # key merges, and each kind of change is in a merge.
    assert outcomes == {"merged", "conflict"}
    assert used == RULES
    assert merged_kinds == set(KEY_KINDS)
    assert merged_changes == {"set", "delete", "move"}
