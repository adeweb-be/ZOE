"""The `<zoe>` section of ZConfig and the `zoe://` URI of zodburi.

`%import zoe` loads `component.xml`, which declares the `<zoe>` section and
the `<zoeclient>` section. The `<zodb>` and `<zodb_db>` sections take either
as their storage. The `zoe` entry point of `zodburi.resolvers` names
`resolve_uri`.

`<zoeclient>` opens a `zoe.zeo.ClientStorage`. It takes every key of
`<zeoclient>`, with the same values, and `wait-timeout`. It has no
environment twins, as `<zeoclient>` has none.

The keys are in kebab-case. The query of the URI takes the same keys with
the same values. Each key has an environment variable, its twin: `ZOE_` and
the key in upper case, with `_` for `-`. The twin gives the value of a key
only when the section does not set it. The URI does not read the twins. An
empty value counts as not set. ZConfig gives each value of the section as
text, and this module converts the section, the twins and the URI with the
same converters.
"""

import os
import urllib.parse

from ZConfig.datatypes import stock_datatypes
from ZODB.config import BaseConfig

# The backends that this version builds.
BACKENDS = ("redb",)


def backend(value):
    """Return the name of a backend. Refuse a backend that is not built."""
    if value not in BACKENDS:
        raise ValueError(
            f"unknown backend {value!r}: the backends are {', '.join(BACKENDS)}"
        )
    return value


def early_locks(value):
    """Return False for a false boolean.

    Refuse a true boolean: the storage has no early locks yet.
    """
    if stock_datatypes["boolean"](value):
        raise ValueError("early-locks on: the storage has no early locks yet")
    return False


def lock_timeout(value):
    """Return whole seconds, as ZConfig's time-interval gives them.

    Refuse a negative time.
    """
    seconds = stock_datatypes["time-interval"](value)
    if seconds < 0:
        raise ValueError("the lock timeout is 0 or more seconds")
    return seconds


# The keys and the converters of their values.
KEYS = {
    "path": stock_datatypes["string"],
    "blob-dir": stock_datatypes["string"],
    "backend": backend,
    "lock-timeout": lock_timeout,
    "early-locks": early_locks,
    "read-only": stock_datatypes["boolean"],
}


def twin(key):
    """Return the name of the environment twin of a key."""
    return "ZOE_" + key.upper().replace("-", "_")


def convert(key, text, source):
    """Convert the text of a key. Name the source in an error."""
    try:
        return KEYS[key](text)
    except ValueError as error:
        raise ValueError(f"{source}: {error}") from None


def open_storage(settings):
    """Open a `zoe.Storage` with settings, a dict of converted values.

    The keys of settings are the keys of the section. A missing key takes
    its default.
    """
    import zoe

    path = settings.get("path")
    if not path:
        raise ValueError(f"<zoe> needs path, or the {twin('path')} variable")
    blob_dir = settings.get("blob-dir")
    return zoe.Storage(
        os.path.expanduser(path),
        read_only=settings.get("read-only", False),
        lock_timeout=settings.get("lock-timeout"),
        blob_dir=os.path.expanduser(blob_dir) if blob_dir else None,
    )


class ZoeStorage(BaseConfig):
    """The `<zoe>` section. `open` opens its `zoe.Storage`."""

    def open(self):
        settings = {}
        for key in KEYS:
            text = getattr(self.config, key.replace("-", "_"))
            source = f"<zoe> {key} {text!r}"
            if not text:
                text = os.environ.get(twin(key), "")
                source = f"{twin(key)}={text!r}"
            if text:
                settings[key] = convert(key, text, source)
        return open_storage(settings)


def resolve_uri(uri):
    """Return the storage factory and the database keywords of a URI.

    The URI is `zoe:///absolute/path?key=value&...`. The path of the URI is
    the path of the store. The query takes the keys of the section, except
    path, each at most once. The other query keys go to zodburi, which takes
    the keys of the database, such as `database_name`, and refuses the rest.
    """
    parts = urllib.parse.urlsplit(uri)
    path = urllib.parse.unquote(parts.path)
    if parts.netloc or parts.fragment or not os.path.isabs(path):
        raise ValueError(f"{uri!r} is not a zoe:///absolute/path URI")
    settings = {"path": path}
    rest = {}
    for name, text in urllib.parse.parse_qsl(parts.query):
        if name in KEYS and name != "path":
            if name in settings:
                raise ValueError(f"{uri!r} gives {name} more than once")
            settings[name] = convert(name, text, f"{name}={text!r}")
        else:
            rest[name] = text
    return lambda: open_storage(settings), rest


class ZoeClient(BaseConfig):
    """The `<zoeclient>` section. `open` opens its `zoe.zeo.ClientStorage`.

    It gives the keys of `<zeoclient>` as `ZODB.config.ZEOClient` gives them,
    and `wait-timeout` as `wait_timeout`.
    """

    def open(self):
        from zoe.zeo import ClientStorage

        config = self.config
        options = {}
        for name in ("blob_cache_size", "blob_cache_size_check", "client_label"):
            value = getattr(config, name)
            if value is not None:
                options[name] = value
        return ClientStorage(
            [server.address for server in config.server],
            blob_dir=config.blob_dir,
            shared_blob_dir=config.shared_blob_dir,
            storage=config.storage,
            cache_size=config.cache_size,
            name=config.name,
            client=config.client,
            var=config.var,
            min_disconnect_poll=config.min_disconnect_poll,
            max_disconnect_poll=config.max_disconnect_poll,
            wait=config.wait,
            read_only=config.read_only,
            read_only_fallback=config.read_only_fallback,
            drop_cache_rather_verify=config.drop_cache_rather_verify,
            username=config.username,
            password=config.password,
            realm=config.realm,
            wait_timeout=config.wait_timeout,
            **options,
        )
