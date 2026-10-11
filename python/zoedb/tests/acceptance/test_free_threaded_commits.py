"""Acceptance test of free-threaded commits through the in-process storage.

Many threads commit through `zoe.Storage`, each on its own counter and on
counters that it shares with other threads. After each round, the test
checks that the serials increase, that no update is lost and that the
lock table is empty.

The test runs on every interpreter and reports whether the GIL is on after
its imports. It asserts the GIL state only on a free-threaded build, and
only for an import of `zoe` alone: the released `persistent` turns the GIL
back on when ZODB imports it.

Each test imports ZODB, transaction and persistent itself, so that the
module collects before these packages are installed.
"""

import collections
import os
import subprocess
import sys
import sysconfig
import threading

import pytest

import zoe

FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
# The number of threads that commit.
THREADS = 32
# The number of rounds. The test checks the store after each round.
ROUNDS = 5
# The number of counters that the threads share.
SHARED = 8
# The largest number of attempts for one change. A conflict costs one.
ATTEMPTS = 1000
# The longest time that a thread waits for the others.
TIMEOUT = 120
# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")


def gil_enabled():
    """Tell if the GIL is on now."""
    return getattr(sys, "_is_gil_enabled", lambda: True)()


def shared_keys(index):
    """Return the shared counters of a thread. Neighbours share one."""
    return [f"shared-{index % SHARED}", f"shared-{(index + 1) % SHARED}"]


def expected_values(rounds):
    """Return the value of each counter after a number of rounds."""
    values = collections.Counter()
    for index in range(THREADS):
        values[f"own-{index}"] += rounds
        for key in shared_keys(index):
            values[key] += rounds
    return values


@pytest.mark.skipif(not FREE_THREADED, reason="the GIL is always on in this build")
def test_importing_zoe_alone_keeps_the_gil_off():
    # A new interpreter imports nothing but zoe. PYTHON_GIL must not force
    # the result.
    environment = {k: v for k, v in os.environ.items() if k != "PYTHON_GIL"}
    code = "import sys, zoe, zoe._zoe; print(sys._is_gil_enabled())"
    result = subprocess.run(
        [sys.executable, "-c", code],
        env=environment,
        capture_output=True,
        text=True,
        check=True,
    )
    assert result.stdout.strip() == "False", result.stderr


def test_threads_commit_without_lost_updates(tmp_path, capsys, record_property):
    import transaction
    import ZODB
    from persistent.mapping import PersistentMapping
    from ZODB.POSException import ConflictError

    gil = gil_enabled()
    record_property("gil_enabled", gil)
    with capsys.disabled():
        print(f"\nThe GIL is {'on' if gil else 'off'} after the imports of the test.")

    storage = zoe.Storage(str(tmp_path / "store"))
    db = ZODB.DB(storage, pool_size=THREADS + 1)
    try:
        with db.transaction() as conn:
            for name in expected_values(0):
                conn.root()[name] = PersistentMapping(value=0)

        barrier = threading.Barrier(THREADS + 1, timeout=TIMEOUT)
        errors = []
        own_attempts = []

        def change(tm, counters):
            """Add one to each counter in one transaction.

            Start again after each conflict. Return the number of attempts.
            """
            for attempt in range(1, ATTEMPTS + 1):
                tm.begin()
                for counter in counters:
                    counter["value"] += 1
                try:
                    tm.commit()
                except ConflictError:
                    tm.abort()
                else:
                    return attempt
            raise AssertionError(f"no commit after {ATTEMPTS} attempts")

        def work(index):
            tm = transaction.TransactionManager()
            conn = db.open(tm)
            try:
                root = conn.root()
                own = [root[f"own-{index}"]]
                shared = [root[key] for key in shared_keys(index)]
                for _ in range(ROUNDS):
                    barrier.wait()
                    own_attempts.append(change(tm, own))
                    change(tm, shared)
                    barrier.wait()
            except Exception as error:
                errors.append(error)
                barrier.abort()
            finally:
                tm.abort()
                conn.close()

        threads = [
            threading.Thread(target=work, args=(index,), daemon=True)
            for index in range(THREADS)
        ]
        for thread in threads:
            thread.start()

        check_tm = transaction.TransactionManager()
        check = db.open(check_tm)
        serials = {}
        last = storage.lastTransaction()
        try:
            for done in range(1, ROUNDS + 1):
                try:
                    barrier.wait()  # The round starts.
                    barrier.wait()  # The round ends.
                except threading.BrokenBarrierError:
                    causes = [
                        error
                        for error in errors
                        if not isinstance(error, threading.BrokenBarrierError)
                    ]
                    pytest.fail(f"a thread failed in round {done}: {causes!r}")

                check_tm.begin()
                for name, value in expected_values(done).items():
                    counter = check.root()[name]
                    # No update is lost.
                    assert counter["value"] == value, (done, name)
                    # Each update made one revision, with a tid larger than
                    # the tid of the revision before it.
                    tids = [
                        entry["tid"]
                        for entry in storage.history(counter._p_oid, value + 10)
                    ]
                    assert len(tids) == value + 1, (done, name)
                    assert tids == sorted(set(tids), reverse=True), (done, name)
                    serial = tuple(storage.loadBefore(counter._p_oid, MAX_TID))[1]
                    assert serial > serials.get(name, bytes(8)), (done, name)
                    serials[name] = serial
                assert storage.lastTransaction() > last, done
                last = storage.lastTransaction()
                assert storage._lock_table_is_empty(), done
        except BaseException:
            # Release the threads that wait for the next round.
            barrier.abort()
            raise
        finally:
            for thread in threads:
                thread.join(TIMEOUT)
            check_tm.abort()
            check.close()

        assert not errors
        # A change on a counter that no other thread writes never conflicts.
        assert own_attempts == [1] * (THREADS * ROUNDS)
    finally:
        db.close()
