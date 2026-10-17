"""The Plone integration. Only the `plone` extra imports Plone.

Plone loads `configure.zcml` of this package through the plone.autoinclude
entry point of zoedb. `uidgenerator` gives `portal_uidgenerator` its ids
from a sequence of the store. `report` marks the transaction of each
request with its method, for the writes-on-GET report of `zoe serve`.

Two backends are opt-in, each with its own ZCML file: `locking.zcml` keeps
the locks of plone.locking in the ephemeral namespace of the store
(`locking`), and `registry.zcml` keeps the records of plone.registry in its
durable namespace (`registry`).
"""

import os

__all__ = ["backends_from_environment"]


def backends_from_environment(event=None):
    """Install the backends that `ZOE_PLONE_BACKENDS` names, separated by
    commas: `locking`, `registry`. The subscriber of `IProcessStarting`
    calls it when Zope starts, so `<environment>` of `zope.conf` can set
    it. An unknown name raises `ValueError`."""
    for name in filter(None, os.environ.get("ZOE_PLONE_BACKENDS", "").split(",")):
        name = name.strip()
        if name == "locking":
            from zoe.plone import locking

            locking.install()
        elif name == "registry":
            from zoe.plone import registry

            registry.install()
        else:
            raise ValueError(
                f"no backend {name!r} of zoe.plone: use locking or registry"
            )
