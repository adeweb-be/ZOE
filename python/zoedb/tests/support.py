"""The helpers that the tests of the zoe package share.

pytest puts the directory of each `conftest.py` on `sys.path`, so a test
module in this directory or below imports this module as `support`. The
shared fixtures are in `conftest.py`.
"""

import contextlib
import io
import pickle
import socket
import subprocess
import time
import urllib.request

# The longest wait of a test for a server, a client or a condition.
TIMEOUT = 30

# The serial of a new object.
ZERO = bytes(8)


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def counter(value):
    """A record of ZODB's PCounter, whose _p_resolveConflict adds."""
    meta = (("ZODB.tests.ConflictResolution", "PCounter"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def strict(value):
    """A record of ZODB's PCounter2, whose _p_resolveConflict refuses."""
    meta = (("ZODB.tests.ConflictResolution", "PCounter2"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def value_of(data):
    """Return the `_value` of the state of a counter record."""
    unpickler = pickle.Unpickler(io.BytesIO(data))
    unpickler.load()
    return unpickler.load()["_value"]


def _begin(storage, user, description, extension, tid):
    """Begin a transaction on `storage`, and return its metadata."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(user, description, extension)
    if tid is None:
        storage.tpc_begin(txn)
    else:
        storage.tpc_begin(txn, tid)
    return txn


def commit(storage, stores, user="", description="", extension=None, tid=None, f=None):
    """Commit `stores`, each `(oid, serial, data)`, through `storage` in one
    transaction. Return the tid.

    `user`, `description` and `extension` are the metadata of the
    transaction, and `tid` the tid of a restore transaction. `f` is the
    callback of `tpc_finish`. A failed step aborts the transaction and
    raises.
    """
    txn = _begin(storage, user, description, extension, tid)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        if f is None:
            return storage.tpc_finish(txn)
        return storage.tpc_finish(txn, f)
    except BaseException:
        storage.tpc_abort(txn)
        raise


def commit_one(storage, oid, serial, data, **metadata):
    """Commit one record in one transaction, with the keyword arguments of
    `commit`. Return the tid."""
    return commit(storage, [(oid, serial, data)], **metadata)


def commit_resolving(storage, stores):
    """Commit `stores` as `commit` does. Return the tid and the result of
    the vote: the oids that the vote resolved."""
    txn = _begin(storage, "", "", None, None)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        resolved = storage.tpc_vote(txn)
        return storage.tpc_finish(txn), resolved
    except BaseException:
        storage.tpc_abort(txn)
        raise


def commit_kv(storage, *writes):
    """Commit `writes` through the KV namespaces of `storage`. Each write is
    a tuple of the arguments of `kv_set` without the transaction, or
    `("delete", flavour, name, key)`. Return the tid."""
    txn = _begin(storage, "", "", None, None)
    for write in writes:
        if write[0] == "delete":
            storage.kv_delete(*write[1:], txn)
        else:
            storage.kv_set(*write, txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def wait_for(condition, what="the condition", timeout=TIMEOUT):
    """Poll `condition` until it is true. Fail after `timeout` seconds."""
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} did not happen in {timeout} seconds")
        time.sleep(0.01)


def free_address():
    """Return a TCP address where nothing listens."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()


def client(server, storage="1", **kw):
    """Open a stock `ZEO.ClientStorage` on the storage `storage` of
    `server`, a `ZoeServer` or an address. The client waits at most
    `TIMEOUT` seconds for the server, unless `kw` gives `wait_timeout`."""
    from ZEO.ClientStorage import ClientStorage

    address = getattr(server, "address", server)
    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(address, storage=storage, **kw)


def native(server, storage="1", **kw):
    """Open the native client on the storage `storage` of `server`, a
    `ZoeServer` or an address, as `client` opens a stock client."""
    import zoe.testing

    address = getattr(server, "address", server)
    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.testing.native_client(address, storage=storage, **kw)


@contextlib.contextmanager
def serving(path, environment=None):
    """Run `zoe serve` on the store in `path`, with the `ZOE_*` variables of
    `environment`. Yield its address. Kill the server at the end. A server
    that does not listen within `TIMEOUT` seconds fails the start."""
    from zoe.testing import ZoeServer

    server = ZoeServer(str(path), env=environment).start(TIMEOUT)
    try:
        yield server.address
    finally:
        server.kill()


def metrics(server):
    """Return the text of `GET /metrics` on the admin port of `server`, a
    `ZoeServer` started with `--admin-listen`."""
    url = f"http://{server.admin}/metrics"
    with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
        return reply.read().decode()


def openssl(*args, cwd):
    """Run the `openssl` command with `args` in the directory `cwd`."""
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


def make_ca(directory, name):
    """Make the self-signed CA certificate `name.pem` and its key
    `name.key` in `directory`."""
    openssl(
        "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", f"/CN={name}", "-keyout", f"{name}.key", "-out", f"{name}.pem",
        "-addext", "keyUsage = critical, keyCertSign, cRLSign",
        cwd=directory,
    )  # fmt: skip


def make_cert(directory, name, ca, server=False):
    """Make the certificate `name.pem` and its key `name.key` in
    `directory`, signed by the CA `ca` of the directory. A server
    certificate is for `localhost` and `127.0.0.1`. Another certificate is
    for client authentication."""
    openssl(
        "req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}",
        "-keyout", f"{name}.key", "-out", f"{name}.csr",
        cwd=directory,
    )  # fmt: skip
    extensions = directory / f"{name}.ext"
    if server:
        extensions.write_text("subjectAltName = DNS:localhost, IP:127.0.0.1\n")
    else:
        extensions.write_text("extendedKeyUsage = clientAuth\n")
    openssl(
        "x509", "-req", "-in", f"{name}.csr", "-CA", f"{ca}.pem",
        "-CAkey", f"{ca}.key", "-CAcreateserial", "-days", "2",
        "-extfile", f"{name}.ext", "-out", f"{name}.pem",
        cwd=directory,
    )  # fmt: skip
