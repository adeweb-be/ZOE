"""The `<zoe>` section of ZConfig and the `zoe://` URI of zodburi.

`%import zoe` loads `component.xml`, which declares the `<zoe>` section and
the `<zoeclient>` section. The `<zodb>` and `<zodb_db>` sections take either
as their storage. The `zoe` entry point of `zodburi.resolvers` names
`resolve_uri`.

`<zoeclient>` opens a `zoe.zeo.ClientStorage`. It takes every key of
`<zeoclient>`, with the same values, `wait-timeout`, `native`,
`early-locks` and `early-lock-classes`, and an
`<ssl>` section with the keys of the `<ssl>` section of ZEO's
`<clientstorage>`. It has no environment twins, as `<zeoclient>` has none.

The keys are in kebab-case. The query of the URI takes the same keys with
the same values. Each key has an environment variable, its twin: `ZOE_` and
the key in upper case, with `_` for `-`. The twin gives the value of a key
only when the section does not set it. The URI does not read the twins. An
empty value counts as not set. ZConfig gives each value of the section as
text, and this module converts the section, the twins and the URI with the
same converters.
"""

import os
import re
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
    """Return the boolean of `early-locks`."""
    return stock_datatypes["boolean"](value)


def class_names(value):
    """Return the classes of `early-lock-classes`, as a tuple of
    `module.name`. The value separates them with spaces or commas."""
    names = tuple(name for name in re.split(r"[\s,]+", value.strip()) if name)
    for name in names:
        if not re.fullmatch(r"[\w.]+\.\w+", name):
            raise ValueError(f"{name!r} is not module.name")
    return names


def lock_timeout(value):
    """Return whole seconds, as ZConfig's time-interval gives them.

    Refuse a negative time.
    """
    seconds = stock_datatypes["time-interval"](value)
    if seconds < 0:
        raise ValueError("the lock timeout is 0 or more seconds")
    return seconds


def compression(value):
    """Return the compression of the records that the store writes.

    The value is `none`, `zstd` or `zstd:LEVEL`, in any case. The store
    parses it again.
    """
    text = value.strip().lower()
    if re.fullmatch(r"none|zstd(:[+-]?[0-9]+)?", text):
        return text
    raise ValueError(f"{value!r} is not none, zstd or zstd:LEVEL")


def merge_mode(value):
    """Return the mode of the structural merges: on, shadow or off."""
    text = value.strip().lower()
    if text in ("on", "shadow", "off"):
        return text
    raise ValueError(f"{value!r} is not on, shadow or off")


def class_modes(value):
    """Return the modes of classes of the structural merges, as a dict.

    The value is one `CLASS MODE` or `CLASS=MODE`, or several of them
    separated by commas, as the twin gives them. The class is
    `module.name`.
    """
    modes = {}
    for item in value.split(","):
        match = re.fullmatch(r"\s*([\w.]+\.\w+)\s*(?:=|\s)\s*(\w+)\s*", item)
        if not match:
            raise ValueError(f"{item.strip()!r} is not CLASS MODE")
        modes[match.group(1)] = merge_mode(match.group(2))
    return modes


# The keys and the converters of their values.
KEYS = {
    "path": stock_datatypes["string"],
    "blob-dir": stock_datatypes["string"],
    "backend": backend,
    "lock-timeout": lock_timeout,
    "early-locks": early_locks,
    "early-lock-classes": class_names,
    "read-only": stock_datatypes["boolean"],
    "pack-gc": stock_datatypes["boolean"],
    "compression": compression,
    "structural-merges": merge_mode,
    "structural-merge-class": class_modes,
    "posting-dicts": merge_mode,
    "resolution-log": stock_datatypes["string"],
}

# The keys that a section and a URI can give more than once. Their values
# add up.
MULTIKEYS = ("structural-merge-class",)


# The twins whose name is not the name of their key: a key that comes
# once for each value has a twin that takes them all.
TWINS = {"structural-merge-class": "ZOE_STRUCTURAL_MERGE_CLASSES"}


def twin(key):
    """Return the name of the environment twin of a key."""
    return TWINS.get(key) or "ZOE_" + key.upper().replace("-", "_")


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
    resolution_log = settings.get("resolution-log")
    return zoe.Storage(
        os.path.expanduser(path),
        read_only=settings.get("read-only", False),
        lock_timeout=settings.get("lock-timeout"),
        early_locks=settings.get("early-locks", False),
        early_lock_classes=settings.get("early-lock-classes"),
        blob_dir=os.path.expanduser(blob_dir) if blob_dir else None,
        pack_gc=settings.get("pack-gc", True),
        compression=settings.get("compression"),
        structural_merges=settings.get("structural-merges"),
        structural_merge_classes=settings.get("structural-merge-class"),
        posting_dicts=settings.get("posting-dicts"),
        resolution_log=os.path.expanduser(resolution_log) if resolution_log else None,
    )


class ZoeStorage(BaseConfig):
    """The `<zoe>` section. `open` opens its `zoe.Storage`."""

    def open(self):
        settings = {}
        for key in KEYS:
            given = getattr(self.config, key.replace("-", "_"))
            if key in MULTIKEYS:
                # A key that comes once for each value.
                texts = [text for text in given or () if text]
                if texts:
                    modes = {}
                    for text in texts:
                        modes.update(convert(key, text, f"<zoe> {key} {text!r}"))
                    settings[key] = modes
                    continue
                given = None
            text = given
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
    path, each at most once; `structural-merge-class` comes once for each
    class. The other query keys go to zodburi, which takes the keys of the
    database, such as `database_name`, and refuses the rest.
    """
    parts = urllib.parse.urlsplit(uri)
    path = urllib.parse.unquote(parts.path)
    if parts.netloc or parts.fragment or not os.path.isabs(path):
        raise ValueError(f"{uri!r} is not a zoe:///absolute/path URI")
    settings = {"path": path}
    rest = {}
    for name, text in urllib.parse.parse_qsl(parts.query):
        if name in MULTIKEYS:
            value = convert(name, text, f"{name}={text!r}")
            settings.setdefault(name, {}).update(value)
        elif name in KEYS and name != "path":
            if name in settings:
                raise ValueError(f"{uri!r} gives {name} more than once")
            settings[name] = convert(name, text, f"{name}={text!r}")
        else:
            rest[name] = text
    return lambda: open_storage(settings), rest


class ZoeClient(BaseConfig):
    """The `<zoeclient>` section. `open` opens its `zoe.zeo.ClientStorage`.

    It gives the keys of `<zeoclient>` as `ZODB.config.ZEOClient` gives them,
    `wait-timeout` as `wait_timeout`, `native` as `native`, and
    `early-locks` and `early-lock-classes` as `early_locks` and
    `early_lock_classes`. It gives the
    `<ssl>` section as it is: the native client reads its files, and ZEO's
    client gets the context that ZEO's `<ssl>` section gives.
    """

    def open(self):
        return self._open_client(self.config.native)

    def _open_client(self, native):
        """Open the client, with the native client inside when `native`
        is true. `native` is None when the section has no key `native`:
        the default of `zoe.zeo.ClientStorage` applies."""
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
            native=native,
            early_locks=config.early_locks,
            early_lock_classes=class_names(config.early_lock_classes or ""),
            _ssl_section=config.ssl,
            **options,
        )
