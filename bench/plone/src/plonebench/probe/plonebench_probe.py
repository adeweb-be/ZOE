"""The probe of a Zope process: a WSGI filter that records each request.

wsgi.ini puts the filter first in the pipeline:

    [filter:probe]
    use = call:plonebench_probe:filter_factory
    out = /path/of/the/probe.jsonl

The runner puts the directory of this module on PYTHONPATH. The filter is
the same code for every storage, so the numbers compare.

For each HTTP request, the probe writes one JSON line when the response
ends. The line holds the ids that Locust sends (`X-Bench-Op`,
`X-Bench-Attempt` and `X-Bench-Name`), never the URL, a query or a body.
It counts:

- the attempts of the publisher: `PubStart` events. An attempt after the
  first is a retry of ZPublisher after a `TransientError`;
- the errors of each failed attempt, with the class of the object of a
  `ConflictError`, whether the publisher retried, and the conflicts that
  the attempt resolved before it failed;
- the time of the commit of each attempt: from `PubBeforeCommit` to
  `PubSuccess` or `PubFailure`;
- the calls of the storage in the thread of the request: `loadBefore`,
  `store`, `tpc_vote`, `tpc_finish` and `tpc_abort`, with their time and
  their bytes; and `tryToResolveConflict`, with its outcome and the
  reason of each failure (for a bucket, the reason of BTrees);
- the CPU time of the thread of the request (`time.thread_time`).

The times overlap: the commit holds the vote and the finish, and a
request holds all of them. The report never adds them up.

A thread of the probe writes a snapshot of the process every 5 seconds:
the requests in flight, the objects in the caches of the ZODB connections,
and the size of the storage client cache when the client tells it.
"""

from __future__ import annotations

import json
import os
import threading
import time

_local = threading.local()
_lock = threading.Lock()
_state = {"in_flight": 0, "out": None, "installed": False}


class _Context:
    __slots__ = (
        "attempt_resolved",
        "attempts",
        "commit_seconds",
        "commit_start",
        "cpu_start",
        "errors",
        "finish_seconds",
        "finishes",
        "load_bytes",
        "load_seconds",
        "loads",
        "resolve_failed",
        "resolve_reasons",
        "resolve_seconds",
        "resolved",
        "retries",
        "store_bytes",
        "stores",
        "aborts",
        "vote_seconds",
        "votes",
    )

    def __init__(self):
        self.attempts = 0
        self.attempt_resolved = 0
        self.retries = 0
        self.errors = []
        self.commit_start = None
        self.commit_seconds = 0.0
        self.loads = 0
        self.load_bytes = 0
        self.load_seconds = 0.0
        self.stores = 0
        self.store_bytes = 0
        self.votes = 0
        self.vote_seconds = 0.0
        self.finishes = 0
        self.finish_seconds = 0.0
        self.aborts = 0
        self.resolved = 0
        self.resolve_failed = 0
        self.resolve_reasons = []
        self.resolve_seconds = 0.0
        self.cpu_start = time.thread_time()


def _context():
    return getattr(_local, "context", None)


def _write(record):
    out = _state["out"]
    if out is None:
        return
    line = json.dumps(record, separators=(",", ":")) + "\n"
    with _lock:
        out.write(line)


def _on_event(event):
    context = _context()
    if context is None:
        return
    name = type(event).__name__
    if name == "PubStart":
        context.attempts += 1
        context.attempt_resolved = context.resolved
        context.commit_start = None
    elif name == "PubBeforeCommit":
        context.commit_start = time.perf_counter()
    elif name in ("PubSuccess", "PubFailure"):
        if context.commit_start is not None:
            context.commit_seconds += time.perf_counter() - context.commit_start
            context.commit_start = None
        if name == "PubFailure":
            exc = event.exc_info[1]
            retry = bool(event.retry)
            if retry:
                context.retries += 1
            error = _error(exc, retry, context.attempts)
            # The conflicts that this attempt resolved before it failed.
            error["resolved"] = context.resolved - context.attempt_resolved
            context.errors.append(error)


def _error(exc, retry, attempt):
    """Return a record of an exception, without its message."""
    from ZODB.POSException import ConflictError, ReadConflictError

    kind = type(exc).__name__
    record = {"attempt": attempt, "error": kind, "retry": retry}
    if isinstance(exc, ConflictError):
        record["conflict"] = "read" if isinstance(exc, ReadConflictError) else "write"
        record["class"] = getattr(exc, "class_name", None)
        oid = getattr(exc, "oid", None)
        record["oid"] = oid.hex() if isinstance(oid, bytes) else None
    return record


def _wrap(storage, name, measure):
    original = getattr(storage, name, None)
    if original is None:
        return

    def wrapper(*args, **kwargs):
        context = _context()
        if context is None:
            return original(*args, **kwargs)
        start = time.perf_counter()
        result = None
        try:
            result = original(*args, **kwargs)
        finally:
            # A failed vote counts too: its time is part of the attempt.
            measure(context, time.perf_counter() - start, args, result)
        return result

    wrapper.__wrapped__ = original
    setattr(storage, name, wrapper)


def _measure_load(context, seconds, args, result):
    context.loads += 1
    context.load_seconds += seconds
    if result:
        context.load_bytes += len(result[0])


def _measure_store(context, seconds, args, result):
    context.stores += 1
    if len(args) > 2 and args[2] is not None:
        context.store_bytes += len(args[2])


def _measure_vote(context, seconds, args, result):
    context.votes += 1
    context.vote_seconds += seconds


def _measure_finish(context, seconds, args, result):
    context.finishes += 1
    context.finish_seconds += seconds


def _measure_abort(context, seconds, args, result):
    context.aborts += 1


def failure_reason(args):
    """Return the reason of a failed resolution from the arguments of the
    debug message of ZODB's resolver: the class of the object, then the
    message of its error without the positions of a BTrees error."""
    klass, error, message = args
    name = getattr(klass, "__name__", str(klass))
    if error == "BTreesConflictError" and ": " in message:
        message = message.split(": ", 1)[1]
    return f"{name}: {error}: {message}"


class _ResolverLogger:
    """Stand in for the logger of `ZODB.ConflictResolution`. Its debug
    message tells why a resolution failed: the logger keeps the reason
    for the thread and drops the message. Every other call goes to the
    real logger."""

    def __init__(self, logger):
        self._logger = logger

    def debug(self, message, *args, **kwargs):
        if len(args) == 3:
            _local.reason = failure_reason(args)
        else:
            self._logger.debug(message, *args, **kwargs)

    def __getattr__(self, name):
        return getattr(self._logger, name)


def _install_resolution():
    """Count the conflicts that ZODB's resolver runs in this process."""
    from ZODB import ConflictResolution

    cls = ConflictResolution.ConflictResolvingStorage
    original = cls.tryToResolveConflict
    if getattr(original, "plonebench", False):
        return
    ConflictResolution.logger = _ResolverLogger(ConflictResolution.logger)

    def tryToResolveConflict(self, *args, **kwargs):
        context = _context()
        start = time.perf_counter()
        _local.reason = None
        try:
            result = original(self, *args, **kwargs)
        except Exception:
            reason = getattr(_local, "reason", None) or "unexpected error"
            if context is not None:
                context.resolve_failed += 1
                context.resolve_reasons.append(reason)
                context.resolve_seconds += time.perf_counter() - start
            else:
                _count_outside("resolve_failed")
            raise
        if context is not None:
            context.resolved += 1
            context.resolve_seconds += time.perf_counter() - start
        else:
            _count_outside("resolved")
        return result

    tryToResolveConflict.plonebench = True
    cls.tryToResolveConflict = tryToResolveConflict


_outside = {"resolved": 0, "resolve_failed": 0}


def _count_outside(key):
    with _lock:
        _outside[key] += 1


def _install(db):
    if _state["installed"]:
        return
    _state["installed"] = True
    import zope.event

    zope.event.subscribers.append(_on_event)
    storage = db.storage
    _wrap(storage, "loadBefore", _measure_load)
    _wrap(storage, "store", _measure_store)
    _wrap(storage, "tpc_vote", _measure_vote)
    _wrap(storage, "tpc_finish", _measure_finish)
    _wrap(storage, "tpc_abort", _measure_abort)
    _install_resolution()


def _client_cache(storage):
    """Return what the storage client cache tells of itself, or None."""
    cache = getattr(storage, "_cache", None)
    if cache is None:
        return None
    result = {}
    for name in ("maxsize",):
        value = getattr(cache, name, None)
        if isinstance(value, int):
            result[name] = value
    try:
        result["entries"] = len(cache)
    except Exception:
        pass
    stats = getattr(cache, "getStats", None)
    if callable(stats):
        try:
            adds, added_bytes, evicts, evicted_bytes, accesses = stats()[:5]
            result.update(
                adds=adds,
                added_bytes=added_bytes,
                evicts=evicts,
                evicted_bytes=evicted_bytes,
                accesses=accesses,
            )
        except Exception:
            pass
    return result or None


def _snapshots(db, interval):
    while True:
        time.sleep(interval)
        try:
            objects = db.cacheSize()
        except Exception:
            objects = None
        with _lock:
            outside = dict(_outside)
        _write(
            {
                "kind": "snapshot",
                "t": time.time(),
                "pid": os.getpid(),
                "in_flight": _state["in_flight"],
                "cache_objects": objects,
                "client_cache": _client_cache(db.storage),
                "outside": outside,
            }
        )


class ProbeMiddleware:
    def __init__(self, app):
        self.app = app

    def __call__(self, environ, start_response):
        if not _state["installed"]:
            import Zope2

            _install(Zope2.DB)
            threading.Thread(
                target=_snapshots, args=(Zope2.DB, 5.0), daemon=True, name="probe"
            ).start()
        context = _Context()
        _local.context = context
        start = time.perf_counter()
        wall = time.time()
        status = []

        def recording_start_response(code, headers, exc_info=None):
            status.append(code.split(" ", 1)[0])
            return start_response(code, headers, exc_info)

        with _lock:
            _state["in_flight"] += 1
        try:
            result = self.app(environ, recording_start_response)
        except BaseException:
            self._end(environ, context, start, wall, "exception")
            raise
        # publish_module returned: the retries and the commit are over.
        self._end(environ, context, start, wall, status[0] if status else None)
        return result

    def _end(self, environ, context, start, wall, status):
        _local.context = None
        with _lock:
            _state["in_flight"] -= 1
        _write(
            {
                "kind": "request",
                "t": wall,
                "pid": os.getpid(),
                "op": environ.get("HTTP_X_BENCH_OP"),
                "attempt": environ.get("HTTP_X_BENCH_ATTEMPT"),
                "name": environ.get("HTTP_X_BENCH_NAME"),
                "method": environ.get("REQUEST_METHOD"),
                "status": status,
                "seconds": time.perf_counter() - start,
                "cpu": time.thread_time() - context.cpu_start,
                "attempts": context.attempts,
                "retries": context.retries,
                "errors": context.errors,
                "commit_seconds": context.commit_seconds,
                "loads": context.loads,
                "load_bytes": context.load_bytes,
                "load_seconds": context.load_seconds,
                "stores": context.stores,
                "store_bytes": context.store_bytes,
                "votes": context.votes,
                "vote_seconds": context.vote_seconds,
                "finishes": context.finishes,
                "finish_seconds": context.finish_seconds,
                "aborts": context.aborts,
                "resolved": context.resolved,
                "resolve_failed": context.resolve_failed,
                "resolve_reasons": context.resolve_reasons,
                "resolve_seconds": context.resolve_seconds,
            }
        )


def filter_factory(global_conf, out, **local_conf):
    """Return the filter of the pipeline. `out` is the JSON lines file."""
    os.makedirs(os.path.dirname(out), exist_ok=True)
    _state["out"] = open(out, "a", buffering=1)

    def make(app):
        return ProbeMiddleware(app)

    return make
