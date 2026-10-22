"""The quickstarts of the user guide run as their pages show them.

`docs/guide/in-process.md` and `docs/guide/server.md` mark the blocks of
their quickstarts with a word after the language of the fence:
```` ```sh test:serve ````. Each test reads its blocks from the page, so a
page and its test cannot drift:

- `test:zope-conf` is the `<zoe>` section of `zope.conf`. The schema of
  Zope reads it, the database of its mount point `/` opens, and a commit
  and a read go through it.
- `test:serve`, `test:serve-config` and `test:serve-zconfig` start `zoe
  serve`: with its options, with the configuration file `test:zoe.toml`,
  and through `python -m zoe.serve` with `test:zoe-server.conf`. A block
  whose name is a file name is that file. `zoe` is the binary of
  `ZOE_BINARY`, and `python` is this interpreter.
- `test:readyz` is a request to the admin port, which the test sends
  with urllib, and `test:client` is a stock ZEO client that commits and
  reads an object, which the test runs in a new process.

Each test runs in a new directory, so the relative paths of the pages go
there. The pages give fixed ports. The test replaces each port of a listen
address with 0, so the server takes free ports and runs beside other
servers, and gives the client the ports that the server writes on stdout.
The server gets no `ZOE_*` variable of the caller. On Linux, it ends when
the test process ends (`_die_with_parent`).
"""

import io
import os
import re
import shlex
import signal
import subprocess
import sys
import threading
import urllib.request
from pathlib import Path

import pytest
from ZConfig.loader import ConfigLoader

from zoe.testing import ZoeServer, _die_with_parent

ROOT = Path(__file__).resolve().parents[3]
GUIDE = ROOT / "docs" / "guide"

# The longest wait for the server and for the client, in seconds.
TIMEOUT = 60

# A fence whose info string holds `test:NAME` after the language. The
# fence can be indented, in a list item.
FENCE = re.compile(
    r"^(?P<indent>[ \t]*)```[\w.+-]*[ \t]+test:(?P<name>\S+)[ \t]*\n"
    r"(?P<body>.*?)^(?P=indent)```[ \t]*$",
    re.M | re.S,
)

# A listen address of a page: a loopback or wildcard host and a port.
ADDRESS = re.compile(r"\b(127\.0\.0\.1|0\.0\.0\.0):\d+\b")


def blocks(page):
    """Return the marked blocks of the guide page `page`, by name."""
    path = GUIDE / page
    text = path.read_text(encoding="utf-8") if path.is_file() else ""
    found = {}
    for match in FENCE.finditer(text):
        indent = match["indent"]
        lines = match["body"].splitlines()
        found[match["name"]] = "".join(
            line.removeprefix(indent) + "\n" for line in lines
        )
    return found


def block(page, name):
    """Return the block `test:name` of the guide page `page`."""
    found = blocks(page)
    assert name in found, f"docs/guide/{page} has no block test:{name}"
    return found[name]


def free_ports(text):
    """Return `text` with the port of each listen address set to 0."""
    return ADDRESS.sub(r"\1:0", text)


def command(text):
    """Return the argument list of the one command of a shell block."""
    lines = [line for line in text.splitlines() if line.strip()]
    assert len(lines) == 1, f"the block holds more than one command: {text!r}"
    args = shlex.split(free_ports(lines[0]))
    if args[0] == "zoe":
        args[0] = ZoeServer.binary()
    elif args[0] == "python":
        args[0] = sys.executable
    return args


def environment():
    """Return the environment of the server: no `ZOE_*` of the caller, but
    `ZOE_BINARY`, which `python -m zoe.serve` runs."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    env["ZOE_BINARY"] = ZoeServer.binary()
    return env


class Server:
    """A server that a block of the guide starts, in the directory `cwd`."""

    def __init__(self, args, cwd):
        self.process = subprocess.Popen(
            args,
            cwd=cwd,
            env=environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            text=True,
            preexec_fn=_die_with_parent(),
        )
        self.admin = self.listening = None
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, self.process.kill)
        timer.start()
        try:
            for line in self.process.stdout:
                word, _, value = line.strip().partition(" ")
                if word == "admin":
                    self.admin = value
                elif word == "listening":
                    self.listening = value
                    break
        finally:
            timer.cancel()
        assert self.listening, "the server wrote no listening line"

    def port(self):
        return int(self.listening.rpartition(":")[2])

    def stop(self):
        """Stop the server with SIGTERM, and return its exit code."""
        self.process.send_signal(signal.SIGTERM)
        try:
            return self.process.wait(TIMEOUT)
        finally:
            self.close()

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()
        self.process.stdout.close()


@pytest.fixture
def start(tmp_path):
    """Start the servers of a test in `tmp_path`, and kill each one at the
    end."""
    servers = []

    def start(text):
        server = Server(command(text), tmp_path)
        servers.append(server)
        return server

    yield start
    for server in servers:
        server.close()


def check_ready(server):
    """Send the request of `test:readyz` to the admin port of `server`."""
    url = command(block("server.md", "readyz"))[-1]
    url = ADDRESS.sub(server.admin, url)
    with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
        assert reply.status == 200
        assert reply.read().decode() == "1: ready\n"


def check_client(server, cwd):
    """Run `test:client` against `server`: it commits and reads an object."""
    code = block("server.md", "client")
    page_port = ADDRESS.search(block("server.md", "serve"))[0].rpartition(":")[2]
    assert page_port in code, "the client does not use the port of the server"
    code = re.sub(rf"\b{page_port}\b", str(server.port()), code)
    result = subprocess.run(
        [sys.executable, "-c", code],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout == "hello\n"


def test_the_zoe_section_of_zope_conf_opens_a_store(tmp_path, monkeypatch):
    from Zope2.Startup.options import ZopeWSGIOptions

    section = block("in-process.md", "zope-conf")
    monkeypatch.chdir(tmp_path)
    home = tmp_path / "instance"
    home.mkdir()
    # The page gives the storage of zope.conf. The schema of Zope also
    # needs an instance home.
    text = f"{section}instancehome {home}\n"
    # Zope loads its schema and zope.conf this way.
    options = ZopeWSGIOptions()
    options.load_schema()
    config, _ = ConfigLoader(options.schema).loadFile(io.StringIO(text))
    db = config.dbtab.getDatabase("/")
    try:
        assert type(db.storage).__module__ == "zoe.storage"
        with db.transaction() as connection:
            connection.root()["greeting"] = "hello"
    finally:
        db.close()
    assert (tmp_path / "var" / "zoe" / "zoe.redb").is_file()

    # A new start reads the commit. The table of databases keeps the
    # database that it opened, so the start loads the file again.
    config, _ = ConfigLoader(options.schema).loadFile(io.StringIO(text))
    db = config.dbtab.getDatabase("/")
    try:
        with db.transaction() as connection:
            assert connection.root()["greeting"] == "hello"
    finally:
        db.close()


def test_zoe_serve_runs_with_the_options_of_the_quickstart(tmp_path, start):
    server = start(block("server.md", "serve"))
    assert server.admin, "the server wrote no admin line"
    check_ready(server)
    check_client(server, tmp_path)
    assert server.stop() == 0
    assert (tmp_path / "var" / "zoe" / "zoe.redb").is_file()


def test_zoe_serve_runs_with_the_configuration_file(tmp_path, start):
    (tmp_path / "zoe.toml").write_text(free_ports(block("server.md", "zoe.toml")))
    server = start(block("server.md", "serve-config"))
    check_ready(server)
    check_client(server, tmp_path)
    assert server.stop() == 0


def test_zoe_serve_runs_with_a_zoe_server_section(tmp_path, start):
    text = free_ports(block("server.md", "zoe-server.conf"))
    (tmp_path / "zoe-server.conf").write_text(text)
    server = start(block("server.md", "serve-zconfig"))
    check_ready(server)
    check_client(server, tmp_path)
    assert server.stop() == 0
