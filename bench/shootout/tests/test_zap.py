"""A zap of zodbshootout empties a zoe store and frees it at once.

zodbshootout zaps the storage before each sample of `add`. The zap closes
the database and calls `cleanup` of the storage.
"""

import gc

from zodbshootout._dbsupport import BenchmarkDBFactory, get_databases_from_string

from shootout.storages import STORAGES, zconfig


def test_a_zap_empties_a_zoe_store_and_frees_it(tmp_path):
    with zconfig(STORAGES["zoe"], tmp_path, None) as text:
        [config] = get_databases_from_string(text)
        factory = BenchmarkDBFactory(config, 1, 1, can_zap=True)
        db = factory.open()
        with db.transaction() as connection:
            connection.root()["value"] = 1
        # Only a close can free the store: the collector does not run.
        gc.disable()
        try:
            # These are the steps of zodbshootout before an add sample.
            db.close()
            db.speedtest_zap_all()
            db = factory.open()
            try:
                with db.transaction() as connection:
                    assert "value" not in connection.root()
            finally:
                db.close()
        finally:
            gc.enable()
