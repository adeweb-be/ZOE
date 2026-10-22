use std::borrow::Cow;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use redb::StorageBackend;

use super::*;
use crate::backend::Record;
use crate::clock::SystemClock;
use crate::guard::SizeLimit;
use crate::store::Options;
use crate::testing::{Captured, commit, commit_all, events_of, open_with_clock};
use crate::vfs::{MemVfs, Vfs};

/// Begin a restore at `tid` that restores `data` for `oid`, and vote.
fn voted_restore<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    txn.restore(oid, Some(data)).unwrap();
    txn.vote().unwrap();
    txn
}

#[test]
fn a_restore_over_an_existing_oid_replaces_it() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let tid = Tid(first.0 + 10);
    voted_restore(&store, tid, oid, b"b").finish().unwrap();

    assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
    let restored = store.backend.load_before(oid, Tid::MAX).unwrap().unwrap();
    // The restored record points to the record that it replaced, and
    // that record stays readable.
    assert_eq!(restored.prev, first);
    let replaced = store.backend.load_before(oid, tid).unwrap().unwrap();
    assert_eq!((replaced.tid, replaced.data), (first, b"a".to_vec()));
}

#[test]
fn a_restore_tid_behind_a_later_commit_fails_at_finish() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let early = voted_restore(&store, Tid(200), Oid(1), b"a");
    let late = voted_restore(&store, Tid(300), Oid(2), b"b");
    assert_eq!(late.finish().unwrap(), Tid(300));
    assert!(matches!(early.finish(), Err(ZoeError::Protocol(_))));
    assert!(store.locks.is_empty());

    // The store does not halt.
    assert_eq!(store.last_transaction(), Tid(300));
    voted_restore(&store, Tid(400), Oid(1), b"c")
        .finish()
        .unwrap();
    commit(&store, Oid(2), Tid(300), b"d");
}

#[test]
fn a_normal_commit_never_gets_the_maximum_tid() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    voted_restore(&store, Tid(u64::MAX - 2), oid, b"a")
        .finish()
        .unwrap();
    let tid = commit(&store, oid, Tid(u64::MAX - 2), b"b");
    assert_eq!(tid, Tid(u64::MAX - 1));
    assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
}

#[test]
fn a_commit_after_a_restore_at_the_last_tid_fails() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let last = Tid(u64::MAX - 1);
    voted_restore(&store, last, oid, b"a").finish().unwrap();

    // No tid is left. The store refuses each finish and does not halt.
    for _ in 0..2 {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, last, b"b").unwrap();
        txn.vote().unwrap();
        assert!(matches!(txn.finish(), Err(ZoeError::Protocol(_))));
        assert!(store.locks.is_empty());
    }
    assert!(store.tpc_begin_restore(Tid::MAX, b"", b"", b"").is_err());
    assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), last));
    assert_eq!(store.last_transaction(), last);
}

#[test]
fn the_next_tid_is_after_the_last_and_below_the_maximum() {
    let max = u64::MAX;
    for (now, last, tid) in [
        (10, 5, 10),
        (10, 10, 11),
        (10, 20, 21),
        (max, 5, max - 1),
        (max, max - 2, max - 1),
    ] {
        assert_eq!(
            next_tid(Tid(now), Tid(last)).unwrap(),
            Tid(tid),
            "{now} {last}"
        );
    }
    for last in [max - 1, max] {
        assert!(matches!(
            next_tid(Tid(0), Tid(last)),
            Err(ZoeError::Protocol(_))
        ));
    }
}

#[test]
fn store_refuses_an_empty_pickle() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    for mut txn in [
        store.tpc_begin(b"", b"", b"").unwrap(),
        store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap(),
    ] {
        let error = txn.store(Oid(1), Tid::ZERO, b"");
        assert!(matches!(error, Err(ZoeError::Protocol(_))));
    }
}

#[test]
fn store_in_a_restore_transaction_checks_the_serial() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    txn.restore(Oid(2), Some(b"b")).unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), Tid(100));
    assert_eq!(store.load(Oid(1)).unwrap(), (b"a".to_vec(), Tid(100)));

    let mut stale = store.tpc_begin_restore(Tid(200), b"", b"", b"").unwrap();
    stale.store(Oid(1), Tid::ZERO, b"c").unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
    assert!(store.locks.is_empty());
}

#[test]
fn the_maximum_oid_is_refused() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let error = txn.store(Oid(u64::MAX), Tid::ZERO, b"a");
    assert!(matches!(error, Err(ZoeError::Protocol(_))));
    let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
    let error = txn.restore(Oid(u64::MAX), Some(b"a"));
    assert!(matches!(error, Err(ZoeError::Protocol(_))));

    // The highest oid that a commit can write uses up the oids.
    voted_restore(&store, Tid(100), Oid(u64::MAX - 1), b"a")
        .finish()
        .unwrap();
    assert!(matches!(store.new_oid(), Err(ZoeError::Protocol(_))));
}

#[test]
fn the_maximum_tid_is_refused() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let result = store.tpc_begin_restore(Tid::MAX, b"", b"", b"");
    assert!(matches!(result, Err(ZoeError::Protocol(_))));
}

#[test]
fn restore_needs_a_restore_transaction_before_the_vote() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    assert!(matches!(
        txn.restore(Oid(1), Some(b"a")),
        Err(ZoeError::Protocol(_))
    ));

    let mut txn = voted_restore(&store, Tid(100), Oid(1), b"a");
    assert!(matches!(
        txn.restore(Oid(2), Some(b"b")),
        Err(ZoeError::Protocol(_))
    ));
}

#[test]
fn a_restored_oid_takes_an_exclusive_lock_at_the_vote() {
    let mut store = Store::open_vfs(MemVfs::new()).unwrap();
    // A zero lock timeout turns every wait into a conflict.
    store.lock_timeout = Duration::ZERO;
    let oid = Oid(1);
    let first = voted_restore(&store, Tid(100), oid, b"a");

    // Neither a store nor a second restore on the same oid gets the
    // lock while the first restore holds it.
    let mut stored = store.tpc_begin(b"", b"", b"").unwrap();
    stored.store(oid, Tid::ZERO, b"b").unwrap();
    let mut restored = store.tpc_begin_restore(Tid(200), b"", b"", b"").unwrap();
    restored.restore(oid, Some(b"c")).unwrap();
    for txn in [&mut stored, &mut restored] {
        let error = txn.vote();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid: Oid(1) })),
            "{error:?}"
        );
    }
    first.finish().unwrap();
    assert!(store.locks.is_empty());

    // The second restore read the serial before the first one finished,
    // so it cannot commit on top of it.
    assert!(matches!(restored.vote(), Err(ZoeError::Conflict { .. })));
    assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), Tid(100)));
    assert!(store.locks.is_empty());
}

#[test]
fn a_change_between_restore_and_vote_is_a_conflict() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store
        .tpc_begin_restore(Tid(u64::MAX - 1), b"", b"", b"")
        .unwrap();
    txn.restore(oid, Some(b"a")).unwrap();
    commit(&store, oid, Tid::ZERO, b"b");
    assert!(matches!(txn.vote(), Err(ZoeError::Conflict { .. })));
    assert!(store.locks.is_empty());
}

#[test]
fn new_oids_stay_above_a_restored_oid() {
    let vfs = MemVfs::new();
    {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        voted_restore(&store, Tid(100), Oid(1000), b"a")
            .finish()
            .unwrap();
        assert!(store.new_oid().unwrap() > Oid(1000));
    }
    let store = Store::open_vfs(vfs).unwrap();
    assert!(store.new_oid().unwrap() > Oid(1000));
}

#[test]
fn another_thread_cannot_store_check_restore_or_vote() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (mine, theirs) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(mine, Tid::ZERO, b"a").unwrap();
    let mut restore = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
    thread::scope(|scope| {
        scope.spawn(|| {
            let refused = [
                txn.store(theirs, Tid::ZERO, b"b"),
                txn.check_current_serial(theirs, Tid(5)),
                txn.vote(),
                txn.vote_resolving().map(drop),
                restore.restore(theirs, Some(b"c")),
            ];
            for result in refused {
                assert!(matches!(result, Err(ZoeError::WrongThread)), "{result:?}");
            }
        });
    });
    assert!(store.lock_table_is_empty());

    // The refused calls changed nothing: the stale read check of the
    // other thread does not fail the vote, and only `mine` commits.
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(mine).unwrap(), (b"a".to_vec(), tid));
    assert!(matches!(store.load(theirs), Err(ZoeError::NotFound { .. })));
    restore.abort();
}

#[test]
fn another_thread_can_finish_or_abort() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut aborted = store.tpc_begin(b"", b"", b"").unwrap();
    aborted.store(oid, Tid::ZERO, b"a").unwrap();
    aborted.vote().unwrap();
    thread::scope(|scope| {
        scope.spawn(move || aborted.abort());
    });
    assert!(store.lock_table_is_empty());

    let mut finished = store.tpc_begin(b"", b"", b"").unwrap();
    finished.store(oid, Tid::ZERO, b"b").unwrap();
    finished.vote().unwrap();
    let tid = thread::scope(|scope| scope.spawn(move || finished.finish()).join())
        .unwrap()
        .unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_stale_vote_gives_the_current_and_the_read_serial() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (read, written) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit(&store, read, Tid::ZERO, b"a");
    let second = commit(&store, read, first, b"b");

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.check_current_serial(read, first).unwrap();
    txn.store(written, Tid::ZERO, b"c").unwrap();
    assert!(matches!(txn.vote(), Err(ZoeError::ReadConflict { oid }) if oid == read));
    assert_eq!(txn.conflict_serials(), Some((second, first)));

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(read, first, b"c").unwrap();
    assert!(matches!(txn.vote(), Err(ZoeError::Conflict { oid }) if oid == read));
    assert_eq!(txn.conflict_serials(), Some((second, first)));
    let staged = txn.staged(read).unwrap();
    assert_eq!(staged, Some((first, Cow::Borrowed(&b"c"[..]))));
    assert_eq!(txn.staged(written).unwrap(), None);

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(read, second, b"c").unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.conflict_serials(), None);
}

#[test]
fn a_lock_timeout_gives_no_serials() {
    let mut store = Store::open_vfs(MemVfs::new()).unwrap();
    store.lock_timeout = Duration::ZERO;
    let oid = store.new_oid().unwrap();
    let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
    holder.store(oid, Tid::ZERO, b"a").unwrap();
    holder.vote().unwrap();
    let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
    waiter.store(oid, Tid::ZERO, b"b").unwrap();
    assert!(matches!(waiter.vote(), Err(ZoeError::Conflict { .. })));
    assert_eq!(waiter.conflict_serials(), None);
}

#[test]
fn a_cancel_ends_the_lock_wait_of_a_vote_from_another_thread() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let low = store.new_oid().unwrap();
    let high = store.new_oid().unwrap();
    let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
    holder.store(high, Tid::ZERO, b"held").unwrap();
    holder.vote().unwrap();
    let cancel = LockCancel::new();
    thread::scope(|scope| {
        let voter = scope.spawn(|| {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.set_lock_cancel(cancel.clone());
            txn.store(low, Tid::ZERO, b"gone").unwrap();
            txn.store(high, Tid::ZERO, b"gone").unwrap();
            let voted = txn.vote();
            (voted, txn.conflict_serials())
        });
        // The voter holds the lock of `low` and waits for `high`.
        while store.locks.waiting(high) == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        cancel.cancel();
        let (voted, serials) = voter.join().unwrap();
        assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == high));
        assert_eq!(serials, None);
    });
    // The vote released the lock of `low`.
    commit(&store, low, Tid::ZERO, b"after");
    holder.finish().unwrap();
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_cancel_does_nothing_to_a_vote_that_passed() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let cancel = LockCancel::new();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.set_lock_cancel(cancel.clone());
    txn.store(oid, Tid::ZERO, b"a").unwrap();
    txn.vote().unwrap();
    cancel.cancel();
    assert!(!store.lock_table_is_empty());
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"a".to_vec(), tid));
    assert!(store.lock_table_is_empty());
}

#[test]
fn each_commit_names_the_commit_before_it() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, first, b"b").unwrap();
    txn.vote().unwrap();
    let finished = txn.finish_after().unwrap();
    assert_eq!(finished.previous, first);
    assert!(finished.tid > first);
    assert_eq!(store.last_transaction(), finished.tid);
}

#[test]
fn a_transaction_that_shares_the_store_borrows_nothing() {
    let store = Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
    let oid = store.new_oid().unwrap();
    let mut txn = Txn::begin(Arc::clone(&store), b"user", b"", b"");
    txn.store(oid, Tid::ZERO, b"a").unwrap();
    txn.vote().unwrap();
    assert!(!store.lock_table_is_empty());
    drop(txn);
    assert!(store.lock_table_is_empty());

    let mut txn = Txn::begin(Arc::clone(&store), b"user", b"", b"");
    txn.store(oid, Tid::ZERO, b"b").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
    assert!(store.lock_table_is_empty());
    assert_eq!(Arc::strong_count(&store), 1);
}

#[test]
fn a_resolving_vote_gives_the_committed_record_and_keeps_the_locks() {
    let mut store = Store::open_vfs(MemVfs::new()).unwrap();
    // A zero lock timeout turns every wait into a conflict.
    store.lock_timeout = Duration::ZERO;
    let (stale, fresh) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit(&store, stale, Tid::ZERO, b"a");
    let second = commit(&store, stale, first, b"b");

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(stale, first, b"c").unwrap();
    txn.store(fresh, Tid::ZERO, b"d").unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    let committed = Record {
        tid: second,
        prev: first,
        data: b"b".to_vec(),
    };
    assert_eq!(conflicts, [(stale, committed)]);
    let staged = txn.staged(stale).unwrap();
    assert_eq!(staged, Some((first, Cow::Borrowed(&b"c"[..]))));
    assert_eq!(txn.conflict_serials(), None);

    // The transaction keeps the lock: another vote on the oid cannot
    // take it.
    let mut other = store.tpc_begin(b"", b"", b"").unwrap();
    other.store(stale, second, b"e").unwrap();
    assert!(matches!(other.vote(), Err(ZoeError::Conflict { .. })));
    assert_eq!(other.conflict_serials(), None);

    // The transaction stores the resolved pickle over the committed
    // serial, and votes again.
    txn.store(stale, second, b"bc").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(stale).unwrap(), (b"bc".to_vec(), tid));
    assert_eq!(store.load(fresh).unwrap(), (b"d".to_vec(), tid));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_resolving_vote_without_conflicts_is_a_vote() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, first, b"b").unwrap();
    assert!(txn.vote_resolving().unwrap().is_empty());
    assert!(matches!(
        txn.store(oid, first, b"c"),
        Err(ZoeError::Protocol(_))
    ));
    assert!(matches!(txn.vote(), Err(ZoeError::Protocol(_))));
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"b".to_vec(), tid));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_conflict_left_unresolved_fails_the_second_vote() {
    let mut store = Store::open_vfs(MemVfs::new()).unwrap();
    store.lock_timeout = Duration::ZERO;
    let (low, high) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit_all(&store, &[(low, Tid::ZERO, b"a"), (high, Tid::ZERO, b"a")]);
    let second = commit_all(&store, &[(low, first, b"b"), (high, first, b"b")]);

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(low, first, b"c").unwrap();
    txn.store(high, first, b"c").unwrap();
    let conflicts = txn.vote_resolving().unwrap();
    let oids: Vec<_> = conflicts.iter().map(|&(oid, _)| oid).collect();
    assert_eq!(oids, [low, high]);

    // The transaction resolves only `low`.
    txn.store(low, second, b"bc").unwrap();
    let error = txn.vote();
    assert!(
        matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
        "{error:?}"
    );
    assert_eq!(txn.conflict_serials(), Some((second, first)));
    assert!(store.lock_table_is_empty());

    // The failed vote released the locks, so another transaction
    // commits at once.
    commit(&store, high, second, b"d");
    txn.abort();
}

#[test]
fn a_read_current_conflict_is_never_resolved() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (written, read) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit_all(
        &store,
        &[(written, Tid::ZERO, b"a"), (read, Tid::ZERO, b"a")],
    );
    let second = commit_all(&store, &[(written, first, b"b"), (read, first, b"b")]);

    // A stale read-current oid fails the vote, even when the stale
    // written oid can be resolved.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(written, first, b"c").unwrap();
    txn.check_current_serial(read, first).unwrap();
    let error = txn.vote_resolving();
    assert!(
        matches!(error, Err(ZoeError::ReadConflict { oid }) if oid == read),
        "{error:?}"
    );
    assert_eq!(txn.conflict_serials(), Some((second, first)));
    assert!(store.lock_table_is_empty());

    // A read-current oid alone fails the same way.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(written, second, b"c").unwrap();
    txn.check_current_serial(read, first).unwrap();
    assert!(matches!(
        txn.vote_resolving(),
        Err(ZoeError::ReadConflict { .. })
    ));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_conflict_without_an_old_and_a_committed_state_cannot_be_resolved() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (created, deleted, restored, missing) = (Oid(1), Oid(2), Oid(3), Oid(4));
    let first = commit_all(
        &store,
        &[
            (created, Tid::ZERO, b"a"),
            (deleted, Tid::ZERO, b"a"),
            (restored, Tid::ZERO, b"a"),
        ],
    );
    let mut delete = store
        .tpc_begin_restore(Tid(first.0 + 1), b"", b"", b"")
        .unwrap();
    delete.restore(deleted, None).unwrap();
    delete.vote().unwrap();
    let gone = delete.finish().unwrap();

    // The transaction read no serial of `created`, `deleted` has no
    // committed pickle and `missing` has no record. A restored record
    // is a copy.
    let mut copy = store
        .tpc_begin_restore(Tid(u64::MAX - 1), b"", b"", b"")
        .unwrap();
    copy.restore(restored, Some(b"b")).unwrap();
    let changed = commit(&store, restored, first, b"c");
    for (oid, read, current, txn) in [
        (created, Tid::ZERO, first, None),
        (deleted, first, gone, None),
        (missing, first, Tid::ZERO, None),
        (restored, first, changed, Some(copy)),
    ] {
        let mut txn = txn.unwrap_or_else(|| {
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(oid, read, b"d").unwrap();
            txn
        });
        let error = txn.vote_resolving();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid: failed }) if failed == oid),
            "{oid:?} {error:?}"
        );
        assert_eq!(txn.conflict_serials(), Some((current, read)), "{oid:?}");
        assert!(store.lock_table_is_empty());
    }
}

#[test]
fn calls_between_the_two_votes() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (stale, other) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit(&store, stale, Tid::ZERO, b"a");
    let second = commit(&store, stale, first, b"b");
    let resolving = || {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(stale, first, b"c").unwrap();
        assert_eq!(txn.vote_resolving().unwrap().len(), 1);
        txn
    };

    // Between the votes, the transaction takes only a store of an oid
    // in conflict.
    let mut txn = resolving();
    for refused in [
        txn.store(other, Tid::ZERO, b"d"),
        txn.check_current_serial(other, Tid::ZERO),
        txn.vote_resolving().map(drop),
    ] {
        assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    }
    txn.store(stale, second, b"bc").unwrap();
    txn.vote().unwrap();
    assert!(matches!(
        txn.store(stale, second, b"bcd"),
        Err(ZoeError::Protocol(_))
    ));
    txn.abort();
    assert!(store.lock_table_is_empty());

    // The transaction cannot finish before the second vote. The failed
    // finish releases the locks.
    let txn = resolving();
    assert!(matches!(txn.finish(), Err(ZoeError::Protocol(_))));
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_lock_timeout_in_a_resolving_vote_gives_no_serials() {
    let mut store = Store::open_vfs(MemVfs::new()).unwrap();
    store.lock_timeout = Duration::ZERO;
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
    holder.store(oid, first, b"b").unwrap();
    holder.vote().unwrap();
    let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
    waiter.store(oid, Tid::ZERO, b"c").unwrap();
    assert!(matches!(
        waiter.vote_resolving(),
        Err(ZoeError::Conflict { .. })
    ));
    assert_eq!(waiter.conflict_serials(), None);
    holder.abort();
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_waiting_vote_gets_the_lock_after_the_resolved_commit() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let second = commit(&store, oid, first, b"b");
    let mut resolving = store.tpc_begin(b"", b"", b"").unwrap();
    resolving.store(oid, first, b"c").unwrap();
    assert_eq!(resolving.vote_resolving().unwrap().len(), 1);

    let (voted, done) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut later = store.tpc_begin(b"", b"", b"").unwrap();
            later.store(oid, second, b"d").unwrap();
            voted
                .send(later.vote_resolving().map(|conflicts| conflicts.len()))
                .unwrap();
        });
        // The later vote waits while the first transaction resolves.
        assert!(done.recv_timeout(Duration::from_millis(100)).is_err());
        resolving.store(oid, second, b"bc").unwrap();
        resolving.vote().unwrap();
        resolving.finish().unwrap();
        // Then the later vote gets the lock and sees the resolved
        // commit as a conflict.
        assert_eq!(done.recv().unwrap().unwrap(), 1);
    });
    assert!(store.lock_table_is_empty());
}

/// Open a store in a new filesystem. Its transactions keep `memory`
/// bytes of pickles in memory and spill the rest.
fn spilling_store(memory: usize) -> (MemVfs, Store) {
    let vfs = MemVfs::new();
    let options = Options {
        staging_memory: memory,
        ..Options::default()
    };
    let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
    (vfs, store)
}

/// Tell if `vfs` holds a spill file.
fn spills(vfs: &MemVfs) -> bool {
    vfs.names().iter().any(|name| name.starts_with("staging/"))
}

#[test]
fn staged_reads_a_pickle_in_memory_or_in_the_spill_file() {
    let (vfs, store) = spilling_store(1);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    txn.store(Oid(2), Tid(5), b"bb").unwrap();
    assert_eq!(txn.staged_in_memory(), 1);
    assert!(spills(&vfs));

    let (serial, data) = txn.staged(Oid(1)).unwrap().unwrap();
    assert!(matches!(data, Cow::Borrowed(b"a")));
    assert_eq!(serial, Tid::ZERO);
    let (serial, data) = txn.staged(Oid(2)).unwrap().unwrap();
    assert_eq!((serial, &*data), (Tid(5), &b"bb"[..]));
    assert!(txn.staged(Oid(3)).unwrap().is_none());
}

#[test]
fn the_vote_checks_the_serials_of_spilled_records() {
    let (vfs, store) = spilling_store(0);
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
    stale.store(oid, Tid::ZERO, b"b").unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
    assert!(store.locks.is_empty());
    drop(stale);
    assert!(!spills(&vfs));

    let tid = commit(&store, oid, first, b"c");
    assert_eq!(store.load(oid).unwrap(), (b"c".to_vec(), tid));
}

#[test]
fn a_restore_transaction_commits_spilled_records() {
    let (vfs, store) = spilling_store(0);
    let first = commit(&store, Oid(1), Tid::ZERO, b"a");
    let tid = Tid(first.0 + 10);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    txn.restore(Oid(1), None).unwrap();
    txn.restore(Oid(2), Some(b"b")).unwrap();
    txn.store(Oid(3), Tid::ZERO, b"c").unwrap();
    assert_eq!(txn.staged_in_memory(), 0);
    assert!(spills(&vfs));
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);

    assert!(!spills(&vfs));
    assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
    assert_eq!(store.load(Oid(2)).unwrap(), (b"b".to_vec(), tid));
    assert_eq!(store.load(Oid(3)).unwrap(), (b"c".to_vec(), tid));
    assert!(store.new_oid().unwrap() > Oid(3));
}

/// A blob stays with its oid when the pickle spills, and when a later
/// `store` of the oid spills a new pickle.
#[test]
fn a_spilled_record_keeps_its_blob() {
    let (vfs, store) = spilling_store(0);
    let (twice, once) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for (oid, bytes) in [(twice, &b"twice"[..]), (once, b"once")] {
        let mut file = store.new_blob_file().unwrap();
        file.write_all(bytes).unwrap();
        txn.store_blob(oid, Tid::ZERO, b"first", file).unwrap();
    }
    txn.store(twice, Tid::ZERO, b"second").unwrap();
    assert!(spills(&vfs));
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert!(!spills(&vfs));
    assert_eq!(store.load(twice).unwrap(), (b"second".to_vec(), tid));
    assert_eq!(store.load(once).unwrap(), (b"first".to_vec(), tid));
    for (oid, bytes) in [(twice, &b"twice"[..]), (once, b"once")] {
        let mut read = Vec::new();
        store
            .open_blob(oid, tid)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, bytes);
    }
}

/// A conflict on a spilled record resolves as one in memory: `staged`
/// reads the pickle from the spill file, and the resolved pickle
/// replaces it.
#[test]
fn a_spilled_record_can_be_resolved() {
    let (vfs, store) = spilling_store(0);
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    let second = commit(&store, oid, first, b"b");

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, first, b"c").unwrap();
    assert!(spills(&vfs));
    let conflicts = txn.vote_resolving().unwrap();
    assert_eq!(conflicts.len(), 1);
    let (serial, data) = txn.staged(oid).unwrap().unwrap();
    assert!(matches!(data, Cow::Owned(_)));
    assert_eq!((serial, &*data), (first, &b"c"[..]));

    txn.store(oid, second, b"bc").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"bc".to_vec(), tid));
    assert!(!spills(&vfs));
}

#[test]
fn dropping_a_transaction_removes_its_spill_file() {
    let (vfs, store) = spilling_store(0);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    txn.vote().unwrap();
    assert!(spills(&vfs));
    drop(txn);
    assert!(!spills(&vfs));
    assert!(store.locks.is_empty());
}

/// A transaction that holds the last share of its store still removes
/// its spill file when it drops.
#[test]
fn the_last_share_of_the_store_removes_the_spill_file() {
    let (vfs, store) = spilling_store(0);
    let store = Arc::new(store);
    let mut txn = Txn::begin(Arc::clone(&store), b"", b"", b"");
    drop(store);
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    assert!(spills(&vfs));
    drop(txn);
    assert!(!spills(&vfs));
}

/// A finish writes only the bytes that the transaction staged. A
/// spill file that changed fails the finish before the commit.
#[test]
fn a_damaged_spill_file_fails_the_finish() {
    let (vfs, store) = spilling_store(0);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"abc").unwrap();
    txn.vote().unwrap();
    let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
    vfs.file(&format!("staging/{name}")).write(1, b"x").unwrap();

    let error = txn.finish();
    assert!(matches!(error, Err(ZoeError::Io(_))), "{error:?}");
    assert!(!spills(&vfs));
    assert!(store.locks.is_empty());
    assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
    assert_eq!(store.last_transaction(), Tid::ZERO);

    // The error came after the finish took the sequencer, so the store
    // halts until it is reopened.
    let mut next = store.tpc_begin(b"", b"", b"").unwrap();
    next.store(Oid(2), Tid::ZERO, b"b").unwrap();
    next.vote().unwrap();
    assert!(matches!(next.finish(), Err(ZoeError::Halted)));
}

/// A spill file that changed while the caller resolves a conflict fails
/// `staged` with the error of the read, never with `None`. The
/// transaction keeps its locks until the caller aborts it.
#[test]
fn a_damaged_spill_file_fails_staged_during_resolution() {
    let (vfs, store) = spilling_store(0);
    let oid = store.new_oid().unwrap();
    let first = commit(&store, oid, Tid::ZERO, b"a");
    commit(&store, oid, first, b"b");

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, first, b"c").unwrap();
    assert_eq!(txn.vote_resolving().unwrap().len(), 1);
    let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
    vfs.file(&format!("staging/{name}")).write(0, b"x").unwrap();

    let error = txn.staged(oid).unwrap_err();
    assert!(
        matches!(&error, ZoeError::Io(e) if e.kind() == std::io::ErrorKind::InvalidData),
        "{error:?}"
    );
    assert!(!store.lock_table_is_empty());
    txn.abort();
    assert!(store.lock_table_is_empty());
    assert!(!spills(&vfs));
}

/// Transactions on many threads spill at once, and some of them drop on
/// another thread. Each commit keeps its pickles, and no spill file
/// stays. Under ThreadSanitizer, this covers the spill paths.
#[test]
fn transactions_on_many_threads_spill_at_once() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 6;
    const OIDS: usize = 3;
    fn pickle(oid: Oid) -> Vec<u8> {
        format!("pickle {}", oid.0).into_bytes()
    }

    // About one pickle of each transaction fits in memory.
    let (vfs, store) = spilling_store(12);
    let store = Arc::new(store);
    let (sender, receiver) = std::sync::mpsc::channel::<Txn<Arc<Store>>>();
    let dropper = thread::spawn(move || receiver.into_iter().for_each(drop));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (store, sender) = (Arc::clone(&store), sender.clone());
            thread::spawn(move || {
                let mut committed = Vec::new();
                for round in 0..ROUNDS {
                    let mut txn = Txn::begin(Arc::clone(&store), b"", b"", b"");
                    let oids: Vec<Oid> = (0..OIDS).map(|_| store.new_oid().unwrap()).collect();
                    for &oid in &oids {
                        txn.store(oid, Tid::ZERO, &pickle(oid)).unwrap();
                    }
                    assert!(txn.staged_in_memory() <= 12);
                    if round % 2 == 1 {
                        sender.send(txn).unwrap();
                        continue;
                    }
                    txn.vote().unwrap();
                    let tid = txn.finish().unwrap();
                    committed.extend(oids.into_iter().map(|oid| (oid, tid)));
                }
                committed
            })
        })
        .collect();
    drop(sender);
    let committed: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    dropper.join().unwrap();

    assert_eq!(committed.len(), THREADS * ROUNDS / 2 * OIDS);
    for (oid, tid) in committed {
        assert_eq!(store.load(oid).unwrap(), (pickle(oid), tid));
    }
    assert!(!spills(&vfs));
    assert!(store.lock_table_is_empty());
}

/// Open a store in memory with `options`.
fn open_with(options: Options) -> Store {
    Store::open_full(MemVfs::new(), SystemClock, options).unwrap()
}

#[test]
fn a_refused_store_stages_nothing() {
    let store = open_with(Options {
        max_record_size: Some(3),
        ..Options::default()
    });
    let (kept, refused) = (Oid(1), Oid(2));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(kept, Tid::ZERO, b"abc").unwrap();
    for oid in [kept, refused] {
        let error = txn.store(oid, Tid::ZERO, b"abcd").unwrap_err();
        assert!(
            matches!(
                error,
                ZoeError::TooLarge {
                    limit: SizeLimit::Record,
                    size: 4,
                    max: 3,
                    ..
                }
            ),
            "{error:?}"
        );
    }
    let (serial, data) = txn.staged(kept).unwrap().unwrap();
    assert_eq!((serial, &*data), (Tid::ZERO, &b"abc"[..]));
    assert!(txn.staged(refused).unwrap().is_none());
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(kept).unwrap(), (b"abc".to_vec(), tid));
    assert!(matches!(
        store.load(refused),
        Err(ZoeError::NotFound { .. })
    ));
}

#[test]
fn a_call_out_of_order_is_protocol_whatever_its_size() {
    let store = open_with(Options {
        max_record_size: Some(1),
        ..Options::default()
    });
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
    txn.vote().unwrap();
    let error = txn.store(Oid(1), Tid::ZERO, b"too large");
    assert!(matches!(error, Err(ZoeError::Protocol(_))), "{error:?}");
}

#[test]
fn a_replaced_pickle_counts_once_toward_the_transaction_size() {
    let store = open_with(Options {
        max_transaction_size: Some(6),
        ..Options::default()
    });
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"abcd").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"abcdef").unwrap();
    let error = txn.store(Oid(2), Tid::ZERO, b"g").unwrap_err();
    assert!(
        matches!(
            error,
            ZoeError::TooLarge {
                oid: Oid(2),
                limit: SizeLimit::Transaction,
                size: 7,
                max: 6
            }
        ),
        "{error:?}"
    );
    // A shorter pickle of the same oid makes room.
    txn.store(Oid(1), Tid::ZERO, b"abcde").unwrap();
    txn.store(Oid(2), Tid::ZERO, b"g").unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
}

#[test]
fn a_restore_transaction_checks_no_size_limit() {
    let store = open_with(Options {
        max_record_size: Some(3),
        max_transaction_size: Some(3),
        ..Options::default()
    });
    let mut txn = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
    txn.restore(Oid(1), Some(b"a copy over the limits"))
        .unwrap();
    txn.store(Oid(2), Tid::ZERO, b"a store over the limits")
        .unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), Tid(100));
    assert_eq!(store.load(Oid(1)).unwrap().0, b"a copy over the limits");
    assert_eq!(store.load(Oid(2)).unwrap().0, b"a store over the limits");

    // A normal transaction keeps the limits.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let error = txn.store(Oid(3), Tid::ZERO, b"abcd");
    assert!(matches!(error, Err(ZoeError::TooLarge { .. })), "{error:?}");
}

/// Return the duration, the lowest oid and the tid of each
/// `transaction-warn` event of `events`.
fn slow_transactions(events: &[Captured]) -> Vec<[Option<u64>; 3]> {
    events
        .iter()
        .filter(|e| e.message().contains("transaction-warn"))
        .map(|e| [e.number("duration_ms"), e.number("oid"), e.number("tid")])
        .collect()
}

#[test]
fn each_end_of_a_transaction_is_logged_once() {
    let (store, clock) = open_with_clock(Options {
        transaction_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    });
    let (tid, events) = events_of(|| {
        let mut finished = store.tpc_begin(b"", b"", b"").unwrap();
        finished.store(Oid(9), Tid::ZERO, b"a").unwrap();
        finished.store(Oid(1), Tid::ZERO, b"a").unwrap();
        finished.vote().unwrap();
        clock.advance(Duration::from_millis(1_100));
        let tid = finished.finish().unwrap();

        let mut aborted = store.tpc_begin(b"", b"", b"").unwrap();
        aborted.store(Oid(2), Tid::ZERO, b"b").unwrap();
        aborted.vote().unwrap();
        clock.advance(Duration::from_millis(1_200));
        aborted.abort();

        let dropped = store.tpc_begin(b"", b"", b"").unwrap();
        clock.advance(Duration::from_millis(1_300));
        drop(dropped);

        // A finish that fails ends the transaction too.
        let mut failed = store.tpc_begin(b"", b"", b"").unwrap();
        failed.store(Oid(3), Tid::ZERO, b"c").unwrap();
        clock.advance(Duration::from_millis(1_400));
        assert!(failed.finish().is_err());
        tid
    });
    // Only a commit has a tid, and only a transaction that writes has
    // an oid: the lowest one.
    assert_eq!(
        slow_transactions(&events),
        [
            [Some(1_100), Some(1), Some(tid.0)],
            [Some(1_200), Some(2), None],
            [Some(1_300), None, None],
            [Some(1_400), Some(3), None],
        ],
        "{events:?}"
    );
}

/// A clock that moves on by `step` at each read.
struct StepClock {
    now: std::sync::Mutex<std::time::SystemTime>,
    step: Duration,
}

impl crate::clock::Clock for StepClock {
    fn now(&self) -> std::time::SystemTime {
        let mut now = self.now.lock().unwrap();
        *now += self.step;
        *now
    }
}

#[test]
fn the_locks_that_a_lock_wait_timeout_releases_are_logged() {
    let step = Duration::from_secs(2);
    let clock = StepClock {
        now: std::sync::Mutex::new(std::time::UNIX_EPOCH),
        step,
    };
    // A zero lock timeout turns every wait into a timeout.
    let options = Options {
        lock_timeout: Duration::ZERO,
        lock_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), clock, options).unwrap();
    let (low, high) = (Oid(1), Oid(2));
    let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
    holder.store(high, Tid::ZERO, b"a").unwrap();
    holder.vote().unwrap();

    // The vote takes `low`, then times out on `high`. It reads the
    // clock when it takes `low` and after it releases `low`, so the
    // hold lasts one step.
    let ((), events) = events_of(|| {
        let mut waiter = store.tpc_begin(b"", b"", b"").unwrap();
        waiter.check_current_serial(low, Tid::ZERO).unwrap();
        waiter.store(high, Tid::ZERO, b"b").unwrap();
        let error = waiter.vote();
        assert!(
            matches!(error, Err(ZoeError::Conflict { oid }) if oid == high),
            "{error:?}"
        );
    });
    let logged: Vec<_> = events
        .iter()
        .filter(|e| e.message().contains("lock-warn"))
        .map(|e| (e.number("oid"), e.number("duration_ms")))
        .collect();
    let step = Some(step.as_millis() as u64);
    assert_eq!(logged, [(Some(low.0), step), (None, None)], "{events:?}");
    holder.abort();
    assert!(store.lock_table_is_empty());
}

#[test]
fn a_lock_taken_after_a_wait_is_held_from_its_grant() {
    let (store, clock) = open_with_clock(Options {
        lock_warn: Some(Duration::from_secs(1)),
        ..Options::default()
    });
    let (low, high) = (Oid(1), Oid(2));
    let mut holder = store.tpc_begin(b"", b"", b"").unwrap();
    holder.store(high, Tid::ZERO, b"a").unwrap();
    holder.vote().unwrap();

    let ((), events) = events_of(|| {
        let waiter = thread::scope(|scope| {
            // The waiter takes `low`, then waits for `high`.
            let waiter = scope.spawn(|| {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.check_current_serial(low, Tid::ZERO).unwrap();
                txn.store(high, Tid::ZERO, b"b").unwrap();
                txn.vote().unwrap();
                txn
            });
            while store.locks.waiting(high) == 0 {
                thread::sleep(Duration::from_millis(1));
            }
            clock.advance(Duration::from_millis(3_000));
            holder.abort();
            waiter.join().unwrap()
        });
        clock.advance(Duration::from_millis(500));
        waiter.finish().unwrap();
    });
    let logged: Vec<_> = events
        .iter()
        .filter(|e| e.message().contains("lock-warn") && e.number("oid").is_some())
        .map(|e| (e.number("oid"), e.number("duration_ms")))
        .collect();
    // The waiter held `low` while it waited, and `high` only from the
    // abort of the holder.
    assert_eq!(
        logged,
        [(Some(high.0), Some(3_000)), (Some(low.0), Some(3_500))],
        "{events:?}"
    );
    assert!(store.lock_table_is_empty());
}

/// The size checks read the length of a spilled pickle, never its
/// bytes. So a damaged spill file does not change them.
#[test]
fn the_size_checks_read_no_spilled_pickle() {
    let vfs = MemVfs::new();
    let options = Options {
        staging_memory: 0,
        max_transaction_size: Some(6),
        ..Options::default()
    };
    let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, b"abcd").unwrap();
    assert!(spills(&vfs));
    let name = &Vfs::list_files(&vfs, "staging").unwrap()[0];
    vfs.file(&format!("staging/{name}"))
        .write(0, b"xxxx")
        .unwrap();

    // The spilled pickles of `Oid(1)` count once, with their length.
    txn.store(Oid(1), Tid::ZERO, b"abcdef").unwrap();
    let error = txn.store(Oid(2), Tid::ZERO, b"g");
    assert!(
        matches!(
            error,
            Err(ZoeError::TooLarge {
                size: 7,
                max: 6,
                ..
            })
        ),
        "{error:?}"
    );
    txn.store(Oid(1), Tid::ZERO, b"abcde").unwrap();
    txn.store(Oid(2), Tid::ZERO, b"g").unwrap();
    txn.abort();
    assert!(!spills(&vfs));
}

/// A pickle of a `persistent.mapping.PersistentMapping` record, as ZODB
/// writes it: the class pickle, then the state pickle.
const MAPPING: &[u8] = b"\x80\x03X\x12\x00\x00\x00persistent.mappingq\x00\
    X\x11\x00\x00\x00PersistentMappingq\x01\x86q\x02N\x86q\x03.\x80\x03}q\x00.";

#[test]
fn delete_writes_a_deletion_with_the_serial_check_of_store() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let (kept, gone) = (Oid(1), Oid(2));
    let first = commit_all(&store, &[(kept, Tid::ZERO, b"k"), (gone, Tid::ZERO, b"g")]);

    // A stale serial fails the vote with the serials of a conflict.
    let mut stale = store.tpc_begin(b"", b"", b"").unwrap();
    stale.delete(gone, Tid::ZERO).unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { oid }) if oid == gone));
    assert_eq!(stale.conflict_serials(), Some((first, Tid::ZERO)));
    assert!(store.lock_table_is_empty());

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.delete(gone, first).unwrap();
    assert_eq!(
        txn.staged(gone).unwrap(),
        Some((first, Cow::Borrowed(&b""[..])))
    );
    txn.vote().unwrap();
    let deleted = txn.finish().unwrap();

    // The deletion is a record with an empty payload after `first`.
    assert!(matches!(store.load(gone), Err(ZoeError::NotFound { .. })));
    let record = store.backend.load_before(gone, Tid::MAX).unwrap().unwrap();
    assert_eq!((record.tid, record.prev), (deleted, first));
    assert!(record.data.is_empty());
    let before = store.load_before(gone, deleted).unwrap().unwrap();
    assert_eq!((before.data, before.serial), (b"g".to_vec(), first));
    let sizes: Vec<_> = store
        .history(gone, 10)
        .unwrap()
        .iter()
        .map(|entry| (entry.tid, entry.size))
        .collect();
    assert_eq!(sizes, vec![(deleted, 0), (first, 1)]);
    let current = store.record_iternext(None).unwrap().unwrap();
    assert_eq!((current.oid, current.next), (kept, None));
    assert_eq!(store.load(kept).unwrap(), (b"k".to_vec(), first));

    // A store after the deletion reads the serial of the deletion.
    let again = commit(&store, gone, deleted, b"h");
    assert_eq!(store.load(gone).unwrap(), (b"h".to_vec(), again));
}

#[test]
fn delete_in_a_restore_transaction_checks_the_serial() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let first = commit(&store, Oid(1), Tid::ZERO, b"a");
    let mut stale = store
        .tpc_begin_restore(Tid(first.0 + 1), b"", b"", b"")
        .unwrap();
    stale.delete(Oid(1), Tid::ZERO).unwrap();
    assert!(matches!(stale.vote(), Err(ZoeError::Conflict { .. })));
    drop(stale);

    let tid = Tid(first.0 + 2);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    txn.delete(Oid(1), first).unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
    assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
}

#[test]
fn a_deletion_replaces_a_staged_pickle_and_drops_its_blob() {
    let store = open_with(Options {
        max_transaction_size: Some(4),
        ..Options::default()
    });
    let first = commit(&store, Oid(1), Tid::ZERO, b"a");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"blob").unwrap();
    txn.store_blob(Oid(1), first, b"abcd", file).unwrap();
    txn.delete(Oid(1), first).unwrap();
    // The deleted pickle no longer counts toward the transaction size.
    txn.store(Oid(2), Tid::ZERO, b"efgh").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert!(matches!(store.load(Oid(1)), Err(ZoeError::NotFound { .. })));
    assert!(store.open_blob(Oid(1), tid).is_err());
    assert_eq!(store.load(Oid(2)).unwrap(), (b"efgh".to_vec(), tid));
}

#[test]
fn a_deletion_is_never_resolved() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let first = commit(&store, Oid(1), Tid::ZERO, b"a");
    let second = commit(&store, Oid(1), first, b"b");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.delete(Oid(1), first).unwrap();
    let error = txn.vote_resolving();
    assert!(
        matches!(error, Err(ZoeError::Conflict { oid: Oid(1) })),
        "{error:?}"
    );
    assert_eq!(txn.conflict_serials(), Some((second, first)));
    assert!(store.lock_table_is_empty());
}

#[test]
fn delete_is_refused_out_of_order_from_another_thread_and_on_the_reserved_oid() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let first = commit(&store, Oid(1), Tid::ZERO, b"a");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let reserved = txn.delete(Oid(u64::MAX), Tid::ZERO);
    assert!(
        matches!(reserved, Err(ZoeError::Protocol(_))),
        "{reserved:?}"
    );
    thread::scope(|scope| {
        scope.spawn(|| {
            let refused = txn.delete(Oid(1), first);
            assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
        });
    });
    // The refused calls staged nothing.
    assert_eq!(txn.staged(Oid(1)).unwrap(), None);
    txn.store(Oid(2), Tid::ZERO, b"b").unwrap();
    txn.vote().unwrap();
    let after = txn.delete(Oid(1), first);
    assert!(matches!(after, Err(ZoeError::Protocol(_))), "{after:?}");
    txn.finish().unwrap();
    assert_eq!(store.load(Oid(1)).unwrap(), (b"a".to_vec(), first));
}

#[test]
fn a_merge_that_replaces_a_merge_counts_the_length_of_the_first_merge() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let oid = Oid(1);
    txn.store(oid, Tid::ZERO, b"0123456789").unwrap();
    // Another record of 100 bytes counts in the transaction too.
    txn.store(Oid(2), Tid::ZERO, &[b'x'; 100]).unwrap();
    let staged = txn.stored_bytes;
    assert_eq!(staged, 110);
    // A first merge of 4 bytes replaces the staged pickle of 10 bytes.
    let first = txn.merged_size(oid, b"abcd", staged).unwrap();
    assert_eq!(first, 104);
    // A second merge of 6 bytes replaces the first one, not the
    // staged pickle: 100 + 6.
    let second = txn
        .merged_size_replacing(oid, b"abcdef", first, Some(4))
        .unwrap();
    assert_eq!(second, 106);
}

#[test]
fn staged_class_name_reads_the_class_pickle() {
    let store = Store::open_vfs(MemVfs::new()).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, MAPPING).unwrap();
    txn.store(Oid(2), Tid::ZERO, b"not a pickle").unwrap();
    txn.delete(Oid(3), Tid::ZERO).unwrap();
    assert_eq!(
        txn.staged_class_name(Oid(1)).as_deref(),
        Some("persistent.mapping.PersistentMapping")
    );
    for oid in [Oid(2), Oid(3), Oid(4)] {
        assert_eq!(txn.staged_class_name(oid), None, "{oid:?}");
    }
}
