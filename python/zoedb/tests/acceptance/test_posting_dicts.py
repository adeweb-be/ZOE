"""Acceptance test of the native merge of the posting dicts of a text index.

ZCTextIndex keeps, for each word id, a dict of document id to score, until
the dict holds 10 documents; then it puts them in an `IIBTree`. These
dicts are values of the IOBTree `_wordinfo`. When two transactions index
documents that share a rare word, both change the dict of that word, and
ZODB's resolver refuses the bucket. With the posting-dict rule on, ZOE
merges the two dicts entry by entry against the old dict.

The test checks, with the rule on, in an `IOBucket` and in an `IOBTree`
that holds one bucket:

- concurrent changes of the dict values merge entry by entry, and ZOE
  calls `tryToResolveConflict` in the client for none of them;
- each refusal case raises `ConflictError`: one entry changed differently
  on both sides; a delete of the word against a change of its dict; a
  dict against an `IIBTree`; a merged dict of more than 10 entries; dicts
  of float scores, which are not posting dicts.

With the rule off, a case gives the result of FileStorage with ZODB's
resolver, as today.

On `Products.ZCTextIndex`, with the rule on, two transactions index
documents that share rare words, from the same snapshot, and both commit
at the first attempt. After them, the index equals a reindex from scratch
of the final documents: `_wordinfo`, `_docweight`, `_docwords`, `length`,
`document_count` and `_totaldoclen`.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import contextlib
import dataclasses
import os
import subprocess
import threading

import pytest

import zoe.testing
from zoe.testing import ZoeServer

# The longest time that a test waits for the server.
TIMEOUT = 30
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The shapes of `_wordinfo`: a bucket, and a BTree that holds one bucket.
SHAPES = ("IOBucket", "IOBTree")


def rule(on):
    """Return the environment that switches the posting-dict rule on or off.

    ADR 0042 names the key and how a store switches the rule on in both
    modes. The human approves a change of this function with the ADR. The
    rule is the key `posting-dicts` of `<zoe>` and of `zoe serve`, a mode,
    through its environment twin: `<zoe>` reads it in-process, and
    `zoe serve` reads it as the twin of `--posting-dicts`.
    """
    return {"ZOE_POSTING_DICTS": "on" if on else "off"}


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
def opened(mode, path, environment, monkeypatch):
    """Open a storage of mode in path with environment. Close it at the end."""
    if mode == "in-process":
        import ZODB.config

        for name, value in environment.items():
            monkeypatch.setenv(name, value)
        storage = ZODB.config.storageFromString(
            f"%import zoe\n<zoe>\n  path {path}\n</zoe>\n"
        )
        try:
            yield storage
        finally:
            storage.close()
        return
    with serving(path, environment) as address:
        if mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(address, storage="1", wait_timeout=TIMEOUT)
        else:
            storage = zoe.testing.native_client(
                address, storage="1", wait_timeout=TIMEOUT
            )
        try:
            yield storage
        finally:
            storage.close()


class Calls:
    """Count the calls of `tryToResolveConflict`, in every thread."""

    def __init__(self, monkeypatch):
        from ZODB.ConflictResolution import ConflictResolvingStorage

        self.count = 0
        lock = threading.Lock()
        original = ConflictResolvingStorage.tryToResolveConflict

        def counted(storage, *args, **kwargs):
            with lock:
                self.count += 1
            return original(storage, *args, **kwargs)

        monkeypatch.setattr(ConflictResolvingStorage, "tryToResolveConflict", counted)


def tree(items):
    """Return an `IIBTree` of items: a posting that ZCTextIndex converted."""
    from BTrees.IIBTree import IIBTree

    return IIBTree(items)


@dataclasses.dataclass(frozen=True)
class Case:
    """Two transactions that change the posting dicts of one bucket.

    `committed` and `new` map a word id to its new posting, a dict or a
    function that returns an `IIBTree`, or to None for a delete. `merged`
    is the state after both, or None when the second must conflict.
    """

    name: str
    base: dict
    committed: dict
    new: dict
    merged: dict | None


BASE = {10: {1: 3, 2: 1}, 20: {1: 2}, 30: {5: 1}, 40: {1: 1}}
NINE = {50: {docid: 1 for docid in range(1, 10)}}
EIGHT = {50: {docid: 1 for docid in range(1, 9)}}

MERGES = [
    Case(
        "two documents join one word",
        BASE,
        {10: {1: 3, 2: 1, 3: 4}},
        {10: {1: 3, 2: 1, 4: 2}},
        {**BASE, 10: {1: 3, 2: 1, 3: 4, 4: 2}},
    ),
    Case(
        "a score changes beside a new document",
        BASE,
        {10: {1: 7, 2: 1}},
        {10: {1: 3, 2: 1, 4: 2}},
        {**BASE, 10: {1: 7, 2: 1, 4: 2}},
    ),
    Case(
        "a document leaves beside a new document",
        BASE,
        {10: {1: 3}},
        {10: {1: 3, 2: 1, 4: 2}},
        {**BASE, 10: {1: 3, 4: 2}},
    ),
    Case(
        "other words change too",
        BASE,
        {10: {1: 3, 2: 1, 3: 4}, 20: {1: 2, 3: 1}},
        {10: {1: 3, 2: 1, 4: 2}, 30: {5: 1, 4: 6}},
        {**BASE, 10: {1: 3, 2: 1, 3: 4, 4: 2}, 20: {1: 2, 3: 1}, 30: {5: 1, 4: 6}},
    ),
    Case(
        "ten entries at most",
        EIGHT,
        {50: {**EIGHT[50], 20: 1}},
        {50: {**EIGHT[50], 21: 1}},
        {50: {**EIGHT[50], 20: 1, 21: 1}},
    ),
]

REFUSALS = [
    Case(
        "one entry changed differently",
        BASE,
        {10: {1: 5, 2: 1}},
        {10: {1: 6, 2: 1}},
        None,
    ),
    Case(
        "a delete of the word against a change of its dict",
        BASE,
        {20: None},
        {20: {1: 2, 4: 1}},
        None,
    ),
    Case(
        "a dict against an IIBTree",
        BASE,
        {10: lambda: tree({1: 3, 2: 1, 3: 4})},
        {10: {1: 3, 2: 1, 4: 2}},
        None,
    ),
    Case(
        "more than ten entries",
        NINE,
        {50: {**NINE[50], 20: 1}},
        {50: {**NINE[50], 21: 1}},
        None,
    ),
    Case(
        "float scores",
        {**BASE, 40: {1: 0.5}},
        {40: {1: 0.5, 2: 0.25}},
        {40: {1: 0.5, 3: 1.5}},
        None,
    ),
]


def change(bucket, postings):
    """Set or delete the postings of each word id in bucket."""
    for wid, posting in postings.items():
        if posting is None:
            del bucket[wid]
        elif callable(posting):
            bucket[wid] = posting()
        else:
            # ZCTextIndex stores the dict again, so the bucket changes.
            bucket[wid] = dict(posting)


def plain(bucket):
    """Return the postings of bucket as plain dicts."""
    return {wid: dict(posting.items()) for wid, posting in bucket.items()}


def race(storage, shape, case):
    """Run case on a new object of shape in storage.

    Return the outcome of the second commit, the state after both commits,
    and the state that the second connection then sees.
    """
    import BTrees.IOBTree
    import transaction
    import ZODB
    from ZODB.POSException import ConflictError

    db = ZODB.DB(storage)
    try:
        with db.transaction() as conn:
            conn.root()["wordinfo"] = getattr(BTrees.IOBTree, shape)(
                {wid: dict(posting) for wid, posting in case.base.items()}
            )
        first_tm = transaction.TransactionManager()
        second_tm = transaction.TransactionManager()
        first = db.open(transaction_manager=first_tm)
        second = db.open(transaction_manager=second_tm)
        first_tm.begin()
        second_tm.begin()
        change(first.root()["wordinfo"], case.committed)
        change(second.root()["wordinfo"], case.new)
        first_tm.commit()
        try:
            second_tm.commit()
        except ConflictError:
            second_tm.abort()
            outcome = "conflict"
        else:
            outcome = "merged"
        second_tm.begin()
        seen = plain(second.root()["wordinfo"])
        second_tm.abort()
        first_tm.begin()
        state = plain(first.root()["wordinfo"])
        first_tm.abort()
        first.close()
        second.close()
        return outcome, state, seen
    finally:
        db.close()


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("shape", SHAPES)
@pytest.mark.parametrize("case", MERGES, ids=lambda case: case.name)
def test_posting_dicts_merge_entry_by_entry(mode, shape, case, tmp_path, monkeypatch):
    calls = Calls(monkeypatch)
    with opened(mode, tmp_path / "store", rule(True), monkeypatch) as storage:
        outcome, state, seen = race(storage, shape, case)
    assert outcome == "merged"
    assert state == case.merged
    # The vote returned the oid as resolved, so the second connection
    # loads the merged state.
    assert seen == case.merged
    assert calls.count == 0


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("shape", SHAPES)
@pytest.mark.parametrize("case", REFUSALS, ids=lambda case: case.name)
def test_the_rule_refuses(mode, shape, case, tmp_path, monkeypatch):
    with opened(mode, tmp_path / "store", rule(True), monkeypatch) as storage:
        outcome, state, _ = race(storage, shape, case)
    assert outcome == "conflict"
    # The first commit stays.
    expected = {**case.base}
    for wid, posting in case.committed.items():
        if posting is None:
            del expected[wid]
        else:
            expected[wid] = dict(posting().items()) if callable(posting) else posting
    assert state == expected


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("case", MERGES + REFUSALS, ids=lambda case: case.name)
def test_with_the_rule_off_a_case_ends_as_on_filestorage(
    mode, case, tmp_path, monkeypatch
):
    from ZODB.FileStorage import FileStorage

    reference = race(FileStorage(str(tmp_path / "Data.fs")), "IOBucket", case)
    with opened(mode, tmp_path / "store", rule(False), monkeypatch) as storage:
        assert race(storage, "IOBucket", case) == reference


# The words of the documents: generic tokens, 150 of them.
WORDS = [f"w{number:03}" for number in range(150)]


def text(first, count, repeat=1):
    """Return a document of `count` words from the word `first` on."""
    words = [WORDS[(first + offset) % len(WORDS)] for offset in range(count)]
    return " ".join(words * repeat)


# Each word of the first documents is in 3 of them: every posting is a dict.
DOCUMENTS = {docid: text(5 * docid, 15, 1 + docid % 2) for docid in range(1, 31)}
# Two rounds of two documents. The two documents of a round share ten words.
ROUNDS = [
    {101: text(0, 20), 102: text(10, 20, 2)},
    {103: text(40, 20, 3), 104: text(50, 20)},
]


def text_index():
    """Return a new Okapi index of ZCTextIndex and its lexicon."""
    try:
        from Products.ZCTextIndex.Lexicon import CaseNormalizer, Lexicon, Splitter
        from Products.ZCTextIndex.OkapiIndex import OkapiIndex
    except ImportError as error:
        pytest.fail(
            "Products.ZCTextIndex is not installed in the test environment: "
            f"the test of the text index needs it ({error})"
        )
    lexicon = Lexicon(Splitter(), CaseNormalizer())
    return OkapiIndex(lexicon), lexicon


def contents(index, lexicon):
    """Return the parts of index that a merge changes, as plain data.

    The lexicon gives each new word a word id from a random start, so the
    data names each word by its text, not by its word id.
    """
    from Products.ZCTextIndex import WidCode

    word = lexicon.get_word
    return {
        "wordinfo": {
            word(wid): (isinstance(posting, dict), dict(posting.items()))
            for wid, posting in index._wordinfo.items()
        },
        "docweight": dict(index._docweight.items()),
        "docwords": {
            docid: [word(wid) for wid in WidCode.decode(code)]
            for docid, code in index._docwords.items()
        },
        "length": index.length(),
        "document_count": index.document_count(),
        "totaldoclen": index._totaldoclen(),
    }


@pytest.mark.parametrize("mode", MODES)
def test_a_text_index_equals_a_reindex_from_scratch(mode, tmp_path, monkeypatch):
    import transaction
    import ZODB
    from ZODB.POSException import ConflictError

    index, lexicon = text_index()
    calls = Calls(monkeypatch)
    with opened(mode, tmp_path / "store", rule(True), monkeypatch) as storage:
        db = ZODB.DB(storage)
        try:
            with db.transaction() as conn:
                conn.root()["lexicon"] = lexicon
                conn.root()["index"] = index
                for docid, document in DOCUMENTS.items():
                    index.index_doc(docid, document)
            managers = [transaction.TransactionManager() for _ in range(2)]
            conns = [db.open(transaction_manager=tm) for tm in managers]
            for documents in ROUNDS:
                for tm in managers:
                    tm.begin()
                for conn, (docid, document) in zip(
                    conns, documents.items(), strict=True
                ):
                    conn.root()["index"].index_doc(docid, document)
                for tm, docid in zip(managers, documents, strict=True):
                    try:
                        tm.commit()
                    except ConflictError as error:
                        tm.abort()
                        pytest.fail(f"document {docid} did not commit: {error}")
            for conn in conns:
                conn.close()
            with db.transaction() as conn:
                root = conn.root()
                stored = contents(root["index"], root["lexicon"])
        finally:
            db.close()
    assert calls.count == 0

    scratch, scratch_lexicon = text_index()
    for docid, document in DOCUMENTS.items():
        scratch.index_doc(docid, document)
    for documents in ROUNDS:
        for docid, document in documents.items():
            scratch.index_doc(docid, document)
    assert stored == contents(scratch, scratch_lexicon)
