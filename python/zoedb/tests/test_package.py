import importlib
import importlib.metadata
import sys
import sysconfig

import pytest

import zoe

MODULES = [
    "zoe.storage",
    "zoe.serve",
    "zoe.zeo",
    "zoe.zodb",
    "zoe.asyncio",
    "zoe.testing",
    "zoe.plone",
]

FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))


def test_version_matches_the_distribution():
    assert zoe.__version__ == importlib.metadata.version("zoedb")


@pytest.mark.parametrize("name", MODULES)
def test_module_imports(name):
    importlib.import_module(name)


@pytest.mark.skipif(not FREE_THREADED, reason="not a free-threaded build")
def test_extension_keeps_the_gil_off():
    # The module imports `zoe`, so the extension is already loaded here.
    assert not sys._is_gil_enabled()
