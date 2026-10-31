"""Acceptance test: the native client connects as `ZEO.ClientStorage` does.

`just test` runs it. The tests open the native client through the helpers
of `zoe.testing`: `native_client(address, **kw)`, and `NATIVE_LAYER` for a
`<zoeclient>` section.

- TLS comes from the `<ssl>` section of `<zoeclient>`, with the keys
  `certificate`, `key`, `authenticate`, `check-hostname` and
  `server-hostname`. The server is `zoe serve` with `--tls-cert`,
  `--tls-key` and `--tls-client-ca`. The client checks the certificate of
  the server against `authenticate`, and with `check-hostname` also its
  name. A failed check fails the open with `ClientDisconnected`, as for
  ZEO.
- An address is a `(host, port)` tuple, the path of a unix domain socket
  as a `str`, or a list of them. `zoe serve --listen PATH` listens on a
  unix domain socket. With a list, the client connects to the addresses
  that answer and keeps one server: a writable server before a read-only
  one.

The test makes its certificates with the `openssl` command, as
`server/test_tls.py` does, so the repository holds no key.
"""

import threading

import pytest
from support import TIMEOUT, ZERO, free_address, make_ca, make_cert, wait_for
from ZEO.Exceptions import ClientDisconnected

import zoe
import zoe.testing
from zoe.testing import ZoeServer

# The open wait of a client that must fail.
REFUSED = 2


@pytest.fixture(scope="module")
def certs(tmp_path_factory):
    """The certificates of the test.

    `server-ca` signs the server, `client-ca` signs the client `writer`,
    and `other-ca` signs nothing that the test uses.
    """
    directory = tmp_path_factory.mktemp("certs")
    for ca in ["server-ca", "client-ca", "other-ca"]:
        make_ca(directory, ca)
    make_cert(directory, "server", "server-ca", server=True)
    make_cert(directory, "writer", "client-ca")
    return directory


def tls_args(certs):
    return [
        "--tls-cert", str(certs / "server.pem"),
        "--tls-key", str(certs / "server.key"),
        "--tls-client-ca", str(certs / "client-ca.pem"),
    ]  # fmt: skip


def ssl_section(certs, authenticate="server-ca", **keys):
    """Return the `<ssl>` section of the client `writer`.

    Each keyword argument is one more key of the section, with `-` in
    place of `_`.
    """
    lines = [
        f"certificate {certs / 'writer.pem'}",
        f"key {certs / 'writer.key'}",
        f"authenticate {certs / f'{authenticate}.pem'}",
    ]
    lines += [f"{name.replace('_', '-')} {value}" for name, value in keys.items()]
    return "<ssl>\n" + "\n".join(lines) + "\n</ssl>"


def zoeclient(address, ssl, wait_timeout=TIMEOUT):
    host, port = address
    return f"""
        %import zoe
        <zoeclient>
          server {host}:{port}
          storage 1
          wait-timeout {wait_timeout}
          {ssl}
        </zoeclient>
        """


def assert_native():
    """Fail when a network thread of ZEO's own client runs."""
    names = [thread.name for thread in threading.enumerate()]
    assert not [n for n in names if n.endswith("zeo client networking thread")], names


def commit_and_load(storage):
    """Commit one record and load it. Return its oid and tid."""
    from ZODB.Connection import TransactionMetaData

    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, ZERO, b"native", "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    assert storage.load(oid) == (b"native", tid)
    return oid, tid


@pytest.fixture
def native():
    """Open native clients. Close them at the end of the test."""
    opened = []

    def open_native(address, **kw):
        kw.setdefault("storage", "1")
        kw.setdefault("wait_timeout", TIMEOUT)
        storage = zoe.testing.native_client(address, **kw)
        opened.append(storage)
        return storage

    yield open_native
    for storage in opened:
        storage.close()


@pytest.fixture
def section():
    """Open `<zoeclient>` sections inside the native layer.

    The layer starts at the first open and ends after the storages close.
    """
    import ZODB.config

    layers, opened = [], []

    def open_section(text):
        if not layers:
            layer = zoe.testing.NATIVE_LAYER
            layer.setUp()
            layers.append(layer)
        storage = ZODB.config.storageFromString(text)
        opened.append(storage)
        return storage

    yield open_section
    for storage in opened:
        storage.close()
    for layer in layers:
        layer.tearDown()


def test_the_client_commits_with_its_certificate_from_an_ssl_section(
    tmp_path, certs, section
):
    with ZoeServer(str(tmp_path / "store"), args=tls_args(certs)) as server:
        ssl = ssl_section(certs, check_hostname="true", server_hostname="localhost")
        storage = section(zoeclient(server.address, ssl))
        assert_native()
        commit_and_load(storage)


def test_a_ca_that_does_not_sign_the_server_fails_the_open(tmp_path, certs, section):
    with ZoeServer(str(tmp_path / "store"), args=tls_args(certs)) as server:
        ssl = ssl_section(certs, authenticate="other-ca", server_hostname="localhost")
        with pytest.raises(ClientDisconnected):
            section(zoeclient(server.address, ssl, wait_timeout=REFUSED))


def test_check_hostname_with_a_wrong_server_hostname_fails_the_open(
    tmp_path, certs, section
):
    with ZoeServer(str(tmp_path / "store"), args=tls_args(certs)) as server:
        ssl = ssl_section(certs, check_hostname="true", server_hostname="example.org")
        with pytest.raises(ClientDisconnected):
            section(zoeclient(server.address, ssl, wait_timeout=REFUSED))


def test_the_client_connects_by_the_path_of_a_unix_socket(
    tmp_path, monkeypatch, native
):
    # A relative path stays short: a socket path holds at most 108 bytes.
    monkeypatch.chdir(tmp_path)
    server = ZoeServer(str(tmp_path / "store"))
    server.address = "./zeo.sock"
    with server:
        storage = native("./zeo.sock")
        assert_native()
        commit_and_load(storage)


def test_with_two_addresses_the_client_connects_to_the_open_one(tmp_path, native):
    closed = free_address()
    with ZoeServer(str(tmp_path / "store")) as server:
        storage = native([closed, server.address])
        commit_and_load(storage)


def test_the_client_keeps_the_writable_server(tmp_path, native):
    zoe.Storage(str(tmp_path / "read-only")).close()
    with (
        ZoeServer(str(tmp_path / "read-only"), args=["--read-only"]) as read_only,
        ZoeServer(str(tmp_path / "writable")) as writable,
    ):
        # The read-only server comes first, and accepts a read-only client.
        storage = native([read_only.address, writable.address], read_only_fallback=True)
        wait_for(lambda: not storage.isReadOnly())
        oid, tid = commit_and_load(storage)
        assert not storage.isReadOnly()
        # The record is in the store of the writable server.
        check = native(writable.address)
        assert check.load(oid) == (b"native", tid)
