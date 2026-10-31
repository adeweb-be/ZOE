"""The helpers that the tests of the native client share.

pytest puts this directory on `sys.path` when it imports a test module of
it, so a test module of this directory imports this module as
`client_support`. The helpers of every test directory are in `support`.
"""

from support import TIMEOUT, metrics, native

from zoe.testing import ZoeServer


def blob_native(server, blob_dir, shared=False, **kw):
    """Open a native client of the storage 1 of `server`, with the blob
    directory `blob_dir`. With `shared`, the client shares the directory
    with the server."""
    return native(server, blob_dir=str(blob_dir), shared_blob_dir=shared, **kw)


class AdminServer(ZoeServer):
    """`zoe serve` with an admin port on 127.0.0.1, for a test that reads
    `GET /metrics`.

    It takes the arguments of `ZoeServer`. A start fails when the server
    does not listen within `TIMEOUT` seconds. Each start has a new admin
    port.
    """

    def __init__(self, path, args=(), **kw):
        args = ["--admin-listen", "127.0.0.1:0", *args]
        super().__init__(str(path), args=args, **kw)

    def start(self, timeout=TIMEOUT):
        return super().start(timeout)

    def metric(self, name):
        """Return the sample of the metric `name` for the storage 1."""
        text = metrics(self)
        prefix = name + '{storage="1"} '
        for line in text.splitlines():
            if line.startswith(prefix):
                return float(line[len(prefix) :])
        raise AssertionError(f"/metrics has no sample of {name} for the storage 1")

    def loads(self):
        """Return the number of `loadBefore` calls of the storage 1."""
        return int(self.metric("zoe_loads_total"))
