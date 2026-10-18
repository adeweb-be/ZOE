"""The counters of the sampler that give the store bytes of a server."""

import os
import shutil
import socket
import subprocess
import sys
import threading

import pytest

from plonebench import sampler

linux = pytest.mark.skipif(sys.platform == "darwin", reason="Linux counters")


def test_sent_bytes_keep_the_count_of_a_closed_socket():
    sent = sampler.SentBytes()
    assert sent.update({1: 100, 2: 50}) == 150
    assert sent.update({1: 300, 2: 80}) == 380
    # Socket 2 closes: its last count stays.
    assert sent.update({1: 400}) == 480
    # A new socket opens with bytes that it sent before the sample.
    assert sent.update({1: 400, 3: 20}) == 500


def test_sent_bytes_take_a_smaller_count_as_a_new_socket():
    sent = sampler.SentBytes()
    assert sent.update({1: 1000}) == 1000
    assert sent.update({1: 10}) == 1010


@linux
def test_other_files_are_the_written_files_outside_the_store(tmp_path):
    store = tmp_path / "store"
    store.mkdir()
    log = tmp_path / "server.log"
    with (
        open(store / "zoe.redb", "wb") as data,
        open(log, "wb") as out,
        open(log, "ab") as again,
        open(tmp_path / "read", "wb") as read,
    ):
        data.write(b"d" * 5000)
        data.flush()
        read.write(b"r" * 700)
        read.flush()
        with open(tmp_path / "read", "rb"):
            before = sampler.read_other_files(os.getpid(), store)
            out.write(b"x" * 300)
            out.flush()
            again.write(b"y" * 200)
            again.flush()
            after = sampler.read_other_files(os.getpid(), store)
    # The log counts once, though two descriptors hold it.
    assert after - before == 500


@linux
@pytest.mark.skipif(shutil.which("ss") is None, reason="needs ss")
def test_the_sockets_of_a_process_give_their_sent_bytes():
    listener = socket.create_server(("127.0.0.1", 0))
    client = socket.create_connection(listener.getsockname())
    server, _ = listener.accept()

    received = []

    def drain():
        while sum(received) < 100_000:
            received.append(len(client.recv(1 << 16)))

    try:
        before = sum(sampler.read_sockets(os.getpid()).values())
        reader = threading.Thread(target=drain)
        reader.start()
        server.sendall(b"z" * 100_000)
        # The peer has every byte, so the socket sent them all.
        reader.join()
        after = sum(sampler.read_sockets(os.getpid()).values())
    finally:
        server.close()
        client.close()
        listener.close()
    assert after - before == 100_000


@linux
def test_the_sampler_reads_the_store_counters_of_a_server(tmp_path):
    store = tmp_path / "Data.fs"
    store.write_bytes(b"s" * 1234)
    sample = sampler.Sampler(tmp_path / "samples.jsonl", None)
    sample.watch("server", os.getpid(), store, appends=True)
    sample.watch("server", os.getpid())
    sample.watch("locust", os.getpid())
    processes = sample.sample()["processes"]
    assert processes["server"]["store_file_bytes"] == 1234
    assert "other_files_bytes" in processes["server"]
    assert "store_file_bytes" not in processes["locust"]


@linux
def test_the_sent_bytes_leave_out_retransmits_and_add_the_queue(monkeypatch):
    with socket.socket() as sock:
        inode = os.fstat(sock.fileno()).st_ino
        line = (
            f"ESTAB 0 0 127.0.0.1:1 127.0.0.1:2 ino:{inode} sk:1 <-> cubic "
            "bytes_sent:1000 bytes_retrans:100 bytes_acked:900 notsent:50"
        )
        monkeypatch.setattr(
            sampler.subprocess,
            "run",
            lambda *args, **kwargs: subprocess.CompletedProcess(args, 0, line, ""),
        )
        assert sampler.read_sockets(os.getpid()) == {inode: 950}


@linux
def test_other_files_skip_a_store_behind_a_symlink(tmp_path):
    real = tmp_path / "real"
    real.mkdir()
    link = tmp_path / "link"
    link.symlink_to(real)
    with open(link / "zoe.redb", "wb") as data:
        before = sampler.read_other_files(os.getpid(), link)
        data.write(b"d" * 5000)
        data.flush()
        assert sampler.read_other_files(os.getpid(), link) == before


def test_zeo_store_bytes_need_the_size_of_data_fs_at_both_ends():
    def server(**counters):
        return {
            "cpu": 0.0,
            "user": 0.0,
            "system": 0.0,
            "threads": 1,
            "rss": 0,
            "run_delay": 0.0,
            **counters,
        }

    samples = [
        {
            "t": float(t),
            "cpu": {"user": 0.0},
            "disk": None,
            "processes": {"server": entry},
        }
        for t, entry in enumerate(
            [
                server(
                    wchar=0,
                    socket_bytes_sent=0,
                    other_files_bytes=0,
                    store_file_bytes=None,
                ),
                server(
                    wchar=900,
                    socket_bytes_sent=100,
                    other_files_bytes=0,
                    store_file_bytes=500,
                ),
            ]
        )
    ]
    result = sampler.summarize(samples, 0.0, 1.0)["processes"]["server"]
    assert "store_write_bytes" not in result
