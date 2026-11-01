"""The files of a run: `zope.conf` and `wsgi.ini` of each instance, from
one template, and `zeo.conf` of the ZEO server."""

from __future__ import annotations

from pathlib import Path

from plonebench.stack.config import PLONE, Settings, StackConfig


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
    backends = f"\n    ZOE_PLONE_BACKENDS {config.backends}" if config.backends else ""
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
    CHAMELEON_CACHE {home / "var" / "cache"}{backends}
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
