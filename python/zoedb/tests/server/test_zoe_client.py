"""Unit tests of `zoe.zeo.ClientStorage` and of the `<zoeclient>` section.

The acceptance test covers the sequences over the protocol, a stock client,
a stock server and the section that opens the client. These tests cover the
rest: the keys of the section, its `<ssl>` section, the read-only fallback
with the declaration, a name that is not a str, the switch to the native
client, the arguments of TLS and of the addresses of the native client, and
the traceparent that the native client reads.
"""

import io
import socket
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


def test_the_native_tls_refuses_what_it_does_not_read(tmp_path):
    from ZODB.POSException import Unsupported

    import zoe.zeo

    refused = {
        "password-function": ssl_section(password_function="m.f"),
        "authenticate": ssl_section(authenticate=None),
        "directory": ssl_section(authenticate=str(tmp_path)),
    }
    for match, section in refused.items():
        with pytest.raises(Unsupported, match=match):
            zoe.zeo._native_tls(section, None)


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


def test_the_native_client_declares_the_extensions_of_the_client_of_zeo():
    import zoe.zeo

    assert zoe.zeo._NATIVE_EXTENSIONS == zoe.zeo.EXTENSIONS
    traces = "trace_context" in zoe.zeo._NATIVE_EXTENSIONS
    assert traces == (zoe.zeo._PROPAGATOR is not None)


def native(server, extensions, traceparent):
    """Open a `zoe._zoe.Client` that declares `extensions`."""
    from zoe import _zoe

    return _zoe.Client(
        [tuple(server.address)],
        "1",
        read_only=False,
        read_only_fallback=False,
        extensions=extensions,
        wait_timeout=float(TIMEOUT),
        traceparent=traceparent,
    )


def test_the_native_client_reads_the_traceparent_only_when_it_traces(tmp_path):
    traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    reads = []

    def read():
        reads.append(None)
        return traceparent

    with ZoeServer(str(tmp_path / "store")) as server:
        # The server accepted the extension: each call reads the
        # traceparent once, when it starts.
        traced = native(server, ["trace_context"], read)
        try:
            assert traced.extensions() == ["trace_context"]
            assert len(traced.new_oids(1)) == 1
            traced.begin(b"", b"", {})
            traced.abort()
            assert len(reads) == 3
        finally:
            traced.close()
        # Without the declaration, no call reads the traceparent.
        reads.clear()
        plain = native(server, ["sequences"], read)
        try:
            assert len(plain.new_oids(1)) == 1
            plain.begin(b"", b"", {})
            plain.abort()
            assert reads == []
        finally:
            plain.close()


def test_a_traceparent_that_is_not_a_str_raises_type_error(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        client = native(server, ["trace_context"], lambda: 1)
        try:
            with pytest.raises(TypeError):
                client.new_oids(1)
        finally:
            client.close()


def test_native_on_another_subclass_raises_type_error():
    import zoe.zeo

    class Mine(zoe.zeo.ClientStorage):
        pass

    with pytest.raises(TypeError, match="native=True"):
        Mine(("127.0.0.1", 1), native=True)
