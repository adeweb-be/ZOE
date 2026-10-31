"""Acceptance test of TLS on the ZEO port of `zoe serve`.

`--tls-cert FILE` and `--tls-key FILE` (`ZOE_TLS_CERT`, `ZOE_TLS_KEY`) make
the server speak TLS with rustls, so the `ssl` option of an unmodified
`ZEO.ClientStorage` works against it. `--tls-client-ca FILE`
(`ZOE_TLS_CLIENT_CA`) requires a client certificate signed by that CA, as
the `authenticate` key of ZEO's `<ssl>` section does.
`--tls-read-only-client-ca FILE` (`ZOE_TLS_READ_ONLY_CLIENT_CA`) also
accepts the certificates of a second CA, and a client with such a
certificate registers read-only only: this is how Loupe and the MCP server
get a read-only or a read-write certificate.

A failed handshake closes that connection only. The test makes its own
certificates with the `openssl` command, so the repository holds no key.
"""

import ssl

import pytest
import support
from support import ZERO, commit, make_ca, make_cert, record

from zoe.testing import ZoeServer


@pytest.fixture(scope="module")
def certs(tmp_path_factory):
    directory = tmp_path_factory.mktemp("certs")
    for ca in ["server-ca", "rw-ca", "ro-ca"]:
        make_ca(directory, ca)
    make_cert(directory, "server", "server-ca", server=True)
    make_cert(directory, "writer", "rw-ca")
    make_cert(directory, "reader", "ro-ca")
    make_cert(directory, "stranger", "server-ca")
    return directory


def context(certs, client=None):
    ctx = ssl.create_default_context(
        ssl.Purpose.SERVER_AUTH, cafile=str(certs / "server-ca.pem")
    )
    if client:
        ctx.load_cert_chain(str(certs / f"{client}.pem"), str(certs / f"{client}.key"))
    return ctx


def client(server, ctx=None, **kw):
    """Open a stock client, over TLS with the context `ctx`."""
    if ctx is not None:
        kw["ssl"] = ctx
        kw["ssl_server_hostname"] = "localhost"
    return support.client(server, **kw)


def tls_args(certs, *extra):
    return [
        "--tls-cert",
        str(certs / "server.pem"),
        "--tls-key",
        str(certs / "server.key"),
        *extra,
    ]


def test_a_stock_client_commits_over_tls(tmp_path, certs):
    with ZoeServer(str(tmp_path / "store"), args=tls_args(certs)) as server:
        storage = client(server, context(certs))
        try:
            oid = storage.new_oid()
            tid = commit(storage, [(oid, ZERO, record("tls"))])
            assert storage.load(oid) == (record("tls"), tid)
        finally:
            storage.close()


def test_a_plain_client_is_refused_and_the_server_goes_on(tmp_path, certs):
    from ZEO.Exceptions import ClientDisconnected

    with ZoeServer(str(tmp_path / "store"), args=tls_args(certs)) as server:
        with pytest.raises(ClientDisconnected):
            client(server, wait_timeout=1)
        storage = client(server, context(certs))
        storage.close()


def test_a_client_certificate_is_required_with_a_client_ca(tmp_path, certs):
    from ZEO.Exceptions import ClientDisconnected

    args = tls_args(certs, "--tls-client-ca", str(certs / "rw-ca.pem"))
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        with pytest.raises(ClientDisconnected):
            client(server, context(certs), wait_timeout=1)
        with pytest.raises(ClientDisconnected):
            client(server, context(certs, "stranger"), wait_timeout=1)
        storage = client(server, context(certs, "writer"))
        try:
            commit(storage, [(storage.new_oid(), ZERO, record("w"))])
        finally:
            storage.close()


def test_a_read_only_certificate_registers_read_only(tmp_path, certs):
    from ZEO.Exceptions import ClientDisconnected
    from ZODB.POSException import ReadOnlyError

    args = tls_args(
        certs,
        "--tls-client-ca",
        str(certs / "rw-ca.pem"),
        "--tls-read-only-client-ca",
        str(certs / "ro-ca.pem"),
    )
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        writer = client(server, context(certs, "writer"))
        try:
            oid = writer.new_oid()
            tid = commit(writer, [(oid, ZERO, record("w"))])
        finally:
            writer.close()

        # Without fallback, a read-write registration is refused.
        with pytest.raises(ClientDisconnected):
            client(server, context(certs, "reader"), wait_timeout=1)

        reader = client(server, context(certs, "reader"), read_only_fallback=True)
        try:
            assert reader.isReadOnly()
            assert reader.load(oid) == (record("w"), tid)
            with pytest.raises(ReadOnlyError):
                reader._call("new_oids", 1)
        finally:
            reader.close()
