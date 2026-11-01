"""The processes of a run: the storage server, the Zope instances and the
scripts of inzope/, and the copy of a store and its sync."""

from __future__ import annotations

import dataclasses
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

from benchcommon import servers

from plonebench.stack.config import (
    INZOPE,
    PROBE,
    STACKS,
    Settings,
    StackConfig,
    interpreter,
    pin,
    serve_options,
    zoe_binary,
    zope_serve_script,
)
from plonebench.stack.templates import wsgi_ini, zeo_conf, zope_conf


def clean_environment(**extra: str) -> dict[str, str]:
    """Return the environment of a child: no ZOE_* or PLONEBENCH_* variable
    of the caller, and the C UTF-8 locale that Zope needs."""
    env = {
        key: value
        for key, value in servers.scrubbed().items()
        if not key.startswith(("PLONEBENCH_", "UV_", "VIRTUAL_ENV"))
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
        # Each port of the run is free first: another run can hold any of them.
        ports = [self.settings.server_port, self.settings.admin_port]
        ports += [self.settings.base_port + i for i in range(self.settings.instances)]
        for port in ports:
            if port_open(port):
                raise RuntimeError(f"port {port} is in use")
        log = open(self.run / "log" / "server.out", "w")
        self.logs.append(log)
        taskset = pin(self.settings.server_cpus)
        if self.config.server == "zeo":
            conf = self.run / "zeo.conf"
            conf.write_text(zeo_conf(self.settings, self.store, self.run))
            env = clean_environment(
                PLONEBENCH_PROBE_OUT=str(self.run / "probe" / "zeoserver.jsonl"),
                PLONEBENCH_ZEO_FSYNC=self.settings.zeo_fsync,
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
            command += serve_options(self.settings)
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
                    *pin(cpus),
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
            codes[name] = servers.stop(self.processes.pop(name), timeout)
        return codes

    def stop(self, timeout: float = 60.0) -> dict[str, int | None]:
        codes = self.stop_instances(timeout)
        if "server" in self.processes:
            codes["server"] = servers.stop(self.processes.pop("server"), timeout)
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


def copy_store(source: Path, destination: Path) -> None:
    """Copy a store directory, with reflinks where the file system has
    them. On macOS, the `cp -c` of the system clones the files on APFS; the
    development shell puts GNU cp first on PATH."""
    if destination.exists():
        shutil.rmtree(destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if sys.platform == "darwin":
        clone = ["/bin/cp", "-a", "-c"]
    else:
        clone = ["cp", "-a", "--reflink=auto"]
    if subprocess.run([*clone, str(source), str(destination)]).returncode:
        if destination.exists():
            shutil.rmtree(destination)
        subprocess.run(["cp", "-a", str(source), str(destination)], check=True)


def sync_files(path: Path) -> None:
    """Write the dirty pages of the files under `path` to the disk, and
    wait for the disk. On Linux, `sync -f` syncs the file system that holds
    `path` (syncfs(2)): one flush for every file of a copy, where a sync of
    each file would wait for the journal once per file. It also writes the
    dirty pages of other files of that file system. On macOS, a copy of a
    store is a clone that writes no data, and `os.sync` schedules the rest."""
    if sys.platform == "darwin":
        os.sync()
        return
    subprocess.run(["sync", "-f", str(path)], check=True)
