"""Acceptance test: persistent, BTrees and zodbpickle keep the GIL off on 3.14t."""

import sys
import sysconfig
import threading

import pytest

pytestmark = pytest.mark.skipif(
    not sysconfig.get_config_var("Py_GIL_DISABLED"),
    reason="needs a free-threaded build",
)

THREADS = 32
KEYS = 10_000


def test_imports_keep_the_gil_off():
    import BTrees.IIBTree  # noqa: F401
    import BTrees.Length  # noqa: F401
    import BTrees.OOBTree  # noqa: F401
    import persistent  # noqa: F401
    import persistent.mapping  # noqa: F401
    import zodbpickle.pickle  # noqa: F401

    assert not sys._is_gil_enabled()


def test_threads_use_the_packages_concurrently():
    import zodbpickle.pickle
    from BTrees.IIBTree import IITreeSet
    from BTrees.OOBTree import OOBTree
    from persistent.mapping import PersistentMapping

    errors = []

    def work(n):
        try:
            tree = OOBTree()
            treeset = IITreeSet()
            mapping = PersistentMapping()
            for i in range(KEYS):
                tree[(n, i)] = i
                treeset.add(i)
                mapping[i] = n
            assert len(tree) == KEYS
            assert len(treeset) == KEYS
            data = zodbpickle.pickle.dumps(dict(mapping), protocol=3)
            assert zodbpickle.pickle.loads(data) == dict(mapping)
        except Exception as exc:  # noqa: BLE001
            errors.append(exc)

    threads = [threading.Thread(target=work, args=(n,)) for n in range(THREADS)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    assert not sys._is_gil_enabled()
