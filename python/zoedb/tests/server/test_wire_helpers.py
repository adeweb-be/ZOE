"""Unit tests of the recorder and the stock-server helper."""

import socket
import struct
import threading

import msgpack
import pytest

from zoe.testing.wire import Recorder, stock_server


def frame(payload):
    return struct.pack(">I", len(payload)) + payload


def receive(sock, size):
    data = b""
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        assert chunk
        data += chunk
    return data


def test_the_recorder_decodes_and_forwards_each_frame():
    listener = socket.create_server(("127.0.0.1", 0))
    reply = msgpack.packb((1, 0, ".reply", None), use_bin_type=True)

    def serve():
        conn, _ = listener.accept()
        with conn:
            conn.sendall(frame(b"M5"))
            receive(conn, 4 + 2)
            size = struct.unpack(">I", receive(conn, 4))[0]
            receive(conn, size)
            conn.sendall(frame(reply))

    thread = threading.Thread(target=serve)
    thread.start()
    call = msgpack.packb((1, False, "ping", ()), use_bin_type=True)
    with Recorder(listener.getsockname()) as recorder:
        with socket.create_connection(recorder.address) as sock:
            assert receive(sock, 6) == frame(b"M5")
            sock.sendall(frame(b"M5") + frame(call))
            assert receive(sock, 4 + len(reply)) == frame(reply)
        thread.join(10)
        messages = recorder.messages
    listener.close()
    assert [(m.direction, m.method) for m in messages] == [
        ("server", None),
        ("client", None),
        ("client", "ping"),
        ("server", ".reply"),
    ]
    assert messages[2].id == 1 and messages[2].flags is False
    assert messages[2].args == ()
    assert messages[3].raw == reply


def test_the_stock_server_refuses_an_unknown_option(tmp_path):
    with pytest.raises(TypeError):
        with stock_server(tmp_path, nonsense=True):
            pass


def test_the_stock_server_announces_msgpack(tmp_path):
    with stock_server(tmp_path) as address:
        with socket.create_connection(address, timeout=30) as sock:
            assert receive(sock, 6) == frame(b"M5")
