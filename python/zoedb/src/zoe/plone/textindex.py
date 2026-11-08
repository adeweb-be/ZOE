"""`ZoeTextIndex`: a ZCatalog text index whose texts are text deltas of the
store, answered by the native text index of the store (ADR 0060).

The index keeps no word and no posting. `index_object` stages the text of
each indexed attribute, joined as ZCTextIndex joins them, as a text delta
of the key (catalog, index, rid). `unindex_object` stages an unindex and
`clear()` a clear delta. An attribute that gives no text stages nothing,
so the entry stays, as in ZCTextIndex. The deltas of a transaction wait in
a data manager of the transaction until the commit, one for each key, with
savepoints. The commit stages them in the storage transaction of the
Connection with `text_set`, `text_unindex` and `text_clear`, with the
snapshot of the Connection as `before`. A clear drops the texts of its
index that the transaction staged before it.

The settings: `indexed_attrs`, `mode` (`enhanced` or `compat`),
`language` (the languages of the fields, the default first) and `boosts`
(the indexes that a query on the index also reads, with their weights).
The index writes the settings of each `ZoeTextIndex` of a catalog in the
durable namespace `zoe.text`, under the 8 bytes of the oid of the catalog,
in the transaction that adds the index, removes it, or calls
`setSettings`. The value is JSON: `{"indexes": {id: {"indexed_attrs",
"mode", "language", "boosts", "fuzzy"}}}`. The write keeps the `fuzzy` of
the value that it replaces, else 0.

The query translator. `translate(lexicon, query)` parses a query with the
`QueryParser` and the pipeline of the lexicon of ZCTextIndex, so the syntax
and the errors stay those of ZCTextIndex. It gives the parse tree as the
nodes of the text clause: `w` holds the words after the lexicon, with
`process_post_glob` when an element has one; `p` holds the pattern of a
glob as the user wrote it: the word that the splitter of the lexicon finds
in the term, before the other elements of the pipeline. A glob that starts
with `*` or `?` raises `QueryError`, as ZCTextIndex raises it.

The search. In `Catalog.search`, the backend `catalog` sends the text
clause in the plan of `catalog_query`. On the Python path, `query_index`
sends `text_query` with the result set of the other indexes as its filter,
sorted, and makes an `IIBucket` of the reply. A filter that passes the cap
of a call is not sent: the index intersects the reply itself. The clause
holds the staged texts of the indexes that the query reads (`st`) and the
indexes that the transaction cleared (`cl`). When the catalog has an index
`Language`, the clause holds the fields of the languages of the `Language`
clause of the query (`lg`, see `_languages_of`). Staged texts that pass the
cap of a call raise `zoe.storage.Unavailable`. `numObjects` gives the
distinct words of the index, `indexSize` its documents, an empty text
included, and `getEntryForObject` the
words of the live text of a rid, split with the pipeline of the lexicon:
all three read `text_info` and do not count the staged texts.

The requirements. The index needs the backend `catalog` of `zoe.plone` in
the process and a storage with the extension `text`: a `zoe.Storage`, a
`zoe.zeo.ClientStorage` in its native mode with a server that accepted
`text`, or a test storage over one. Elsewhere each method that writes or
searches raises `ZODB.POSException.Unsupported`, so the transaction fails.

A test storage, a `DemoStorage` whose chain of bases ends at a ZOE storage:
the deltas of a commit go to memory that belongs to the `DemoStorage`, as
the namespace writes of `zoe.kv` do. The `DemoStorage` keeps the newest
delta of each key and the clear deltas of each index; it checks nothing. A
query goes to the base at its last tid. The clause holds the texts of each
`DemoStorage` of the stack, then the staged texts of the transaction, and
the settings of the index (`set`). It lists an index in `cl` when the base
holds no record of the catalog, when the base has no text index for the
catalog, and when a `DemoStorage` of the stack holds a clear delta of the
index. On a test storage, `getEntryForObject` and `indexSize` read the
texts of the stack first, then the base; `numObjects` counts the words of
the base, or, when the base has no text index for the catalog, the
distinct words of the texts of the stack, split with the lexicon.

The profile steps. `zoe.plone:text` replaces the ZCTextIndex indexes
`SearchableText`, `Title` and `Description` of `portal_catalog` with
`ZoeTextIndex` indexes of the same ids, in the enhanced mode, with the
languages of the site and, for `SearchableText`, the boosts `Title^3`,
`Description^1.5` and `SearchableText^1`. The new indexes are pending
first, under the key `PENDING` and their id, and no query reads them: the
step indexes the catalog into them in batches of `BATCH_SIZE` objects, one
commit each, which writes the deltas, while the old indexes serve. The last
batch puts them in place, in the commit of the caller. An import after a
failure halfway finishes the work. It refuses with `Unsupported`, and
changes nothing, an instance without the backend `catalog`, a storage
without the extension `text`, and the mode `compat`.
`zoe.plone:text-uninstall` fills pending ZCTextIndex indexes with the same
attributes and lexicons in the same way, then stages a clear delta for
each `ZoeTextIndex` and puts the ZCTextIndex indexes in place.
"""

import json
import math
import threading
import weakref

import transaction
import transaction.interfaces
import zope.interface
from AccessControl.class_init import InitializeClass
from AccessControl.Permissions import manage_zcatalog_indexes
from AccessControl.SecurityInfo import ClassSecurityInfo
from Acquisition import Implicit, aq_base, aq_chain, aq_inner, aq_parent
from BTrees.IIBTree import IIBucket
from OFS.SimpleItem import SimpleItem
from Persistence import Persistent
from Products.PluginIndexes.interfaces import IPluggableIndex, IQueryIndex
from Products.PluginIndexes.util import safe_callable
from Products.ZCatalog.query import IndexQuery
from Products.ZCTextIndex import ParseTree
from Products.ZCTextIndex.QueryParser import QueryParser
from ZODB.Connection import TransactionMetaData
from ZODB.DemoStorage import DemoStorage
from ZODB.POSException import POSKeyError, Unsupported
from ZODB.utils import p64, u64

import zoe.kv

__all__ = [
    "BATCH_SIZE",
    "BOOSTS",
    "MODES",
    "NAMESPACE",
    "PENDING",
    "TEXT_INDEXES",
    "ZoeTextIndex",
    "install_text",
    "settings_changed",
    "translate",
    "uninstall_text",
]

#: The durable namespace of the settings.
NAMESPACE = "zoe.text"

#: The modes of a `ZoeTextIndex`.
MODES = ("enhanced", "compat")

#: The ZCTextIndex indexes that the profile step replaces.
TEXT_INDEXES = ("SearchableText", "Title", "Description")

#: The boosts of `SearchableText` that the profile step gives.
BOOSTS = {"Title": 3.0, "Description": 1.5, "SearchableText": 1.0}

#: The objects that each commit of a profile step indexes.
BATCH_SIZE = 1000

#: The prefix of the key of a pending index in a catalog: the index that a
#: profile step fills while the old index serves.
PENDING = "zoe-text-pending-"

# The cap of a call (ADR 0059, section 8): msgpack values and bytes.
_CAP_VALUES = 50_000
_CAP_BYTES = 16 * 1024 * 1024

# The longest language of a delta, in bytes of ASCII.
_LANGUAGE = 35

# The smallest and the largest rid of a delta.
_RID = (-(2**31), 2**31 - 1)


# The translator.


class _Parser(QueryParser):
    """The `QueryParser` of ZCTextIndex. It also keeps the term of each glob
    node, so the translator gives the pattern as the user wrote it."""

    def __init__(self, lexicon):
        super().__init__(lexicon)
        self.terms = {}

    def _parseAtom(self):
        term = self._tokens[self._index]
        tree = super()._parseAtom()
        node = tree.getValue() if isinstance(tree, ParseTree.NotNode) else tree
        if isinstance(node, ParseTree.GlobNode):
            self.terms[id(node)] = term
        return tree


def _pipeline(lexicon):
    """The elements of the pipeline of `lexicon`."""
    return getattr(aq_base(lexicon), "_pipeline", ())


def _words(lexicon, value):
    """The words of `value`, a word or a list of words of the parse tree,
    after the pipeline of the lexicon, as `termToWordIds` gives them."""
    last = [value] if isinstance(value, str) else list(value)
    for element in _pipeline(lexicon):
        process = getattr(element, "process_post_glob", element.process)
        last = process(last)
    return [w if isinstance(w, str) else w.decode("utf-8", "replace") for w in last]


def _written(lexicon, term, pattern):
    """The pattern of a glob as the user wrote it: the glob word that the
    splitter of the lexicon finds in `term`, the term of the query without
    the hyphen of a NOT and the quotes. Without one glob word there, the
    pattern after the lexicon, `pattern`."""
    text = term[1:] if term.startswith("-") else term
    if len(text) >= 2 and text[0] == '"' and text[-1] == '"':
        text = text[1:-1]
    elements = _pipeline(lexicon)
    splitter = elements[0] if elements else None
    process = getattr(splitter, "processGlob", None)
    if process is not None:
        words = [
            w.decode("utf-8", "replace") if isinstance(w, bytes) else w
            for w in process([text])
        ]
        found = [w for w in words if isinstance(w, str) and ("*" in w or "?" in w)]
        if len(found) == 1:
            return found[0]
    return pattern


def translate(lexicon, query):
    """Return the parse tree of the str `query` as the node of a text clause
    (ADR 0060, section 12). `lexicon` is the lexicon of ZCTextIndex whose
    pipeline gives the words. A query that the parser of ZCTextIndex refuses
    raises its `ParseError`; a glob that starts with `*` or `?` raises
    `QueryError`."""
    parser = _Parser(lexicon)
    tree = parser.parseQuery(query)
    return _node(tree, lexicon, parser.terms)


def _node(tree, lexicon, terms):
    kind = tree.nodeType()
    if kind == "AND":
        items, nots = [], []
        for sub in tree.getValue():
            if sub.nodeType() == "NOT":
                nots.append(_node(sub.getValue(), lexicon, terms))
            else:
                items.append(_node(sub, lexicon, terms))
        return {"n": "and", "items": items, "nots": nots}
    if kind == "OR":
        return {"n": "or", "items": [_node(s, lexicon, terms) for s in tree.getValue()]}
    if kind == "ATOM":
        return {"n": "atom", "w": _words(lexicon, tree.getValue())}
    if kind == "PHRASE":
        return {"n": "phrase", "w": _words(lexicon, tree.getValue())}
    if kind == "GLOB":
        pattern = tree.getValue()
        if pattern[:1] in ("*", "?"):
            raise ParseTree.QueryError(
                f"pattern {pattern!r} shouldn't start with glob character"
            )
        term = terms.get(id(tree), pattern)
        return {"n": "glob", "p": _written(lexicon, term, pattern)}
    # A NOT node outside an AND, as ZCTextIndex refuses it.
    raise ParseTree.QueryError("NOT parse tree node cannot be executed directly")


# The storages.


def _chain(storage):
    """The `DemoStorage`s of a test storage, the top first, and its base;
    or None when `storage` is not a `DemoStorage`."""
    if not isinstance(storage, DemoStorage):
        return None
    chain = []
    while isinstance(storage, DemoStorage):
        chain.append(storage)
        storage = storage.base
    return chain, storage


def _serves(storage):
    """Tell if `storage` has the extension `text`."""
    supported = getattr(storage, "text_supported", None)
    try:
        return supported is not None and bool(supported())
    except Exception:
        return False


def _check(storage):
    """Raise `Unsupported` when the process has no backend `catalog`, or
    when `storage`, or the base of a test storage, has no extension
    `text`."""
    from zoe.plone import catalog as backend

    if not backend.installed():
        raise Unsupported(
            "ZoeTextIndex needs the backend catalog of zoe.plone in each instance"
        )
    found = _chain(storage)
    base = found[1] if found is not None else storage
    if not _serves(base):
        raise Unsupported(
            f"{type(storage).__name__} has no extension text: ZoeTextIndex needs"
            " zoe.Storage, or zoe.zeo.ClientStorage in its native mode"
        )


def _snapshot(jar):
    """The exclusive tid of the snapshot of the Connection `jar`, or None."""
    from zoe.plone.catalog import _start_of

    start = _start_of(jar)
    if start is None:
        # A historical Connection.
        storage = getattr(jar, "_storage", None)
        start = getattr(storage, "_before", None)
    return start


def _next(tid):
    """The tid after `tid`, as 8 bytes."""
    return p64(u64(tid) + 1)


def _check_delta(index, rid, language):
    """Raise `ValueError` for an index id, a rid or a language that the
    store refuses."""
    if not 1 <= len(index.encode("utf-8")) <= 255:
        raise ValueError(f"the index id {index!r} does not have 1 to 255 bytes")
    if (
        isinstance(rid, bool)
        or not isinstance(rid, int)
        or not _RID[0] <= rid <= _RID[1]
    ):
        raise ValueError(f"the rid {rid!r} is not a signed 32-bit int")
    if len(language) > _LANGUAGE or not language.isascii():
        raise ValueError(f"the language {language!r} is not at most 35 bytes of ASCII")


# The deltas of a transaction.

# The data manager of the deltas of the current transaction of each
# Connection.
_managers = weakref.WeakKeyDictionary()


def _deltas(jar, create):
    """The deltas of the current transaction of the Connection `jar`. With
    `create`, make them and join the transaction when there are none; else
    return None."""
    txn = jar.transaction_manager.get()
    found = _managers.get(jar)
    if found is not None and found.transaction is txn:
        return found
    if not create:
        return None
    deltas = _Deltas(jar, txn)
    _managers[jar] = deltas
    txn.join(deltas)
    return deltas


@zope.interface.implementer(transaction.interfaces.IDataManager)
class _Deltas:
    """The text deltas of one transaction of one Connection: a data manager
    of the transaction."""

    def __init__(self, jar, txn):
        self.transaction_manager = jar.transaction_manager
        self.transaction = txn
        self._jar = jar
        # (catalog, index, rid) -> (language, text or None for an unindex)
        self.texts = {}
        # (catalog, index) of each clear delta.
        self.clears = set()
        # The storage transaction that this data manager began, or None.
        self._meta = None

    def set(self, catalog, index, rid, language, text):
        _check_delta(index, rid, language)
        self.texts[(catalog, index, rid)] = (language, text)

    def clear(self, catalog, index):
        _check_delta(index, 0, "")
        for key in [k for k in self.texts if k[:2] == (catalog, index)]:
            del self.texts[key]
        self.clears.add((catalog, index))

    def _end(self):
        if _managers.get(self._jar) is self:
            del _managers[self._jar]

    def _storage(self):
        return self._jar.db().storage

    # IDataManager

    def abort(self, txn):
        self.texts.clear()
        self.clears.clear()
        self._end()

    def tpc_begin(self, txn):
        pass

    def commit(self, txn):
        if not self.texts and not self.clears:
            return
        storage = self._storage()
        if isinstance(storage, DemoStorage):
            # The overlay takes the deltas at the finish.
            return
        # The storage transaction of the Connection, else the one that
        # `zoe.kv` began for this transaction, else one of its own. Both
        # data managers sort after the Connection, and `zoe.kv` before this
        # one.
        try:
            meta = txn.data(self._jar)
        except KeyError:
            meta = None
        if meta is None:
            writes = zoe.kv._managers.get(self._jar)
            if writes is not None and writes.transaction is txn:
                meta = writes._meta
        if meta is None:
            meta = TransactionMetaData(txn.user, txn.description, txn.extension)
            storage.tpc_begin(meta)
            self._meta = meta
        before = _snapshot(self._jar)
        if before is None:
            before = _next(storage.lastTransaction())
        for catalog, index in sorted(self.clears):
            storage.text_clear(catalog, index, meta)
        for (catalog, index, rid), (language, text) in sorted(self.texts.items()):
            if text is None:
                storage.text_unindex(catalog, index, rid, before, meta)
            else:
                storage.text_set(catalog, index, rid, language, text, before, meta)

    def tpc_vote(self, txn):
        if self._meta is not None:
            self._storage().tpc_vote(self._meta)

    def tpc_finish(self, txn):
        try:
            storage = self._storage()
            if isinstance(storage, DemoStorage):
                _apply(storage, self.clears, self.texts)
            if self._meta is not None:
                storage.tpc_finish(self._meta)
        finally:
            self._meta = None
            self._end()

    def tpc_abort(self, txn):
        try:
            if self._meta is not None:
                self._storage().tpc_abort(self._meta)
        finally:
            self._meta = None
            self.texts.clear()
            self.clears.clear()
            self._end()

    def sortKey(self):
        # After the Connection, whose key is the key of its storage, and
        # after the data manager of `zoe.kv`.
        return f"{self._storage().sortKey()}:zoe.text:{id(self)}"

    def savepoint(self):
        return _Savepoint(self, dict(self.texts), set(self.clears))


@zope.interface.implementer(transaction.interfaces.IDataManagerSavepoint)
class _Savepoint:
    """A savepoint of the deltas: its rollback gives them back."""

    def __init__(self, deltas, texts, clears):
        self._deltas = deltas
        self._texts = texts
        self._clears = clears

    def rollback(self):
        self._deltas.texts = dict(self._texts)
        self._deltas.clears = set(self._clears)


# The deltas of each `DemoStorage`, while the `DemoStorage` lives. The lock
# guards them.
_overlays = weakref.WeakKeyDictionary()
_overlays_lock = threading.Lock()


class _Overlay:
    """The deltas that the commits on one `DemoStorage` made."""

    def __init__(self):
        # (catalog, index, rid) -> (language, text or None)
        self.texts = {}
        # (catalog, index) of each clear delta.
        self.clears = set()


def _apply(demo, clears, texts):
    """Put the deltas of a commit in the overlay of the `DemoStorage` `demo`.
    A clear drops the texts of its index in this `DemoStorage`."""
    with _overlays_lock:
        overlay = _overlays.get(demo)
        if overlay is None:
            overlay = _overlays[demo] = _Overlay()
        for catalog, index in clears:
            for key in [k for k in overlay.texts if k[:2] == (catalog, index)]:
                del overlay.texts[key]
            overlay.clears.add((catalog, index))
        overlay.texts.update(texts)


def _stack(chain, catalog, indexes):
    """The texts of the stack `chain` of `DemoStorage`s for `catalog` and
    the indexes `indexes`, and the indexes that a `DemoStorage` of the stack
    cleared: `({(index, rid): (language, text or None)}, {index})`. A
    `DemoStorage` above replaces the deltas below it."""
    texts, cleared = {}, set()
    with _overlays_lock:
        for demo in reversed(chain):
            overlay = _overlays.get(demo)
            if overlay is None:
                continue
            for c, index in overlay.clears:
                if c == catalog and index in indexes:
                    cleared.add(index)
                    for key in [k for k in texts if k[0] == index]:
                        del texts[key]
            for (c, index, rid), value in overlay.texts.items():
                if c == catalog and index in indexes:
                    texts[(index, rid)] = value
    return texts, cleared


# The settings.


def _attrs(value):
    """The indexed attributes of `value`: a str of names separated by
    commas, or a sequence of names."""
    if value is None:
        return []
    if isinstance(value, str):
        value = value.split(",")
    return [a.strip() for a in value if a and a.strip()]


def _languages(value):
    """The languages of `value`: a str of tags separated by commas, or a
    sequence of tags. A tag that the store refuses raises `ValueError`."""
    if value is None:
        return []
    if isinstance(value, str):
        value = value.split(",")
    out = []
    for tag in value:
        tag = tag.strip()
        if len(tag) > _LANGUAGE or not tag.isascii():
            raise ValueError(f"the language {tag!r} is not at most 35 bytes of ASCII")
        if tag and tag not in out:
            out.append(tag)
    return out


def _boosts(value):
    """The boosts of `value`: a mapping of index ids to weights, or a str of
    `id:weight` items separated by commas. A weight that is not a finite
    number above 0 raises `ValueError`."""
    if value is None:
        return {}
    if isinstance(value, str):
        items = []
        for item in value.split(","):
            if not item.strip():
                continue
            name, _, weight = item.partition(":")
            items.append((name.strip(), weight.strip()))
    else:
        items = list(dict(value).items())
    out = {}
    for name, weight in items:
        try:
            weight = float(weight)
        except (TypeError, ValueError):
            raise ValueError(f"the boost of {name!r} is not a number") from None
        if not name or not math.isfinite(weight) or not 0 < weight < 1e6:
            raise ValueError(
                f"the boost {name!r}: {weight} is not a finite weight above 0"
            )
        out[str(name)] = weight
    return out


def _mode(value):
    mode = (value or "enhanced").strip()
    if mode not in MODES:
        raise ValueError(f"the mode {mode!r} is not one of {', '.join(MODES)}")
    return mode


def _catalog_of(index):
    """The `Catalog` object of the catalog that holds the wrapped `index`,
    or None."""
    from Products.ZCatalog.Catalog import Catalog

    for obj in aq_chain(index)[1:]:
        base = aq_base(obj)
        if isinstance(base, Catalog):
            return base
        inner = aq_base(getattr(base, "_catalog", None))
        if isinstance(inner, Catalog):
            return inner
    return None


def _text_indexes(catalog, pending=True):
    """The `ZoeTextIndex` indexes of the `Catalog` object `catalog`, by the
    id of each index. Without `pending`, leave out the pending indexes of a
    profile step."""
    return {
        aq_base(index).id: aq_base(index)
        for index in catalog.indexes.values()
        if isinstance(aq_base(index), ZoeTextIndex)
        and (pending or not aq_base(index)._is_pending())
    }


# The catalogs whose settings the current transaction writes before its
# commit, by transaction.
_pending = weakref.WeakKeyDictionary()


def settings_changed(catalog):
    """Write the settings of the `ZoeTextIndex` indexes of the `Catalog`
    object `catalog` in `zoe.text`, before the commit of the current
    transaction of the Connection of the catalog. A catalog without a
    database gets no write."""
    catalog = aq_base(catalog)
    jar = catalog._p_jar
    txn = jar.transaction_manager.get() if jar is not None else transaction.get()
    pending = _pending.setdefault(txn, {})
    if id(catalog) in pending:
        return
    pending[id(catalog)] = catalog
    txn.addBeforeCommitHook(_write_settings, (catalog,))


def _write_settings(catalog):
    jar = catalog._p_jar
    if jar is None or not zoe.kv._supported(jar):
        return
    if catalog._p_oid is None:
        jar.add(catalog)
    space = zoe.kv.Namespace(jar, "durable", NAMESPACE)
    key = catalog._p_oid
    old = space.get(key)
    fuzzy = {}
    if old is not None:
        try:
            for name, item in json.loads(old).get("indexes", {}).items():
                fuzzy[name] = item.get("fuzzy", 0)
        except (ValueError, AttributeError):
            pass
    indexes = {}
    for name, index in _text_indexes(catalog).items():
        item = index._settings()
        item["indexed_attrs"] = list(index._indexed_attrs)
        item["fuzzy"] = fuzzy.get(name, 0)
        indexes[name] = item
    if not indexes:
        if old is not None:
            space.delete(key)
        return
    value = json.dumps({"indexes": indexes}, sort_keys=True).encode("utf-8")
    if value != old:
        space.set(key, value)


def _measure(value):
    """The msgpack values and a bound of the bytes of `value`."""
    from zoe.plone.catalog import _size

    return _size(value)


def _language_of(obj):
    """The language of `obj` for its delta: its `Language`, when it is at
    most 35 bytes of ASCII, else the empty str."""
    language = getattr(obj, "Language", None)
    if safe_callable(language):
        try:
            language = language()
        except Exception:
            language = None
    if not isinstance(language, str):
        return ""
    if len(language) > _LANGUAGE or not language.isascii():
        return ""
    return language


@zope.interface.implementer(IPluggableIndex, IQueryIndex)
class ZoeTextIndex(Persistent, Implicit, SimpleItem):
    """A text index of ZCatalog whose texts are text deltas of the store.

    `ZoeTextIndex(id, extra=None, caller=None)`, as `ZCatalog.addIndex`
    makes it, reads from `extra`: `indexed_attrs` or `doc_attr`, the
    attributes, separated by commas (the id by default); `lexicon_id`, the
    lexicon of ZCTextIndex in `caller` whose pipeline parses the queries;
    `mode`, `language` (tags separated by commas) and `boosts` (`id:weight`
    items separated by commas). A missing lexicon raises `LookupError`.
    """

    meta_type = "ZoeTextIndex"
    zmi_icon = "fas fa-list"
    operators = ("and", "or")
    useOperator = "or"
    query_options = ("query",)

    manage_options = ()

    security = ClassSecurityInfo()
    security.declareObjectProtected(manage_zcatalog_indexes)

    def __init__(self, id, extra=None, caller=None):
        self.id = id
        attrs = getattr(extra, "indexed_attrs", None) or getattr(
            extra, "doc_attr", None
        )
        self._indexed_attrs = _attrs(attrs) or [id]
        lexicon_id = getattr(extra, "lexicon_id", "") or ""
        lexicon = getattr(caller, lexicon_id, None) if lexicon_id else None
        if lexicon is None:
            raise LookupError(f"Lexicon {lexicon_id!r} not found")
        self.lexicon_id = lexicon.getId()
        self.mode = _mode(getattr(extra, "mode", None))
        self.language = _languages(getattr(extra, "language", None))
        self.boosts = _boosts(getattr(extra, "boosts", None))

    # The settings.

    def _settings(self):
        return {
            "mode": self.mode,
            "language": list(self.language),
            "boosts": dict(self.boosts),
        }

    def getSettings(self):
        """Return the settings: a dict of `indexed_attrs`, `mode`, `language`
        and `boosts`."""
        out = self._settings()
        out["indexed_attrs"] = list(self._indexed_attrs)
        return out

    def setSettings(self, indexed_attrs=None, mode=None, language=None, boosts=None):
        """Change the settings that are not None, and write them in
        `zoe.text` before the commit. A change of `mode` or `language` makes
        the store rebuild the text index of the catalog; a change of
        `indexed_attrs` changes the texts that the index stages, so the site
        reindexes the index; a change of `boosts` applies to the next
        query."""
        catalog = self._catalog()
        self._check(catalog._p_jar)
        if indexed_attrs is not None:
            self._indexed_attrs = _attrs(indexed_attrs) or [self.id]
        if mode is not None:
            self.mode = _mode(mode)
        if language is not None:
            self.language = _languages(language)
        if boosts is not None:
            self.boosts = _boosts(boosts)
        settings_changed(catalog)

    def getLexicon(self):
        """The lexicon of ZCTextIndex whose pipeline parses the queries."""
        lexicon = getattr(aq_parent(aq_inner(self)), self.lexicon_id, None)
        if lexicon is None:
            raise LookupError(f"Lexicon {self.lexicon_id!r} not found")
        return lexicon

    # The places.

    def _catalog(self):
        catalog = _catalog_of(self)
        if catalog is None:
            raise Unsupported("the ZoeTextIndex is not in a catalog")
        return catalog

    def _jar(self, catalog, obj=None):
        """The Connection of `catalog`, else of `obj` or of a container of
        the index."""
        jar = catalog._p_jar
        if jar is None and obj is not None:
            jar = getattr(aq_base(obj), "_p_jar", None)
        if jar is None:
            for each in aq_chain(self):
                jar = getattr(aq_base(each), "_p_jar", None)
                if jar is not None:
                    break
        if jar is None:
            raise Unsupported("the catalog of the ZoeTextIndex has no database")
        return jar

    def _check(self, jar):
        _check(jar.db().storage)

    def _oid(self, catalog, jar):
        if catalog._p_oid is None:
            jar.add(catalog)
        return catalog._p_oid

    def _stage(self, obj=None):
        catalog = self._catalog()
        jar = self._jar(catalog, obj)
        self._check(jar)
        return self._oid(catalog, jar), _deltas(jar, create=True)

    def _reads(self):
        """The indexes that a query on this index reads: the index, then
        the indexes of its boosts."""
        out = [self.id]
        out.extend(name for name in self.boosts if name not in out)
        return out

    # Pluggable Index APIs

    def index_object(self, documentId, obj, threshold=None):
        """Stage the text of the indexed attributes of `obj`, joined as
        ZCTextIndex joins them, for the rid `documentId`. An object whose
        attributes give no text stages nothing."""
        catalog, deltas = self._stage(obj)
        texts = []
        for attr in self._indexed_attrs:
            text = getattr(obj, attr, None)
            if text is None:
                continue
            if safe_callable(text):
                text = text()
            if text is None:
                continue
            if isinstance(text, (list, tuple, set)):
                texts.extend(text)
            else:
                texts.append(text)
        texts = [t for t in texts if isinstance(t, str)]
        if not texts:
            return 0
        deltas.set(catalog, self.id, documentId, _language_of(obj), " ".join(texts))
        return 1

    def unindex_object(self, docid):
        """Stage an unindex of the rid `docid`."""
        catalog, deltas = self._stage()
        deltas.set(catalog, self.id, docid, "", None)

    def clear(self):
        """Stage a clear delta of the index. A catalog without a database
        has nothing to clear."""
        catalog = self._catalog()
        try:
            jar = self._jar(catalog)
        except Unsupported:
            return
        self._check(jar)
        _deltas(jar, create=True).clear(self._oid(catalog, jar), self.id)

    def _apply_index(self, request):
        record = IndexQuery(request, self.id, self.query_options)
        if record.keys is None:
            return None
        catalog = self._catalog()
        return (
            self.query_index(record, languages=self._languages_of(catalog, request)),
            (self.id,),
        )

    def query_index(self, record, resultset=None, languages=None):
        """Answer the text query of `record` with `text_query`: an
        `IIBucket` of the rids of the hits in `resultset` and their integer
        scores, or None for an empty query."""
        query = " ".join(record.keys)
        if not query:
            return None
        catalog = self._catalog()
        jar = self._jar(catalog)
        self._check(jar)
        storage, tid, clause = self._clause(catalog, jar, query, languages)
        rids = None
        if resultset is not None:
            keys = resultset.keys() if hasattr(resultset, "keys") else resultset
            rids = sorted(keys)
            values, size = _measure(clause)
            if size + 5 + 4 * len(rids) > _CAP_BYTES or values + 1 > _CAP_VALUES:
                # The filter passes the cap of a call: the index intersects
                # the reply itself.
                rids = None
        reply = storage.text_query(catalog._p_oid, tid, clause, rids)
        hits = zip(reply["r"], reply["sc"], strict=True)
        if resultset is not None and rids is None:
            keep = set(resultset.keys() if hasattr(resultset, "keys") else resultset)
            hits = ((r, s) for r, s in hits if r in keep)
        return IIBucket(list(hits))

    # The clause.

    def _target(self, jar):
        """The storage that a query asks and its exclusive tid: the storage
        of the Connection at its snapshot, or the base of a test storage at
        its last tid."""
        storage = jar.db().storage
        found = _chain(storage)
        if found is not None:
            base = found[1]
            return base, _next(base.lastTransaction()), found[0]
        tid = _snapshot(jar)
        if tid is None:
            tid = _next(storage.lastTransaction())
        return storage, tid, None

    def _base_serves(self, base, oid, tid, reads):
        """Tell if the base of a test storage holds the catalog `oid` and a
        text index for it with each index of `reads`."""
        try:
            if base.loadBefore(oid, tid) is None:
                return False
        except POSKeyError:
            return False
        if base.kv_get("durable", "zoe.catalog", oid) is None:
            return False
        value = base.kv_get("durable", NAMESPACE, oid)
        if value is None:
            return False
        try:
            indexes = json.loads(value)["indexes"]
        except (ValueError, KeyError, TypeError):
            return False
        return all(name in indexes for name in reads)

    def _clause(self, catalog, jar, query, languages=None):
        """The storage to ask, the tid, and the text clause of `query`
        without `k`. Staged texts that pass the cap of a call raise
        `zoe.storage.Unavailable`."""
        from zoe.storage import Unavailable

        node = translate(self.getLexicon(), query)
        reads = self._reads()
        storage, tid, chain = self._target(jar)
        oid = catalog._p_oid
        staged, cleared, settings = {}, set(), None
        if chain is not None:
            staged, cleared = _stack(chain, oid, set(reads))
            settings = self._settings()
            if oid is None or not self._base_serves(storage, oid, tid, reads):
                cleared.update(reads)
        deltas = _deltas(jar, create=False)
        if deltas is not None:
            for c, index in deltas.clears:
                if c == oid and index in reads:
                    cleared.add(index)
                    for key in [k for k in staged if k[0] == index]:
                        del staged[key]
            for (c, index, rid), value in deltas.texts.items():
                if c == oid and index in reads:
                    staged[(index, rid)] = value
        clause = {
            "i": self.id,
            "q": node,
            "lg": languages,
            "cl": sorted(cleared),
            "st": [
                [index, rid, language, text]
                for (index, rid), (language, text) in sorted(staged.items())
            ],
            "set": settings,
        }
        values, size = _measure(clause)
        if values > _CAP_VALUES or size > _CAP_BYTES:
            raise Unavailable("the staged texts pass the cap of a call")
        if oid is None:
            # A catalog that no commit stored: no index holds it.
            clause["cl"] = sorted(set(reads))
        return storage, tid, clause

    def _plan_clause(self, query, request):
        """The text clause of `query` in a plan of `catalog_query`, with the
        languages of the catalog query `request`."""
        catalog = self._catalog()
        jar = self._jar(catalog)
        self._check(jar)
        languages = self._languages_of(catalog, request)
        _storage, _tid, clause = self._clause(catalog, jar, query, languages)
        return {"k": "t", **clause}

    def _languages_of(self, catalog, request):
        """The fields that a query reads, as `lg`: None for every field.

        ZCatalog reads the `Language` clause of a query only when the
        catalog has an index `Language`. Then each language of the clause
        maps to a field of this index as a delta maps to it: the same tag,
        else the same primary subtag, else the first language of the
        settings. So the neutral language `""` maps to the first field. The
        index `Language` still filters the rows."""
        if "Language" not in catalog.indexes:
            return None
        wanted = _query_languages(request)
        fields = list(self.language) or [""]
        if wanted is None or len(fields) == 1:
            return None
        mapped = {fields[_field_of(fields, tag)] for tag in wanted}
        if mapped >= set(fields):
            return None
        return [f for f in fields if f in mapped]

    # The counts.

    def _info(self, rid=None):
        catalog = self._catalog()
        jar = self._jar(catalog)
        self._check(jar)
        storage, tid, chain = self._target(jar)
        oid = catalog._p_oid
        if oid is None:
            return None, chain, storage, tid
        if chain is None:
            return storage.text_info(oid, self.id, tid, rid), None, storage, tid
        return None, chain, storage, tid

    def _stack_view(self):
        """On a test storage: the texts of the stack, and whether the base
        answers for the rest."""
        catalog = self._catalog()
        jar = self._jar(catalog)
        self._check(jar)
        storage, tid, chain = self._target(jar)
        oid = catalog._p_oid
        texts, cleared = _stack(chain, oid, {self.id})
        serves = (
            oid is not None
            and not cleared
            and self._base_serves(storage, oid, tid, [self.id])
        )
        return texts, serves, storage, tid, oid

    def numObjects(self):
        """Return the number of distinct words of the index."""
        info, chain, _storage, _tid = self._info()
        if chain is None:
            return 0 if info is None else info["words"]
        texts, serves, storage, tid, oid = self._stack_view()
        if serves:
            return storage.text_info(oid, self.id, tid, None)["words"]
        words = set()
        for _language, text in texts.values():
            if text is not None:
                words.update(self._split(text))
        return len(words)

    def indexSize(self):
        """Return the number of documents of the index."""
        info, chain, _storage, _tid = self._info()
        if chain is None:
            return 0 if info is None else info["docs"]
        texts, serves, storage, tid, oid = self._stack_view()
        if not serves:
            return sum(1 for _l, text in texts.values() if text is not None)
        count = storage.text_info(oid, self.id, tid, None)["docs"]
        for (_index, rid), (_language, text) in texts.items():
            below = storage.text_info(oid, self.id, tid, rid)["text"] is not None
            count += (text is not None) - below
        return count

    def getEntryForObject(self, documentId, default=None):
        """Return the words of the live text of the rid `documentId`, split
        with the pipeline of the lexicon, or `default`."""
        info, chain, _storage, _tid = self._info(documentId)
        if chain is None:
            text = None if info is None else info["text"]
        else:
            texts, serves, storage, tid, oid = self._stack_view()
            found = texts.get((self.id, documentId), False)
            if found is not False:
                text = found[1]
            elif serves:
                text = storage.text_info(oid, self.id, tid, documentId)["text"]
            else:
                text = None
        if text is None:
            return default
        return self._split(text)

    def _split(self, text):
        last = [text]
        for element in _pipeline(self.getLexicon()):
            last = element.process(last)
        return list(last)

    def uniqueValues(self, name=None, withLengths=0):
        raise NotImplementedError

    def getIndexSourceNames(self):
        """Return the names of the indexed attributes."""
        return list(self._indexed_attrs)

    def getIndexQueryNames(self):
        """The index applies to the queries for its id. A pending index of
        a profile step applies to no query."""
        if self._is_pending():
            return ()
        return (self.id,)

    def _is_pending(self):
        """Tell if a profile step fills this index while the old index of
        its id serves."""
        return bool(getattr(aq_base(self), "_pending", False))

    def getIndexType(self):
        """Return the type of the index."""
        return "ZoeTextIndex"


InitializeClass(ZoeTextIndex)


def _field_of(languages, language):
    """The place of the field of `language` among `languages`, as the text
    index places a delta: the same tag, else the same primary subtag, else
    the first."""
    lower = [tag.lower() for tag in languages]
    if language.lower() in lower:
        return lower.index(language.lower())

    def primary(tag):
        return tag.replace("_", "-").split("-")[0].lower()

    for k, tag in enumerate(languages):
        if primary(tag) == primary(language):
            return k
    return 0


def _query_languages(query):
    """The languages of the `Language` clause of a catalog query, or None
    for every field."""
    get = getattr(query, "get", None)
    value = get("Language", None) if get is not None else None
    if isinstance(value, dict):
        value = value.get("query")
    if isinstance(value, str):
        value = [value]
    if not isinstance(value, (list, tuple)):
        return None
    out = []
    for tag in value:
        if not isinstance(tag, str) or len(tag) > _LANGUAGE or not tag.isascii():
            return None
        out.append(tag)
    return out


# The profile steps.


class _Extra:
    """The extra record of an index."""


def _site_of(setup):
    from Products.CMFCore.utils import getToolByName

    return getToolByName(setup, "portal_url").getPortalObject()


def _site_languages(site):
    """The languages of the site: the default language, then, with
    plone.app.multilingual, the other available languages."""
    registry = getattr(site, "portal_registry", None)
    default = "en"
    available = []
    if registry is not None:
        default = registry.get("plone.default_language", None) or default
        setup = getattr(site, "portal_setup", None)
        multilingual = setup is not None and (
            setup.getLastVersionForProfile("plone.app.multilingual:default")
            not in ("unknown", None)
        )
        if multilingual:
            available = list(registry.get("plone.available_languages", None) or [])
    return _languages([default, *available])


def _check_site(tool):
    """Raise `Unsupported` when the catalog of `tool` cannot hold a
    `ZoeTextIndex`."""
    jar = aq_base(tool)._catalog._p_jar or aq_base(tool)._p_jar
    if jar is None:
        raise Unsupported("the catalog has no database")
    _check(jar.db().storage)


def _reindex(tool, keys):
    """Index each catalogued object of `tool` in the indexes of the keys
    `keys`, with a commit after each `BATCH_SIZE` objects. The objects of
    the last batch wait for the commit of the caller."""
    paths = list(tool._catalog.uids.keys())
    for k, path in enumerate(paths, 1):
        obj = tool.resolve_path(path)
        if obj is not None:
            tool.catalog_object(obj, path, idxs=list(keys), update_metadata=0)
        if k % BATCH_SIZE == 0 and k < len(paths):
            transaction.commit()


def _drop_pending(catalog, keep):
    """Remove from `catalog` each pending index that is not an instance of
    `keep`: the pending indexes of the other profile step. The removal
    changes the mapping of the indexes directly, so the hook of `delIndex`
    stages no clear delta for the id that the pending index shares with a
    serving index. Return the ids of the dropped pending `ZoeTextIndex`
    indexes."""
    indexes = dict(catalog.indexes)
    dropped = []
    for key in [k for k in indexes if k.startswith(PENDING)]:
        index = aq_base(indexes[key])
        if isinstance(index, keep):
            continue
        del indexes[key]
        if isinstance(index, ZoeTextIndex):
            dropped.append(index.id)
    if len(indexes) != len(catalog.indexes):
        catalog.indexes = indexes
        settings_changed(catalog)
    return dropped


def _swap(catalog, names):
    """Put the pending index of each id of `names` in the place of the old
    index of the id, in the current transaction. The hook of `delIndex`
    stages a clear delta of an old `ZoeTextIndex`."""
    for name in names:
        if name in catalog.indexes:
            catalog.delIndex(name)
        indexes = dict(catalog.indexes)
        index = aq_base(indexes.pop(PENDING + name))
        index.id = name
        if "_pending" in index.__dict__:
            del index._pending
        indexes[name] = index
        catalog.indexes = indexes
    settings_changed(catalog)


def install_text(setup, mode="enhanced"):
    """The profile step `zoe.plone:text`, with the setup tool `setup` of a
    site.

    For each ZCTextIndex of `TEXT_INDEXES`, the step adds a pending
    `ZoeTextIndex` of the same id, which no query reads, and indexes the
    catalog into it in batches while the ZCTextIndex serves. A save during
    the step stages its texts in the pending index too. The last batch
    puts each pending index in the place of its ZCTextIndex, in the commit
    of the caller. An import after a failure finds the pending indexes and
    indexes the catalog again into them. The pending ZCTextIndex indexes of
    a step back that failed go first."""
    site = _site_of(setup)
    tool = site.portal_catalog
    if mode != "enhanced":
        raise Unsupported(f"the mode {mode!r} is not served")
    _check_site(tool)
    from zoe.plone import register_text_index

    register_text_index()
    catalog = tool._catalog
    languages = _site_languages(site)
    # The pending ZCTextIndex indexes of a step back that failed go: the
    # `ZoeTextIndex` indexes still serve.
    _drop_pending(catalog, ZoeTextIndex)
    names = []
    for name in TEXT_INDEXES:
        if PENDING + name in catalog.indexes:
            names.append(name)
            continue
        old = catalog.indexes.get(name)
        if old is None or getattr(aq_base(old), "meta_type", None) != "ZCTextIndex":
            continue
        old = catalog.getIndex(name)
        extra = _Extra()
        extra.indexed_attrs = ",".join(old.getIndexSourceNames())
        extra.lexicon_id = old.lexicon_id
        extra.mode = mode
        extra.language = ",".join(languages)
        if name == "SearchableText":
            extra.boosts = BOOSTS
        index = ZoeTextIndex(name, extra=extra, caller=tool)
        index._pending = True
        catalog.addIndex(PENDING + name, index)
        names.append(name)
    if names:
        settings_changed(catalog)
        _reindex(tool, [PENDING + name for name in names])
        _swap(catalog, names)


def uninstall_text(setup):
    """The profile step `zoe.plone:text-uninstall`, with the setup tool
    `setup` of a site.

    For each `ZoeTextIndex`, the step adds a pending ZCTextIndex with the
    same attributes and lexicon under the key `PENDING` and the id, which
    no query reads, and indexes the catalog into it in batches while the
    `ZoeTextIndex` serves. The last batch stages a clear delta of each
    `ZoeTextIndex` and puts each ZCTextIndex in its place, in the commit of
    the caller. An import after a failure indexes the catalog again into
    the pending indexes. The pending `ZoeTextIndex` indexes of a
    `zoe.plone:text` that failed go first, with a clear delta of their
    texts. The ZCTextIndex indexes are of the type `Okapi
    BM25 Rank`, the type of Plone."""
    from Products.ZCTextIndex.ZCTextIndex import ZCTextIndex

    site = _site_of(setup)
    tool = site.portal_catalog
    catalog = tool._catalog
    # The pending `ZoeTextIndex` indexes of a `zoe.plone:text` that failed
    # go, with a clear delta of their texts: the ZCTextIndex indexes of
    # their ids still serve.
    dropped = _drop_pending(catalog, ZCTextIndex)
    if dropped:
        _check_site(tool)
        deltas = _deltas(aq_base(catalog)._p_jar, create=True)
        for name in dropped:
            if not isinstance(aq_base(catalog.indexes.get(name)), ZoeTextIndex):
                deltas.clear(aq_base(catalog)._p_oid, name)
    serving = _text_indexes(aq_base(catalog), pending=False)
    names = sorted(
        set(serving)
        | {key[len(PENDING) :] for key in catalog.indexes if key.startswith(PENDING)}
    )
    if not names:
        return
    _check_site(tool)
    for name in names:
        if PENDING + name in catalog.indexes:
            continue
        index = catalog.getIndex(name)
        extra = _Extra()
        extra.doc_attr = ",".join(index.getIndexSourceNames())
        extra.lexicon_id = index.lexicon_id
        extra.index_type = "Okapi BM25 Rank"
        pending = ZCTextIndex(PENDING + name, extra=extra, caller=tool)
        catalog.addIndex(PENDING + name, pending)
    _reindex(tool, [PENDING + name for name in names])
    jar = aq_base(catalog)._p_jar
    deltas = _deltas(jar, create=True)
    for name in names:
        if name in serving:
            deltas.clear(aq_base(catalog)._p_oid, name)
    _swap(catalog, names)
