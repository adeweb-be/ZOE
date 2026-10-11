"""ZOE, the Zope Object Engine: a ZODB storage written in Rust."""

from zoe._zoe import __version__

__all__ = ["Storage", "WrongThread", "__version__"]


def __getattr__(name):
    # `zoe.storage` imports ZODB. Import it at the first use, so that
    # `import zoe` does not load ZODB. On CPython 3.14t, ZODB loads
    # `persistent`, which turns the GIL back on.
    if name in ("Storage", "WrongThread"):
        from zoe import storage

        return getattr(storage, name)
    raise AttributeError(f"module 'zoe' has no attribute {name!r}")
