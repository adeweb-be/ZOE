"""Acceptance test of the dependencies of the `plone` extra of zoedb.

zoe serve speaks only the msgpack protocol M5 of ZEO. A stock ZEO client
needs the `msgpack` package for it. So `zoedb[plone]` installs `msgpack`.

The test reads the metadata of the installed zoedb distribution.
"""

import importlib.metadata

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name


def extra_requirements(extra):
    """Return the names of the requirements that `extra` adds."""
    names = set()
    for line in importlib.metadata.requires("zoedb") or []:
        requirement = Requirement(line)
        marker = requirement.marker
        if marker is None:
            continue
        if marker.evaluate({"extra": extra}) and not marker.evaluate({"extra": ""}):
            names.add(canonicalize_name(requirement.name))
    return names


def test_the_plone_extra_names_msgpack():
    assert "msgpack" in extra_requirements("plone")


def test_the_plone_extra_still_names_plone():
    assert "products-cmfplone" in extra_requirements("plone")
