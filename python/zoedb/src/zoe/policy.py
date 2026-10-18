"""The merge policy of a store (ADR 0046).

The policy is the persistent object ``root['zoe.merge_policy']`` of the
database, of the class `MergePolicy`. It is a record like any other: it
commits or aborts with its transaction, it has a history, and the store
reads it again at the next vote with a conflict after a change. It holds
plain values only, so the core reads it in Rust, in-process and in
``zoe serve``:

- ``version``: an int; each `install` adds 1;
- ``structural_merges`` and ``posting_dicts``, optional: the mode of tier 1
  and of the rules of ADR 0042, ``on``, ``shadow`` or ``off``; they win over
  the keys of the same names of the store;
- ``split_merges``, optional: the mode of the split merge of ADR 0048,
  ``on``, ``shadow`` or ``off``. It merges the splits of the nodes of
  BTrees where ZODB's resolver refuses, in the classes where tier 1 runs.
  The store has no key of this name: without the switch, the split merge
  is off;
- ``folder_orders``, optional: the mode of the order merge of ADR 0052,
  ``on``, ``shadow`` or ``off``. It merges two appends to the order of a
  folder of plone.folder and renumbers its positions, in a transaction
  that queues its indexing (queued indexing, ADR 0047). Its companions are
  queued indexing and ``split_merges``. The store has no key of this name:
  without the switch, the order merge is off. While it is not ``off``, a
  policy cannot give ``persistent.list.PersistentList`` a ``tier2`` other
  than ``off``: tier 2 would merge an order without its positions;
- ``classes``: a dict of class names (``module.Class``) to their entries.

The entry of a class has these keys, each optional:

- ``tier1``: the mode of tier 1 for a class of BTrees or ``Length``;
- ``tier2``: the mode of the merges of fields: ``on``, ``shadow`` or
  ``off``, the default; a class without ``tier2`` never merges in tier 2;
- ``default``: the strategy of a field that ``fields`` does not name:
  ``strict``, the default, ``lww``, ``max``, ``sum``, ``union``,
  ``append`` or ``keys``;
- ``fields``: a dict of field names to their strategies;
- ``coupled``: a list of groups of fields, each a list of field names;
- ``blocked``: True for a class that never merges in tier 2, whatever its
  other keys say; `zoe.plone` blocks a class whose invariant it cannot
  read;
- ``options``: a dict of strategy names to their options. Only ``append``
  takes options, and its only option is ``state`` (see below).

The option ``state`` of ``append`` names a key of the items of the field,
such as ``review_state`` in the statuses of a workflow history. When both
sides appended items, the merge goes ahead only if each item that the
committed side appended holds the state of the last item of the old value
under that key; else it refuses. An item without the key, or an old value
without items, gives no answer (ADR 0046, section 12)::

    {"tier2": "on", "default": "append",
     "options": {"append": {"state": "review_state"}}}

The strategy ``keys`` merges a field whose value is a dict key by key: a
key that one side changed, added or deleted takes that change, and two
different changes of one key refuse. The state of a ``PersistentMapping``
is ``{'data': {...}}``, so the entry of a mapping class gives the field
``data`` the strategy ``keys``::

    {"classes": {
        "mysite.registry.Registry": {"tier2": "on", "fields": {"data": "keys"}},
    }}

An entry names one class, not its subclasses. Name a subclass of your own,
for the mappings whose keys do not depend on each other. Do not name
``persistent.mapping.PersistentMapping`` or
``Persistence.mapping.PersistentMapping``: the root of each database, and
many mappings of Zope and CMF whose keys depend on each other, such as the
WebDAV locks of an object, are of those classes (ADR 0046, section 11).

`validate` checks a policy and gives a copy of plain values, `install`
writes a policy in the transaction of a connection, and `read` gives it
back as a dict.
"""

import copy

import persistent

#: The name of the policy in the root of the database.
NAME = "zoe.merge_policy"
#: The modes of a switch.
MODES = ("on", "shadow", "off")
#: The strategies of a field.
STRATEGIES = ("strict", "lww", "max", "sum", "union", "append", "keys")
#: The keys of a policy that `install` takes, besides ``classes``.
SWITCHES = ("structural_merges", "posting_dicts", "split_merges", "folder_orders")
#: The class of the order of a folder, which tier 2 never merges while
#: ``folder_orders`` is not ``off``.
ORDER_CLASS = "persistent.list.PersistentList"
#: The keys of the entry of a class.
ENTRY_KEYS = ("tier1", "tier2", "default", "fields", "coupled", "blocked", "options")
#: The options that each strategy takes.
OPTIONS = {"append": ("state",)}


class MergePolicy(persistent.Persistent):
    """The merge policy of a store. Write it with `install`.

    Its attributes are ``version``, ``classes`` and, when the policy gives
    them, ``structural_merges``, ``posting_dicts``, ``split_merges`` and
    ``folder_orders``.
    """

    def __init__(self):
        self.version = 0
        self.classes = {}


def _text(value, where):
    """Return `value` when it is a str, not a subclass of str: the core
    reads a plain str only."""
    if type(value) is not str:
        raise ValueError(f"{where}: {value!r} is not a str")
    return value


def _canonical(value, allowed, where, words):
    """Return the constant of `allowed` that equals `value`, so the policy
    stores a plain str."""
    if type(value) is not str or value not in allowed:
        raise ValueError(f"{where}: {value!r} is not {words}")
    return allowed[allowed.index(value)]


def _mode(value, where):
    return _canonical(value, MODES, where, "on, shadow or off")


def _strategy(value, where):
    return _canonical(value, STRATEGIES, where, f"one of {', '.join(STRATEGIES)}")


def _options(options, where):
    """Return a copy of the options of an entry, with plain values only."""
    if type(options) is not dict:
        raise ValueError(f"{where}: options is a dict of strategies to options")
    checked = {}
    for strategy, given in options.items():
        if strategy not in OPTIONS:
            raise ValueError(f"{where}: the strategy {strategy!r} takes no options")
        if type(given) is not dict:
            raise ValueError(f"{where}: the options of {strategy} are a dict")
        names = OPTIONS[strategy]
        unknown = set(given) - set(names)
        if unknown:
            raise ValueError(
                f"{where}: {strategy} has no option {sorted(map(str, unknown))[0]!r}"
            )
        values = {}
        for name in names:
            if given.get(name) is not None:
                value = _text(given[name], f"{where}: {strategy} {name}")
                if not value:
                    raise ValueError(f"{where}: {strategy} {name} is an empty str")
                values[name] = value
        if values:
            checked[_strategy(strategy, f"{where}: options")] = values
    return checked


def validate(policy):
    """Return a copy of `policy` with plain values only, or raise
    `ValueError` when it breaks the rules of a policy. `policy` is a dict
    with the key ``classes`` and, optionally, ``structural_merges``,
    ``posting_dicts``, ``split_merges`` and ``folder_orders``. ``version``
    is not part of it: `install` sets it. While ``folder_orders`` is not
    ``off``, an entry of `ORDER_CLASS` with a ``tier2`` other than ``off``
    is an error.

    Each text must be a plain `str`, and the copy holds the constants of
    `MODES` and `STRATEGIES`, so the core reads every value."""
    if type(policy) is not dict:
        raise ValueError("a policy is a dict")
    unknown = set(policy) - {"classes", *SWITCHES}
    if unknown:
        raise ValueError(f"a policy has no key {sorted(map(str, unknown))[0]!r}")
    result = {}
    for switch in SWITCHES:
        if policy.get(switch) is not None:
            result[switch] = _mode(policy[switch], switch)
    classes = policy.get("classes", {})
    if type(classes) is not dict:
        raise ValueError("classes is a dict of class names to entries")
    result["classes"] = {}
    for name, entry in classes.items():
        if type(name) is not str or "." not in name:
            raise ValueError(f"{name!r} is not a class as module.Class")
        if type(entry) is not dict:
            raise ValueError(f"{name}: an entry is a dict")
        unknown = set(entry) - set(ENTRY_KEYS)
        if unknown:
            raise ValueError(
                f"{name}: an entry has no key {sorted(map(str, unknown))[0]!r}"
            )
        checked = {}
        for key in ("tier1", "tier2"):
            if entry.get(key) is not None:
                checked[key] = _mode(entry[key], f"{name}: {key}")
        if entry.get("default") is not None:
            checked["default"] = _strategy(entry["default"], f"{name}: default")
        if entry.get("blocked") is not None:
            if type(entry["blocked"]) is not bool:
                raise ValueError(f"{name}: blocked is True or False")
            if entry["blocked"]:
                checked["blocked"] = True
        if "fields" in entry:
            fields = entry["fields"]
            if type(fields) is not dict:
                raise ValueError(f"{name}: fields is a dict of fields to strategies")
            if fields:
                checked["fields"] = {
                    _text(field, f"{name}: a field"): _strategy(
                        strategy, f"{name}: {field}"
                    )
                    for field, strategy in fields.items()
                }
        if "coupled" in entry:
            coupled = entry["coupled"]
            if type(coupled) not in (list, tuple):
                raise ValueError(f"{name}: coupled is a list of groups of fields")
            groups = []
            for group in coupled:
                if type(group) not in (list, tuple) or not group:
                    raise ValueError(f"{name}: a coupled group is a list of fields")
                groups.append(
                    [_text(field, f"{name}: a coupled field") for field in group]
                )
            if groups:
                checked["coupled"] = groups
        if entry.get("options") is not None:
            options = _options(entry["options"], f"{name}: options")
            if options:
                checked["options"] = options
        result["classes"][name] = checked
    order = result["classes"].get(ORDER_CLASS, {})
    if (
        result.get("folder_orders", "off") != "off"
        and order.get("tier2", "off") != "off"
    ):
        raise ValueError(
            f"{ORDER_CLASS}: no tier2 while folder_orders is not off, "
            "as tier 2 would merge the order of a folder without its positions"
        )
    return result


def install(connection, policy):
    """Write `policy` as the merge policy of the database of `connection`,
    in its current transaction, and return its new version.

    The first call creates the object and its name in the root. Each call
    adds 1 to ``version``. The caller commits the transaction, so the
    policy commits or aborts with it. A switch that `policy` does not give
    leaves the key of the store in force."""
    checked = validate(policy)
    root = connection.root()
    stored = root.get(NAME)
    if stored is None:
        stored = MergePolicy()
        root[NAME] = stored
    elif not isinstance(stored, MergePolicy):
        raise ValueError(f"root[{NAME!r}] is not a MergePolicy")
    stored.version += 1
    stored.classes = checked["classes"]
    for switch in SWITCHES:
        if switch in checked:
            setattr(stored, switch, checked[switch])
        elif switch in stored.__dict__:
            delattr(stored, switch)
    return stored.version


def read(connection):
    """Return the merge policy of the database of `connection` as a dict,
    with its ``version``, or ``None`` when the database has none."""
    stored = connection.root().get(NAME)
    if stored is None:
        return None
    policy = {"version": stored.version, "classes": copy.deepcopy(stored.classes)}
    for switch in SWITCHES:
        value = getattr(stored, switch, None)
        if value is not None:
            policy[switch] = value
    return policy
