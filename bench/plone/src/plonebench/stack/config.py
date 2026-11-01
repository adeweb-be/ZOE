"""The configurations of the comparison: the stacks, the settings that
every stack shares, and the files that a run needs."""

from __future__ import annotations

import dataclasses
import glob
import sys
from dataclasses import dataclass
from pathlib import Path

from benchcommon import servers

# The package plonebench.
HERE = Path(__file__).resolve().parents[1]
REPO = HERE.parents[3]
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
    # The backends of zoe.plone that the instances install, for
    # ZOE_PLONE_BACKENDS in zope.conf; empty for none.
    backends: str = ""
    # True when the run moves the properties of the members and groups of
    # the site into records of zoe.plone, with their tier 2 entry, before
    # the instances start (ADR 0046, section 9).
    member_properties: bool = False


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
            "server: zoe serve (redb store, raw records unless --zoe-compression, "
            "per-oid locks, group commit)",
            "client: native Rust client (msgpack M5), extensions sequences and "
            "trace_context declared",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
    ),
    "zoe-native-kv": StackConfig(
        name="zoe-native-kv",
        scenario="zoeclient",
        server="zoe",
        section="zoeclient",
        native=True,
        description=(
            "zoe-native with the locking backend of zoe.plone: the locks of "
            "plone.locking in the ephemeral namespace of the store (ADR 0043)"
        ),
        differs=(
            "server: zoe serve (redb store, raw records unless --zoe-compression, "
            "per-oid locks, group commit)",
            "client: native Rust client (msgpack M5), extensions sequences, kv "
            "and trace_context declared",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "zoe.plone: the locks of edit forms live in the ephemeral namespace "
            "of the store, so a lock writes no object",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
        backends="locking",
    ),
    "zoe-native-qi": StackConfig(
        name="zoe-native-qi",
        scenario="zoeclient",
        server="zoe",
        section="zoeclient",
        native=True,
        description=(
            "zoe-native with queued indexing of zoe.plone: a save writes its "
            "catalog operations to a durable namespace of the store, and one "
            "indexer for each site applies them (ADR 0047)"
        ),
        differs=(
            "server: zoe serve (redb store, raw records unless --zoe-compression, "
            "per-oid locks, group commit)",
            "client: native Rust client (msgpack M5), extensions sequences, kv "
            "and trace_context declared",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "zoe.plone: a save writes no object of the catalog; the indexer of "
            "the site applies the queue in transactions of its own, and the "
            "next request of the user who saved waits for it",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
        backends="indexing",
    ),
    "zoe-native-qi-kv": StackConfig(
        name="zoe-native-qi-kv",
        scenario="zoeclient",
        server="zoe",
        section="zoeclient",
        native=True,
        description=(
            "zoe-native-qi with the locking backend of zoe.plone (ADR 0043) "
            "and the properties of the members and groups in records of "
            "zoe.plone, which merge a login at the vote (ADR 0046)"
        ),
        differs=(
            "server: zoe serve (redb store, raw records unless --zoe-compression, "
            "per-oid locks, group commit)",
            "client: native Rust client (msgpack M5), extensions sequences, kv "
            "and trace_context declared",
            "zoe.plone: portal_uidgenerator takes its ids from a store sequence",
            "zoe.plone: a save writes no object of the catalog; the indexer of "
            "the site applies the queue in transactions of its own, and the "
            "next request of the user who saved waits for it",
            "zoe.plone: the locks of edit forms live in the ephemeral namespace "
            "of the store, so a lock writes no object",
            "zoe.plone: the properties of each member and group are a record "
            "of their own, so two logins do not conflict on the property "
            "sheet of PAS",
            "conflicts: the client resolves through the vote (tier 3)",
        ),
        backends="indexing,locking",
        member_properties=True,
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
    # load generator. macOS has no CPU affinity: there the processes are not
    # pinned, and the result says so.
    server_cpus: str = "0"
    zope_cpus: tuple[str, ...] = ("1", "2")
    locust_cpus: str = "3"
    base_port: int = 18080
    server_port: int = 18100
    admin_port: int = 18199
    probe: bool = True
    # How the ZEO server syncs a commit on macOS: `plain` keeps stock
    # fsync(2), as `zoe serve` does (ADR 0040); `full` makes os.fsync use
    # F_FULLFSYNC, a stronger sync than the one of `zoe serve`. Linux
    # ignores it.
    zeo_fsync: str = "plain"
    # The page cache of the store of `zoe serve` (`--cache-size`). None
    # gives no option, so the server keeps the default of the store.
    zoe_cache_size: str | None = None
    # The compression of the records that `zoe serve` writes
    # (`--compression`): `none`, `zstd` or `zstd:LEVEL`. None gives no
    # option, so the server keeps the default of the store: raw records.
    zoe_compression: str | None = None
    # The switch `split_merges` of the merge policy (ADR 0048): `on`,
    # `shadow` or `off`. The run writes it into the merge policy of the
    # store before the instances start. None writes nothing, so the store
    # keeps the policy of the fixture.
    split_merges: str | None = None
    # The switch `folder_orders` of the merge policy (ADR 0052): `on`,
    # `shadow` or `off`, with the tier 2 entry of the class of the folders
    # of the fixture. None writes nothing.
    folder_orders: str | None = None

    def to_json(self) -> dict:
        return dataclasses.asdict(self)


# The modes of the switch `split_merges`.
SPLIT_MERGES = ("on", "shadow", "off")
# The modes of the switch `folder_orders`.
FOLDER_ORDERS = SPLIT_MERGES
# The class of the folders of the fixture, and the tier 2 entry that the
# order merge needs for it (ADR 0052, section 5).
FOLDER_CLASS = "plone.app.contenttypes.content.Folder"
FOLDER_ENTRY = {
    "tier2": "on",
    "default": "strict",
    "fields": {"modification_date": "max"},
}


def split_merges_refusal(config: StackConfig, settings: Settings) -> str | None:
    """Return why the run cannot write the switches `split_merges` and
    `folder_orders` of the settings into the store of `config`, or None
    when it can."""
    for option, mode in (
        ("--split-merges", settings.split_merges),
        ("--folder-orders", settings.folder_orders),
    ):
        if mode is None:
            continue
        if mode not in SPLIT_MERGES:
            return f"{option} {mode!r} is not one of {', '.join(SPLIT_MERGES)}"
        if config.server != "zoe" or config.section != "zoeclient":
            return f"{option} needs a stack of zoe serve with zoedb, not {config.name}"
    return None


def cpus_refusal(settings: Settings) -> str | None:
    """Return why the instances of the settings cannot each run on a CPU
    list of their own, or None when they can. A longer list is valid: the
    instances take its first lists."""
    lists = len(settings.zope_cpus)
    if lists < settings.instances:
        return (
            f"--zope-cpus gives {lists} CPU lists for --instances "
            f"{settings.instances}: give one list for each Zope instance"
        )
    return None


def instance_cpus(settings: Settings) -> tuple[str, ...]:
    """Return the CPU list of each instance: the first lists of
    `zope_cpus`, one for each instance."""
    return tuple(settings.zope_cpus[: settings.instances])


def profile_refusal(config: StackConfig) -> str | None:
    """Return why a run of `config` cannot import a profile of `zoe.plone`
    into its store, or None when it can."""
    if config.server != "zoe" or config.section != "zoeclient":
        return f"--profile needs a stack of zoe serve with zoedb, not {config.name}"
    return None


def campaign_settings(config: StackConfig, settings: Settings) -> Settings:
    """Return the settings of a run of `config` in a campaign: without the
    switch `split_merges` when the stack does not take it, so a campaign of
    mixed stacks runs each of them."""
    if config.server == "zoe" and config.section == "zoeclient":
        return settings
    return dataclasses.replace(settings, split_merges=None, folder_orders=None)


def split_merges_policy(current: dict | None, mode: str) -> dict:
    """Return the policy that `zoe.policy.install` takes: the policy
    `current`, as `zoe.policy.read` gives it, or none, with the switch
    `split_merges` set to `mode`. The classes and the other switches of the
    policy stay; `install` sets the version."""
    policy = {key: value for key, value in (current or {}).items() if key != "version"}
    policy.setdefault("classes", {})
    policy["split_merges"] = mode
    return policy


def folder_orders_policy(current: dict | None, mode: str) -> dict:
    """Return the policy `current`, or none, with the switch
    `folder_orders` set to `mode` and the tier 2 entry `FOLDER_ENTRY` for
    `FOLDER_CLASS` (ADR 0052). The other classes and switches stay."""
    policy = {key: value for key, value in (current or {}).items() if key != "version"}
    classes = dict(policy.get("classes", {}))
    entry = dict(classes.get(FOLDER_CLASS, {}))
    entry.update({"tier2": "on", "default": entry.get("default", "strict")})
    entry["fields"] = {**entry.get("fields", {}), **FOLDER_ENTRY["fields"]}
    classes[FOLDER_CLASS] = entry
    policy["classes"] = classes
    policy["folder_orders"] = mode
    return policy


def serve_options(settings: Settings) -> list[str]:
    """The options of `zoe serve` that come from the settings. A setting
    that is None gives no option, so the server keeps its default."""
    options = []
    if settings.zoe_cache_size is not None:
        options += ["--cache-size", settings.zoe_cache_size]
    if settings.zoe_compression is not None:
        options += ["--compression", settings.zoe_compression]
    return options


def pinning() -> str:
    """How the processes of a run are pinned to CPUs."""
    if sys.platform == "darwin":
        return "none: macOS has no CPU affinity"
    return "taskset"


def pin(cpus: str) -> list[str]:
    """The prefix of a command that runs it on the CPUs `cpus`."""
    if sys.platform == "darwin":
        return []
    return ["taskset", "-c", cpus]


def interpreter(config: StackConfig) -> Path:
    return PLONE / config.scenario / "parts" / "instance" / "bin" / "interpreter"


def zope_serve_script() -> Path:
    found = sorted(
        path
        for path in egg_paths("Zope-*")
        if (path / "Zope2" / "Startup" / "serve.py").exists()
    )
    if not found:
        raise FileNotFoundError(
            "no Zope egg under target/plone/eggs: run `just plone zeo build`"
        )
    return found[-1] / "Zope2" / "Startup" / "serve.py"


def egg_paths(pattern: str) -> list[Path]:
    """The eggs of the buildout scenarios that match `pattern`. Buildout
    puts them in target/plone/eggs, or in a subdirectory per Python ABI
    such as eggs/cp313."""
    eggs = PLONE / "eggs"
    return [Path(p) for p in glob.glob(str(eggs / pattern))] + [
        Path(p) for p in glob.glob(str(eggs / "*" / pattern))
    ]


def zoe_binary() -> Path:
    """The `zoe` binary of `zoe serve`. The file can be missing:
    `missing_prerequisites` says how to build it."""
    return servers.zoe_path()


def missing_prerequisites(config: StackConfig) -> list[str]:
    missing = []
    if not interpreter(config).exists():
        missing.append(
            f"{interpreter(config)}: run `just plone {config.scenario} build`"
        )
    if config.server == "zeo" and not interpreter(STACKS["zeo"]).exists():
        missing.append("the zeo scenario: run `just plone zeo build`")
    if config.server == "zoe":
        try:
            servers.zoe_binary()
        except FileNotFoundError as error:
            missing.append(str(error))
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
