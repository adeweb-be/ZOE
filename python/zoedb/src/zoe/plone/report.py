"""The mark of the writes-on-GET report (F32).

A GET or a HEAD request should write nothing. Plone and its add-ons write
on some of them: a lock of an edit form, a last-login time, a lazy
migration. `zoe serve` counts the records of each transaction that a GET or
a HEAD request commits, by class, in the counter `zoe_writes_on_get_total`
of `GET /metrics`.

`mark(transaction, method)` marks a transaction as the transaction of a
request of `method`: it sets the key `zoe.request_method` of the extension
of the transaction. The extension goes to the server with `tpc_begin`, for
a stock `ZEO.ClientStorage` too, and the transaction row of the store keeps
it. `request_started` is the subscriber of `ZPublisher.pubevents.PubStart`:
Zope begins the transaction of each attempt of a request just before the
event, so the subscriber marks that transaction.

A write of a KV namespace is no record, so it counts nothing.

Only the subscriber reads a request. This module imports no Plone.
"""

import transaction

__all__ = ["KEY", "mark", "request_started"]

# The key of the transaction extension that names the method of the
# request.
KEY = "zoe.request_method"


def mark(txn, method):
    """Mark `txn` as the transaction of a request of `method`."""
    txn.extension[KEY] = str(method).upper()


def request_started(event):
    """Mark the current transaction with the method of the request of
    `event`, a `PubStart` of ZPublisher."""
    method = getattr(event.request, "method", None)
    if method:
        mark(transaction.get(), method)
