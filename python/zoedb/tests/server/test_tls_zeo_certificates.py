"""Acceptance test: `zoe serve` accepts the TLS certificates of ZEO's tests.

ZEO's tests authenticate with self-signed certificates that are CA
certificates (`CA:TRUE`): `client.pem` is the client certificate and also
the file of the key `authenticate` of the server. OpenSSL accepts a client
certificate that is in that file. rustls refuses a CA certificate as a
client certificate.

So `zoe serve` accepts a client certificate that is byte for byte one of
the certificates of `--tls-client-ca`, also when it is a CA certificate.
The client still proves its key in the handshake, and the certificate must
be within its validity period. No other CA certificate is accepted as a
client certificate: not one that chains to a certificate of the file, and
not one of `--tls-read-only-client-ca`. A certificate that is not a CA
certificate and that chains to the file is accepted, as today.

The test uses the certificates of `ZEO.tests` and makes the others with the
`openssl` command, so the repository holds no key.
"""

import ssl
import subprocess

import pytest

from zoe.testing import ZoeServer

TIMEOUT = 30


def zeo_certificates():
    from ZEO.tests import testssl

    return testssl


def openssl(*args, cwd):
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


@pytest.fixture(scope="module")
def certs(tmp_path_factory):
    """Make the certificates that ZEO's tests do not have.

    - `other-ca`: a self-signed CA certificate that is in no file.
    - `sub-ca`: a CA certificate signed by the key of ZEO's `client.pem`.
    - `leaf`: a client certificate, not a CA, signed by the same key.
    """
    zeo = zeo_certificates()
    directory = tmp_path_factory.mktemp("certs")
    openssl(
        "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", "/CN=other-ca", "-keyout", "other-ca.key", "-out", "other-ca.pem",
        "-addext", "basicConstraints = critical, CA:TRUE",
        "-addext", "keyUsage = critical, keyCertSign, cRLSign, digitalSignature",
        cwd=directory,
    )  # fmt: skip
    extensions = {
        "sub-ca": "basicConstraints = critical, CA:TRUE\n"
        "keyUsage = critical, keyCertSign, cRLSign, digitalSignature\n",
        "leaf": "basicConstraints = critical, CA:FALSE\n"
        "extendedKeyUsage = clientAuth\n",
    }
    for name, text in extensions.items():
        openssl(
            "req", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={name}",
            "-keyout", f"{name}.key", "-out", f"{name}.csr",
            cwd=directory,
        )  # fmt: skip
        (directory / f"{name}.ext").write_text(text)
        openssl(
            "x509", "-req", "-in", f"{name}.csr", "-CA", zeo.client_cert,
            "-CAkey", zeo.client_key, "-CAcreateserial",
            "-CAserial", str(directory / "zeo-client.srl"), "-days", "2",
            "-extfile", f"{name}.ext", "-out", f"{name}.pem",
            cwd=directory,
        )  # fmt: skip
    return directory


def tls_args(client_ca=None, read_only_client_ca=None):
    zeo = zeo_certificates()
    args = ["--tls-cert", zeo.server_cert, "--tls-key", zeo.server_key]
    if client_ca:
        args += ["--tls-client-ca", str(client_ca)]
    if read_only_client_ca:
        args += ["--tls-read-only-client-ca", str(read_only_client_ca)]
    return args


def context(cert, key):
    """Return the client context of ZEO's tests, with another certificate."""
    zeo = zeo_certificates()
    ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH, cafile=zeo.server_cert)
    ctx.load_cert_chain(str(cert), str(key))
    ctx.check_hostname = False
    return ctx


def client(server, ctx, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", ssl=ctx, **kw)


def commit_and_load(storage):
    from ZODB.Connection import TransactionMetaData

    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, bytes(8), b"tls", "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    assert storage.load(oid) == (b"tls", tid)


def assert_refused(server, ctx):
    from ZEO.Exceptions import ClientDisconnected

    with pytest.raises(ClientDisconnected):
        client(server, ctx, wait_timeout=2)


def test_the_client_certificate_of_zeos_tests_commits(tmp_path):
    zeo = zeo_certificates()
    args = tls_args(client_ca=zeo.client_cert)
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        # The context of ZEO's own tests: client.pem with its key.
        storage = client(server, zeo.client_ssl())
        try:
            commit_and_load(storage)
        finally:
            storage.close()


def test_a_ca_certificate_that_is_in_no_file_is_refused(tmp_path, certs):
    zeo = zeo_certificates()
    args = tls_args(client_ca=zeo.client_cert)
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        assert_refused(server, context(certs / "other-ca.pem", certs / "other-ca.key"))
        # The server goes on.
        storage = client(server, zeo.client_ssl())
        storage.close()


def test_a_ca_certificate_that_chains_to_the_file_is_refused(tmp_path, certs):
    zeo = zeo_certificates()
    args = tls_args(client_ca=zeo.client_cert)
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        assert_refused(server, context(certs / "sub-ca.pem", certs / "sub-ca.key"))


def test_a_certificate_that_is_not_a_ca_and_chains_to_the_file_commits(tmp_path, certs):
    zeo = zeo_certificates()
    args = tls_args(client_ca=zeo.client_cert)
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        storage = client(server, context(certs / "leaf.pem", certs / "leaf.key"))
        try:
            commit_and_load(storage)
        finally:
            storage.close()


def test_a_ca_certificate_of_the_read_only_file_is_refused(tmp_path, certs):
    zeo = zeo_certificates()
    args = tls_args(
        client_ca=certs / "other-ca.pem", read_only_client_ca=zeo.client_cert
    )
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        assert_refused(server, zeo.client_ssl())
