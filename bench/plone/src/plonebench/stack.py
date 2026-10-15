"""The stacks under test: a storage server and the Zope instances on it.

Each stack is a storage server and a client section in `zope.conf`. The
runner writes `zope.conf` and `wsgi.ini` of each instance itself, from one
template, so every setting but the storage section is the same in every
stack. The eggs come from the buildout scenarios of `plone/`.
"""

from __future__ import annotations

import dataclasses
import glob
import json
import os
import shutil
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target")
PLONE = REPO / "target" / "plone"
WORK = REPO / "target" / "bench" / "plone"
PROBE = HERE / "probe"
INZOPE = HERE / "inzope"


@dataclass(frozen=True)
class StackConfig:
    """One configuration of the comparison."""

    name: str
    # The buildout scenario whose eggs the instances run.
    scenario: str
    # "zeo" (runzeo with a FileStorage) or "zoe" (zoe serve).
    server: str
    # The section of the storage in zope.conf.
    section: str
    # The key `native` of <zoeclient>, or None to leave it out.
    native: bool | None
    description: str
    # What differs from the baseline `zeo`.
    differs: tuple[str, ...] = ()


STACKS = {
    "zeo": StackConfig(
        name="zeo",
        scenario="zeo",
        server="zeo",
        section="zeoclient",
        native=None,
        description="ZEO 6 server (runzeo, FileStorage) and ZEO.ClientStorage",
        differs=(),
    ),
    "zoe-native": StackConfig(
        name="zoe-native",
        scenario="zoeclient",
        server="zoe",
        section="zoeclient",
        native=True,
        description=(
            "zoe serve and zoe.zeo.ClientStorage with the native client, "
            "zoedb[plone] installed: the intended production combination"
        ),
        differs=(
            "server: zoe serve (redb store, zstd level 3, per-oid locks, group commit)",
            "client: native Rust client (msgpack M5), extensions sequences and "
            "trace_context declared",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
    ),
    "zoe-asyncio": StackConfig(
        name="zoe-asyncio",
        scenario="zoeclient",
        server="zoe",
        section="zoeclient",
        native=False,
        description=(
            "zoe serve and zoe.zeo.ClientStorage with the asyncio client of "
            "ZEO inside, zoedb[plone] installed"
        ),
        differs=(
            "server: zoe serve",
            "client: ZEO's asyncio client (msgpack M5) with the ZOE extensions",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
    ),
    "zoe-stock": StackConfig(
        name="zoe-stock",
        scenario="zeoclient",
        server="zoe",
        section="zeoclient",
        native=None,
        description=(
            "zoe serve and a stock ZEO.ClientStorage, no zoedb: the server "
            "alone, with no client extension"
        ),
        differs=(
            "server: zoe serve",
            "client: stock ZEO.ClientStorage over msgpack M5, no extension",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
    ),
}


@dataclass(frozen=True)
class Settings:
    """What stays the same in every stack."""

    instances: int = 2
    threads: int = 2
    # Objects in the cache of each ZODB connection.
    zodb_cache_objects: int = 30_000
    # The pool of ZODB connections of each instance.
    pool_size: int = 7
    # The storage client cache of each instance, in memory for both
    # clients: no `client` key, so no persistent cache file.
    client_cache: str = "128MB"
    # The CPUs (taskset lists) of the server, of each instance, and of the
    # load generator.
    server_cpus: str = "0"
    zope_cpus: tuple[str, ...] = ("1", "2")
    locust_cpus: str = "3"
    base_port: int = 18080
    server_port: int = 18100
    admin_port: int = 18199
    probe: bool = True

    def to_json(self) -> dict:
        return dataclasses.asdict(self)


def interpreter(config: StackConfig) -> Path:
    return PLONE / config.scenario / "parts" / "instance" / "bin" / "interpreter"


def zope_serve_script() -> Path:
    found = sorted(
        glob.glob(str(PLONE / "eggs" / "Zope-*" / "Zope2" / "Startup" / "serve.py"))
    )
    if not found:
        raise FileNotFoundError(
            "no Zope egg under target/plone/eggs: run `just plone zeo build`"
        )
    return Path(found[-1])


def zoe_binary() -> Path:
    if os.environ.get("ZOE_BINARY"):
        return Path(os.environ["ZOE_BINARY"])
    return TARGET / "release" / "zoe"


def missing_prerequisites(config: StackConfig) -> list[str]:
    missing = []
    if not interpreter(config).exists():
        missing.append(
            f"{interpreter(config)}: run `just plone {config.scenario} build`"
        )
    if config.server == "zeo" and not interpreter(STACKS["zeo"]).exists():
        missing.append("the zeo scenario: run `just plone zeo build`")
    if config.server == "zoe" and not zoe_binary().exists():
        missing.append(
            f"{zoe_binary()}: run `cargo build --release -p zoe-server --bin zoe`"
        )
    try:
        zope_serve_script()
    except FileNotFoundError as error:
        missing.append(str(error))
    return missing


def store_layout(store: Path, config: StackConfig) -> dict[str, Path]:
    if config.server == "zeo":
        return {
            "data_fs": store / "filestorage" / "Data.fs",
            "blobs": store / "blobstorage",
        }
    return {"zoe": store / "zoe", "blobs": store / "zoe" / "blobs"}


def storage_section(
    config: StackConfig, settings: Settings, blob_dir: Path, name: str
) -> str:
    lines = [
        f"    <{config.section}>",
        f"      server 127.0.0.1:{settings.server_port}",
        "      storage 1",
        f"      name {name}",
        f"      cache-size {settings.client_cache}",
        f"      blob-dir {blob_dir}",
        "      shared-blob-dir off",
        "      read-only false",
    ]
    if config.native is not None:
        lines.append(f"      native {'true' if config.native else 'false'}")
    lines.append(f"    </{config.section}>")
    return "\n".join(lines)


def zope_conf(config: StackConfig, settings: Settings, home: Path, name: str) -> str:
    """Return the zope.conf of an instance, or of a script client."""
    head = "%import zoe\n" if config.section == "zoeclient" else ""
    instancehome = PLONE / config.scenario / "parts" / "instance"
    return f"""{head}%define INSTANCEHOME {instancehome}
instancehome $INSTANCEHOME
%define CLIENTHOME {home / "var"}
clienthome $CLIENTHOME
debug-mode off
security-policy-implementation C
verbose-security off
default-zpublisher-encoding utf-8
<environment>
    CHAMELEON_CACHE {home / "var" / "cache"}
</environment>
<zodb_db main>
    cache-size {settings.zodb_cache_objects}
    pool-size {settings.pool_size}
{storage_section(config, settings, home / "blobcache", name)}
    mount-point /
</zodb_db>
python-check-interval 1000
"""


def wsgi_ini(settings: Settings, home: Path, port: int, probe_out: Path | None) -> str:
    log = home / "log"
    pipeline = ["translogger"]
    probe = ""
    if probe_out is not None:
        pipeline.insert(0, "probe")
        probe = f"""
[filter:probe]
use = call:plonebench_probe:filter_factory
out = {probe_out}
"""
    stages = "\n    ".join([*pipeline, "egg:Zope#httpexceptions", "zope"])
    return f"""[server:main]
paste.server_factory = plone.recipe.zope2instance:main
use = egg:plone.recipe.zope2instance#main
fast-listen = 127.0.0.1:{port}
threads = {settings.threads}
clear_untrusted_proxy_headers = false
max_request_body_size = 1073741824
asyncore_use_poll = false

[app:zope]
use = egg:Zope#main
zope_conf = {home / "zope.conf"}

[filter:translogger]
use = egg:Paste#translogger
setup_console_handler = False
{probe}
[pipeline:main]
pipeline =
    {stages}

[loggers]
keys = root, plone, waitress.queue, waitress, wsgi

[handlers]
keys = accesslog, eventlog

[formatters]
keys = generic, message

[logger_root]
level = INFO
handlers = eventlog

[logger_plone]
level = INFO
handlers = eventlog
qualname = plone

[logger_waitress.queue]
level = INFO
handlers = eventlog
qualname = waitress.queue
propagate = 0

[logger_waitress]
level = INFO
handlers = eventlog
qualname = waitress

[logger_wsgi]
level = INFO
handlers = accesslog
qualname = wsgi
propagate = 0

[handler_accesslog]
class = FileHandler
args = (r'{log / "access.log"}', 'a')
kwargs = {{}}
level = INFO
formatter = message

[handler_eventlog]
class = FileHandler
args = (r'{log / "event.log"}', 'a')
kwargs = {{}}
level = NOTSET
formatter = generic

[formatter_generic]
format = %(asctime)s %(levelname)-7.7s [%(name)s:%(lineno)s][%(threadName)s] %(message)s

[formatter_message]
format = %(message)s
"""


def zeo_conf(settings: Settings, store: Path, run: Path) -> str:
    return f"""<zeo>
  address 127.0.0.1:{settings.server_port}
  read-only false
  invalidation-queue-size 100
  pid-filename {run / "zeoserver.pid"}
</zeo>

<filestorage 1>
  path {store / "filestorage" / "Data.fs"}
  blob-dir {store / "blobstorage"}
</filestorage>

<eventlog>
  level info
  <logfile>
    path {run / "log" / "zeoserver.log"}
    format %(asctime)s %(message)s
  </logfile>
</eventlog>
"""


def clean_environment(**extra: str) -> dict[str, str]:
    """Return the environment of a child: no ZOE_* or PLONEBENCH_* variable
    of the caller, and the C UTF-8 locale that Zope needs."""
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("ZOE_", "PLONEBENCH_", "UV_", "VIRTUAL_ENV"))
    }
    env["LC_ALL"] = "C.UTF-8"
    env.pop("PYTHONPATH", None)
    env.update(extra)
    return env


def port_open(port: int) -> bool:
    with socket.socket() as sock:
        sock.settimeout(0.5)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def wait_for(
    predicate, timeout: float, what: str, process: subprocess.Popen | None = None
):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError(f"{what}: the process exited with {process.returncode}")
        if predicate():
            return
        time.sleep(0.2)
    raise TimeoutError(f"{what}: not ready after {timeout:.0f} s")


def http_ok(url: str) -> bool:
    try:
        with urllib.request.urlopen(url, timeout=5) as response:
            return response.status == 200
    except (urllib.error.URLError, OSError):
        return False


@dataclass
class Stack:
    """A running stack: its server, its instances and their files."""

    config: StackConfig
    settings: Settings
    run: Path
    store: Path
    processes: dict[str, subprocess.Popen] = field(default_factory=dict)
    logs: list = field(default_factory=list)

    def __post_init__(self):
        (self.run / "log").mkdir(parents=True, exist_ok=True)
        (self.run / "probe").mkdir(parents=True, exist_ok=True)

    # The server.

    def start_server(self, timeout: float = 120.0) -> None:
        if port_open(self.settings.server_port):
            raise RuntimeError(f"port {self.settings.server_port} is in use")
        log = open(self.run / "log" / "server.out", "w")
        self.logs.append(log)
        taskset = ["taskset", "-c", self.settings.server_cpus]
        if self.config.server == "zeo":
            conf = self.run / "zeo.conf"
            conf.write_text(zeo_conf(self.settings, self.store, self.run))
            env = clean_environment(
                PLONEBENCH_PROBE_OUT=str(self.run / "probe" / "zeoserver.jsonl"),
            )
            launch = PROBE / "zeoserver" / "launch.py"
            command = [
                *taskset,
                str(interpreter(STACKS["zeo"])),
                str(launch),
                str(conf),
            ]
            process = subprocess.Popen(
                command, env=env, stdout=log, stderr=subprocess.STDOUT, cwd=self.run
            )
            self.processes["server"] = process
            wait_for(
                lambda: port_open(self.settings.server_port), timeout, "runzeo", process
            )
        else:
            command = [
                *taskset,
                str(zoe_binary()),
                "serve",
                "--path",
                str(self.store / "zoe"),
                "--listen",
                f"127.0.0.1:{self.settings.server_port}",
                "--admin-listen",
                f"127.0.0.1:{self.settings.admin_port}",
            ]
            process = subprocess.Popen(
                command,
                env=clean_environment(),
                stdout=log,
                stderr=open(self.run / "log" / "zoe-serve.log", "w"),
                cwd=self.run,
            )
            self.processes["server"] = process
            wait_for(
                lambda: http_ok(f"http://127.0.0.1:{self.settings.admin_port}/readyz"),
                timeout,
                "zoe serve",
                process,
            )

    # The instances.

    def instance_home(self, index: int) -> Path:
        return self.run / f"instance{index}"

    def instance_port(self, index: int) -> int:
        return self.settings.base_port + index

    def urls(self) -> list[str]:
        return [
            f"http://127.0.0.1:{self.instance_port(i)}"
            for i in range(self.settings.instances)
        ]

    def prepare_home(self, home: Path, name: str) -> Path:
        for sub in ("var/cache", "log", "blobcache"):
            (home / sub).mkdir(parents=True, exist_ok=True)
        conf = home / "zope.conf"
        conf.write_text(zope_conf(self.config, self.settings, home, name))
        return conf

    def start_instances(self, timeout: float = 300.0) -> None:
        serve = zope_serve_script()
        for index in range(self.settings.instances):
            home = self.instance_home(index)
            self.prepare_home(home, f"instance{index}")
            probe_out = self.run / "probe" / f"instance{index}.jsonl"
            ini = home / "wsgi.ini"
            ini.write_text(
                wsgi_ini(
                    self.settings,
                    home,
                    self.instance_port(index),
                    probe_out if self.settings.probe else None,
                )
            )
            cpus = self.settings.zope_cpus[index % len(self.settings.zope_cpus)]
            log = open(home / "log" / "stdout.log", "w")
            self.logs.append(log)
            env = clean_environment(PYTHONPATH=str(PROBE))
            process = subprocess.Popen(
                [
                    "taskset",
                    "-c",
                    cpus,
                    str(interpreter(self.config)),
                    str(serve),
                    str(ini),
                ],
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                cwd=home,
            )
            self.processes[f"instance{index}"] = process
        for index, url in enumerate(self.urls()):
            wait_for(
                lambda url=url: http_ok(f"{url}/Plone"),
                timeout,
                f"instance{index}",
                self.processes[f"instance{index}"],
            )

    def run_script(
        self, script: str, *args: str, name: str = "script", timeout=None
    ) -> str:
        """Run a script of inzope/ in a client process of its own, and
        return its stdout. The process opens the storage as the instances
        do, with a cache and a blob cache of its own."""
        home = self.run / name
        conf = self.prepare_home(home, name)
        env = clean_environment()
        result = subprocess.run(
            [
                str(interpreter(self.config)),
                str(INZOPE / "run.py"),
                str(conf),
                script,
                *args,
            ],
            env=env,
            cwd=home,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        (home / "log" / f"{script}.out").write_text(result.stdout)
        (home / "log" / f"{script}.err").write_text(result.stderr)
        if result.returncode != 0:
            tail = "\n".join(result.stderr.splitlines()[-30:])
            raise RuntimeError(f"{script} failed with {result.returncode}:\n{tail}")
        return result.stdout

    # The stop.

    def pids(self) -> dict[str, int]:
        return {name: process.pid for name, process in self.processes.items()}

    def stop_instances(self, timeout: float = 60.0) -> dict[str, int | None]:
        codes = {}
        for name in [n for n in self.processes if n.startswith("instance")]:
            codes[name] = _stop(self.processes.pop(name), timeout)
        return codes

    def stop(self, timeout: float = 60.0) -> dict[str, int | None]:
        codes = self.stop_instances(timeout)
        if "server" in self.processes:
            codes["server"] = _stop(self.processes.pop("server"), timeout)
        for log in self.logs:
            log.close()
        self.logs.clear()
        return codes

    def describe(self) -> dict:
        return {
            "stack": dataclasses.asdict(self.config),
            "settings": self.settings.to_json(),
            "zope_conf": zope_conf(
                self.config, self.settings, Path("<home>"), "instance0"
            ),
        }


def _stop(process: subprocess.Popen, timeout: float) -> int | None:
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    return process.returncode


def copy_store(source: Path, destination: Path) -> None:
    """Copy a store directory, with reflinks where the file system has
    them."""
    if destination.exists():
        shutil.rmtree(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["cp", "-a", "--reflink=auto", str(source), str(destination)], check=True
    )


def write_json(path: Path, data) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(data, indent=1, sort_keys=False))
    tmp.replace(path)
