"""Unit tests of `zoe.zeo.ClientStorage` and of the `<zoeclient>` section.

The acceptance test covers the sequences over the protocol, a stock client,
a stock server and the section that opens the client. These tests cover the
rest: the keys of the section, its `<ssl>` section, the read-only fallback
with the declaration, a name that is not a str, the switch to the native
client, and the arguments of TLS and of the addresses of the native client.
They also cover the CA certificates of the native client: a directory as
`authenticate`, and those of the system without `authenticate`.
"""

import io
import shutil
import socket
import subprocess
import types

import pytest
import ZConfig
from ZODB.POSException import ReadOnlyError

import zoe
from zoe.testing import ZoeServer

TIMEOUT = 30

SCHEMA = """
<schema>
  <import package="ZODB"/>
  <import package="zoe"/>
  <section type="ZODB.storage" name="*" attribute="storage"/>
</schema>
"""


def client(server, **kw):
    import zoe.zeo

    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.zeo.ClientStorage(server.address, storage="1", **kw)


def keys(schema, name):
    return {key for key, info in schema.gettype(name) if not info.issection()}


def test_the_zoeclient_section_takes_every_key_of_zeoclient():
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    assert keys(schema, "zoeclient") == keys(schema, "zeoclient") | {
        "wait-timeout",
        "native",
    }


def test_the_ssl_section_takes_the_keys_of_the_ssl_section_of_zeo():
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    assert keys(schema, "ssl") == {
        "certificate",
        "key",
        "password-function",
        "authenticate",
        "check-hostname",
        "server-hostname",
    }


def test_the_ssl_section_of_zoeclient_keeps_its_files(tmp_path):
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    text = (
        "%import zoe\n<zoeclient>\nserver 127.0.0.1:1\n"
        f"<ssl>\ncertificate {tmp_path / 'c.pem'}\n"
        f"authenticate {tmp_path / 'ca.pem'}\nserver-hostname localhost\n"
        "</ssl>\n</zoeclient>\n"
    )
    config, _ = ZConfig.loadConfigFile(schema, io.StringIO(text))
    ssl = config.storage.config.ssl
    assert ssl.certificate == str(tmp_path / "c.pem")
    assert ssl.key is None
    assert ssl.authenticate == str(tmp_path / "ca.pem")
    assert ssl.check_hostname is True
    assert ssl.server_hostname == "localhost"
    assert ssl.password_function is None


def test_a_read_only_fallback_keeps_the_declaration(tmp_path):
    path = str(tmp_path / "store")
    zoe.Storage(path).close()
    with ZoeServer(path, args=["--read-only"]) as server:
        storage = client(server, read_only_fallback=True)
        try:
            assert storage.isReadOnly()
            assert "sequences" in storage._info["extensions"]
            with pytest.raises(ReadOnlyError):
                storage.sequence("intids").next()
        finally:
            storage.close()


def test_the_name_of_a_sequence_is_a_str(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        storage = client(server)
        try:
            with pytest.raises(TypeError):
                storage.sequence(b"intids")
            assert storage.sequence("intids").next() == 1
        finally:
            storage.close()


def test_the_native_key_opens_the_native_client(tmp_path):
    from ZEO.ClientStorage import ClientStorage

    import zoe.zeo

    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    with ZoeServer(str(tmp_path / "store")) as server:
        host, port = server.address
        for native in ("true", "false"):
            text = (
                f"%import zoe\n<zoeclient>\nserver {host}:{port}\n"
                f"native {native}\n</zoeclient>\n"
            )
            config, _ = ZConfig.loadConfigFile(schema, io.StringIO(text))
            storage = config.storage.open()
            try:
                assert isinstance(storage, ClientStorage)
                assert isinstance(storage, zoe.zeo.ClientStorage)
                is_native = isinstance(storage, zoe.zeo._NativeClientStorage)
                assert is_native == (native == "true")
            finally:
                storage.close()


def test_the_native_client_is_off_by_default(tmp_path):
    import zoe.zeo

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = client(server)
        try:
            assert not isinstance(storage, zoe.zeo._NativeClientStorage)
        finally:
            storage.close()


def test_the_native_client_ignores_what_zeo_ignores(tmp_path):
    import zoe.testing

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address,
            storage="1",
            wait_timeout=TIMEOUT,
            min_disconnect_poll=5,
            max_disconnect_poll=300,
            drop_cache_rather_verify=False,
        )
        try:
            assert storage._addr == [server.address]
            assert storage.sequence("a").next() == 1
        finally:
            storage.close()


def test_the_ssl_section_gives_zeos_client_a_context_without_native(tmp_path):
    from unittest import mock

    import ZEO.ClientStorage

    import zoe.zeo

    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    certificate, key = str(tmp_path / "c.pem"), str(tmp_path / "k.pem")
    text = (
        "%import zoe\n<zoeclient>\nserver 127.0.0.1:1\nnative false\n"
        f"<ssl>\ncertificate {certificate}\nkey {key}\n"
        f"authenticate {tmp_path / 'ca.pem'}\nserver-hostname db\n"
        "</ssl>\n</zoeclient>\n"
    )
    config, _ = ZConfig.loadConfigFile(schema, io.StringIO(text))
    # As the tests of ZEO.tests.testssl, the test mocks the context and the
    # client of ZEO.
    with (
        mock.patch("ssl.create_default_context") as factory,
        mock.patch.object(ZEO.ClientStorage.ClientStorage, "__init__") as init,
    ):
        init.return_value = None
        storage = config.storage.open()
    assert not isinstance(storage, zoe.zeo._NativeClientStorage)
    given = init.call_args.kwargs
    context = factory.return_value
    assert given["ssl"] is context
    assert given["ssl_server_hostname"] == "db"
    context.load_cert_chain.assert_called_with(certificate, key, None)
    assert context.check_hostname is True


def test_an_ssl_context_fails_the_native_open_and_names_it():
    import ssl

    from ZODB.POSException import Unsupported

    import zoe.testing

    context = ssl.create_default_context()
    with pytest.raises(Unsupported, match="ssl context"):
        zoe.testing.native_client(("127.0.0.1", 1), ssl=context, wait_timeout=1)


def ssl_section(**keys):
    """Return an object with the attributes of an `<ssl>` section."""
    section = {
        "certificate": "c.pem",
        "key": None,
        "password_function": None,
        "authenticate": "ca.pem",
        "check_hostname": True,
        "server_hostname": None,
    }
    section.update(keys)
    return types.SimpleNamespace(**section)


def test_the_native_tls_takes_the_files_of_the_ssl_section(tmp_path, monkeypatch):
    import zoe.zeo

    monkeypatch.chdir(tmp_path)
    tls = zoe.zeo._native_tls(ssl_section(), None)
    assert tls == (
        str(tmp_path / "c.pem"),
        None,
        str(tmp_path / "ca.pem"),
        True,
        None,
    )
    tls = zoe.zeo._native_tls(
        ssl_section(key="k.pem", check_hostname=False, server_hostname="a"), None
    )
    assert tls[1] == str(tmp_path / "k.pem")
    assert tls[3:] == (False, "a")
    # The argument ssl_server_hostname comes before the section.
    tls = zoe.zeo._native_tls(ssl_section(server_hostname="a"), "b")
    assert tls[4] == "b"


def test_the_native_tls_refuses_an_encrypted_key():
    from ZODB.POSException import Unsupported

    import zoe.zeo

    with pytest.raises(Unsupported, match="password-function"):
        zoe.zeo._native_tls(ssl_section(password_function="m.f"), None)


def test_the_native_tls_takes_a_directory_or_the_system_store(tmp_path):
    import zoe.zeo

    tls = zoe.zeo._native_tls(ssl_section(authenticate=str(tmp_path)), None)
    assert tls[2] == str(tmp_path)
    # As for ZEO, no authenticate and DYNAMIC take the system store.
    for authenticate in [None, "", "DYNAMIC"]:
        tls = zoe.zeo._native_tls(ssl_section(authenticate=authenticate), None)
        assert tls[2] is None, authenticate


def openssl(*args, cwd):
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


NEW_KEY = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes"]


def sign(directory, name, ca, extensions):
    """Make the certificate `name`, signed by the CA `ca`."""
    openssl(
        "req", *NEW_KEY, "-subj", f"/CN={name}",
        "-keyout", f"{name}.key", "-out", f"{name}.csr",
        cwd=directory,
    )  # fmt: skip
    (directory / f"{name}.ext").write_text(extensions)
    openssl(
        "x509", "-req", "-in", f"{name}.csr", "-CA", f"{ca}.pem",
        "-CAkey", f"{ca}.key", "-CAcreateserial", "-days", "2",
        "-extfile", f"{name}.ext", "-out", f"{name}.pem",
        cwd=directory,
    )  # fmt: skip


@pytest.fixture(scope="module")
def certs(tmp_path_factory):
    """`server-ca` signs the server, `client-ca` signs the client, and
    `other-ca` signs nothing."""
    directory = tmp_path_factory.mktemp("certs")
    for ca in ["server-ca", "client-ca", "other-ca"]:
        openssl(
            "req", "-x509", *NEW_KEY, "-days", "2", "-subj", f"/CN={ca}",
            "-keyout", f"{ca}.key", "-out", f"{ca}.pem",
            "-addext", "keyUsage = critical, keyCertSign, cRLSign",
            cwd=directory,
        )  # fmt: skip
    sign(directory, "server", "server-ca", "subjectAltName = DNS:localhost\n")
    sign(directory, "client", "client-ca", "extendedKeyUsage = clientAuth\n")
    return directory


@pytest.fixture
def tls_server(tmp_path, certs):
    """A server with TLS that requires a certificate of `client-ca`."""
    args = [
        "--tls-cert", str(certs / "server.pem"),
        "--tls-key", str(certs / "server.key"),
        "--tls-client-ca", str(certs / "client-ca.pem"),
    ]  # fmt: skip
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        yield server


def open_tls(server, certs, authenticate, wait_timeout=TIMEOUT):
    """Open a native client of `server` with the CA certificates of
    `authenticate`, and check its connection. Close it."""
    import zoe.testing

    section = ssl_section(
        certificate=str(certs / "client.pem"),
        key=str(certs / "client.key"),
        authenticate=authenticate,
        server_hostname="localhost",
    )
    storage = zoe.testing.native_client(
        server.address,
        storage="1",
        wait_timeout=wait_timeout,
        _ssl_section=section,
    )
    try:
        assert len(storage.new_oid()) == 8
    finally:
        storage.close()


def test_a_directory_of_ca_files_as_authenticate_checks_the_server(
    tmp_path, certs, tls_server
):
    from ZEO.Exceptions import ClientDisconnected

    good, other = tmp_path / "good", tmp_path / "other"
    for directory, cas in [(good, ["other-ca", "server-ca"]), (other, ["other-ca"])]:
        directory.mkdir()
        for ca in cas:
            shutil.copy(certs / f"{ca}.pem", directory)
    open_tls(tls_server, certs, str(good))
    with pytest.raises(ClientDisconnected):
        open_tls(tls_server, certs, str(other), wait_timeout=2)


def test_without_authenticate_the_client_checks_the_server_with_the_system_store(
    certs, tls_server, monkeypatch
):
    from ZEO.Exceptions import ClientDisconnected

    monkeypatch.delenv("SSL_CERT_DIR", raising=False)
    monkeypatch.setenv("SSL_CERT_FILE", str(certs / "server-ca.pem"))
    open_tls(tls_server, certs, None)
    # A CA that the system store does not hold fails the open.
    monkeypatch.setenv("SSL_CERT_FILE", str(certs / "other-ca.pem"))
    with pytest.raises(ClientDisconnected):
        open_tls(tls_server, certs, None, wait_timeout=2)


def test_an_empty_host_is_the_loopback_for_the_native_client():
    import zoe.zeo

    assert zoe.zeo._native_address(("", 8100)) == ("127.0.0.1", 8100)
    assert zoe.zeo._native_address(("db", 8100)) == ("db", 8100)
    assert zoe.zeo._native_address("./zeo.sock") == "./zeo.sock"


def test_a_native_open_without_a_server_raises_client_disconnected(tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    import zoe.testing

    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        closed = probe.getsockname()
    addresses = [closed, str(tmp_path / "zeo.sock")]
    with pytest.raises(ClientDisconnected, match="zeo.sock"):
        zoe.testing.native_client(addresses, wait_timeout=1)


def test_a_call_after_the_close_raises_client_disconnected(tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    import zoe.testing

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        storage.close()
        storage.close()
        with pytest.raises(ClientDisconnected):
            storage.new_oid()
        assert not storage.is_connected()


def test_a_read_only_native_client_refuses_writes(tmp_path):
    import zoe.testing

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT, read_only=True
        )
        try:
            assert storage.isReadOnly()
            with pytest.raises(ReadOnlyError):
                storage.new_oid()
            with pytest.raises(ReadOnlyError):
                storage.sequence("a").next()
        finally:
            storage.close()


def test_a_child_of_fork_refuses_a_client_of_its_parent(tmp_path):
    import os

    import zoe.testing

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        try:
            pid = os.fork()
            if pid == 0:
                code = 1
                try:
                    storage.new_oid()
                except RuntimeError:
                    code = 0
                finally:
                    os._exit(code)
            _, status = os.waitpid(pid, 0)
            assert os.waitstatus_to_exitcode(status) == 0
            # The parent still works.
            assert len(storage.new_oid()) == 8
        finally:
            storage.close()


def test_native_on_another_subclass_raises_type_error():
    import zoe.zeo

    class Mine(zoe.zeo.ClientStorage):
        pass

    with pytest.raises(TypeError, match="native=True"):
        Mine(("127.0.0.1", 1), native=True)
