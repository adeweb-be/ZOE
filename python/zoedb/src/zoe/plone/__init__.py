"""The Plone integration. Only the `plone` extra imports Plone.

Plone loads `configure.zcml` of this package through the plone.autoinclude
entry point of zoedb. `uidgenerator` gives `portal_uidgenerator` its ids
from a sequence of the store. `report` marks the transaction of each
request with its method, for the writes-on-GET report of `zoe serve`.
`reader` gives the reader of ZOE to the connections of the main database
on a ZOE storage.

The backends are opt-in, each with its own ZCML file: `locking.zcml` keeps
the locks of plone.locking in the ephemeral namespace of the store, and
`indexing.zcml` writes the catalog operations of a save to a durable
namespace, which one indexer applies (`indexing`), `registry.zcml`
lets the records of a registry move to a durable namespace (`registry`),
`catalog.zcml` answers `Catalog.search` from the catalog mirror of the
store (`catalog`), and `security.zcml` keeps `allowedRolesAndUsers` as
inherited inputs, so a share or a transition of a folder writes the entry
of the folder alone (`security`).

`text.zcml`, which `configure.zcml` includes, registers the index class
`ZoeTextIndex` of `textindex` (`register_text_index`) and the profiles
`zoe.plone:text` and `zoe.plone:text-uninstall`. `ZoeTextIndex` needs the
backend `catalog`.
"""

import os

__all__ = ["backends_from_environment", "register_text_index"]

# The meta type of the text index.
_TEXT_META_TYPE = "ZoeTextIndex"


def register_text_index():
    """Register the index class `ZoeTextIndex` with its meta type, so
    `ZCatalog.addIndex` and `catalog.xml` can add it. A second call does
    nothing."""
    import Products
    from zope.interface import implementedBy

    from zoe.plone.textindex import ZoeTextIndex

    entries = getattr(Products, "meta_types", ())
    if any(entry.get("name") == _TEXT_META_TYPE for entry in entries):
        return
    Products.meta_types = (
        *entries,
        {
            "name": _TEXT_META_TYPE,
            "action": "",
            "product": "zoe.plone",
            "permission": "Add Pluggable Index",
            "visibility": None,
            "interfaces": tuple(implementedBy(ZoeTextIndex)),
            "instance": ZoeTextIndex,
            "container_filter": None,
        },
    )


def backends_from_environment():
    """Install the backends that `ZOE_PLONE_BACKENDS` names, separated by
    commas: `locking`, `indexing`, `registry`, `catalog` and `security`.
    `configure.zcml` calls it when the ZCML loads, after `<environment>` of
    `zope.conf` set the variable. An unknown name raises `ValueError`."""
    for name in filter(None, os.environ.get("ZOE_PLONE_BACKENDS", "").split(",")):
        name = name.strip()
        if name == "locking":
            from zoe.plone import locking

            locking.install()
        elif name == "indexing":
            from zoe.plone import indexing

            indexing.install()
        elif name == "registry":
            from zoe.plone import registry

            registry.install()
        elif name == "catalog":
            from zoe.plone import catalog

            catalog.install()
        elif name == "security":
            from zoe.plone import security

            security.install()
        else:
            raise ValueError(
                f"no backend {name!r} of zoe.plone:"
                " use locking, indexing, registry, catalog or security"
            )
