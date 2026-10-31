"""Unit tests of the data manager of the locking backend of `zoe.plone`,
without Plone.

The rows of the locks of a transaction join it as a data manager that
writes nothing itself. A transaction that is not active, as in an
after-commit hook, takes no data manager: a second data manager of a
transaction that already committed does not join it, and does not raise.
"""

import transaction
import ZODB

import zoe
from zoe.plone import locking


def test_a_locking_data_manager_joined_twice_to_a_transaction_does_not_raise(
    tmp_path,
):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        txn = manager.begin()
        locking._Rows(connection, txn)
        # The transaction catches the errors of an after-commit hook, so the
        # hook keeps them.
        errors = []
        second = []

        def after_commit(status):
            try:
                second.append(locking._Rows(connection, txn))
            except Exception as error:
                errors.append(error)

        txn.addAfterCommitHook(after_commit)
        manager.commit()
        assert errors == []
        assert len(second) == 1
        assert not second[0].joined
        connection.close()
    finally:
        db.close()
