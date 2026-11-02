import importlib
import importlib.metadata
import subprocess
import sys
import sysconfig

import pytest

import zoe

MODULES = [
    "zoe.storage",
    "zoe.serve",
    "zoe.zeo",
    "zoe.zodb",
    "zoe.testing",
    "zoe.kv",
    "zoe.plone",
    "zoe.plone.uidgenerator",
    "zoe.plone.report",
    "zoe.plone.locking",
    "zoe.plone.reader",
    "zoe.reader",
]

FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))


def test_version_matches_the_distribution():
    assert zoe.__version__ == importlib.metadata.version("zoedb")


@pytest.mark.parametrize("name", MODULES)
def test_module_imports(name):
    importlib.import_module(name)


def test_the_uri_extra_requires_zodburi():
    # The dev group of the workspace installs zodburi, so no other test sees
    # a change of the extra `uri` in the metadata of the wheel.
    from packaging.requirements import Requirement

    found = []
    for text in importlib.metadata.requires("zoedb") or ():
        requirement = Requirement(text)
        marker = requirement.marker
        if marker is not None and marker.evaluate({"extra": "uri"}):
            if not marker.evaluate({"extra": ""}):
                found.append(f"{requirement.name}{requirement.specifier}")
    assert found == ["zodburi>=3"]


@pytest.mark.skipif(not FREE_THREADED, reason="not a free-threaded build")
def test_extension_keeps_the_gil_off():
    # A new interpreter imports `zoe` and nothing else. In this process,
    # other tests import ZODB, and the `persistent` module that ZODB loads
    # turns the GIL back on.
    code = "import sys, zoe; print(sys._is_gil_enabled())"
    result = subprocess.run(
        [sys.executable, "-W", "error", "-c", code],
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout.strip() == "False"
