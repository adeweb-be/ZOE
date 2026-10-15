"""Unit tests of the bound of the disk that one value of a cell needs."""

import pytest

from shootout import FULL, KB, QUICK, Cell, disk_needed
from shootout.storages import STORAGES, empty_database

MiB = 1024 * KB


def test_the_bound_counts_the_populate_and_two_calls_of_update():
    # c=4: 500 records of the populate, 7 of the root, the tree and the
    # mappings, and 2 × 4 × 100 × 101 records of two calls of update.
    records = 500 + 7 + 80_800
    assert disk_needed("zeo", Cell("threads", 4, KB)) == records * 9 * KB + 64 * MiB


def test_above_a_concurrency_of_ten_a_call_has_thirty_transactions():
    records = 6_500 + 67 + 2 * 64 * 30 * 101
    assert (
        disk_needed("zeo", Cell("processes", 64, 100 * KB))
        == records * 108 * KB + 64 * MiB
    )


@pytest.mark.parametrize("storage", ["filestorage", "zeo", "zoe", "zoe-server"])
def test_the_bound_is_the_same_for_each_storage_with_a_local_store(storage):
    for cell in FULL.cells + QUICK.cells:
        assert disk_needed(storage, cell) == disk_needed("filestorage", cell)


def test_relstorage_needs_only_the_room_of_the_logs():
    for cell in FULL.cells:
        assert disk_needed("relstorage", cell) == 64 * MiB


@pytest.mark.parametrize("storage", ["filestorage", "zeo", "zoe", "zoe-server"])
def test_only_relstorage_has_a_database_to_empty(storage):
    # The other storages need no DSN, and nothing happens.
    empty_database(STORAGES[storage], None)


def test_relstorage_needs_a_dsn_to_empty_its_database():
    with pytest.raises(ValueError, match="DSN"):
        empty_database(STORAGES["relstorage"], None)
