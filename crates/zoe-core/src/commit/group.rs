//! Group commit.
//!
//! Each finish joins a commit batch. A finish that finds no leader becomes
//! the leader: it takes every finish that waits, writes them all in one
//! backend write transaction under the commit sequencer, and commits it
//! with one sync. Finishes that arrive while the leader writes wait, and
//! form the next batch. When the batch is durable or failed, the leader
//! gives each member its outcome and wakes the waiting finishes. One of
//! them then leads the next batch.
//!
//! The members of a batch commit in arrival order. Each member gets a tid
//! after the tid of the member before it, so the commit order is the batch
//! order. The leader publishes the last tid of the batch after the whole
//! batch is visible to readers.
//!
//! A batch whose members are all deferred restores commits without a
//! sync. It is visible at once, and the next durable commit makes it
//! durable. Each deferred member keeps its intent file in `blobs/tmp/`
//! until then, so a crash before it leaves no blob file of a lost commit.
//! A batch with another member commits with a sync, which also makes every
//! earlier deferred commit durable.
//!
//! A member can merge onto the record of a commit in flight that came to
//! it with a lock hand-off (ADR 0044). That commit is in the same batch
//! before the member, or in an earlier batch, so it has its tid when the
//! leader writes the member. The member writes that tid as the previous
//! serial of the record. A member whose commit in flight got no tid fails
//! with `Protocol`.
//!
//! The queue mutex is held with no other lock. The leader takes the commit
//! sequencer after it releases the queue mutex. A finish that waits for the
//! end of a batch waits on a waiter of the store clock, so a simulator runs
//! other threads meanwhile. The leader wakes the waiters after it releases
//! the queue mutex.
//!
//! The leader writes the namespace writes of each member after its records,
//! in the same backend write. An ephemeral key expires at the time of that
//! write plus its TTL. Each durable namespace that a member writes gets the
//! tid of the member as its generation.
//!
//! The write of a batch and its sync run in a `tracing` span `fsync`, with
//! the field `commits`, the number of members that have a tid. The span
//! is a child of the span of the leader's finish. A batch of deferred
//! restores commits without a sync, so it has no span `fsync`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use super::staging::Staging;
use super::{Finished, Observer, check_restore_tid, next_tid};
use crate::backend::{TxnMeta, TxnStatus, WriteTxn};
use crate::blob::PendingBlob;
use crate::clock::{Waiter, timestamp};
use crate::error::{Result, ZoeError};
use crate::format;
use crate::kv::{self, Flavour};
use crate::lock::InFlight;
use crate::store::Store;
use crate::types::{Oid, Tid};
use crate::undo;

/// The part of a transaction that the leader of its batch writes.
pub(super) struct Member {
    /// The tid of a restore transaction. A normal transaction gets its tid
    /// in the batch.
    pub(super) tid: Option<Tid>,
    /// True for a restore transaction whose finish commits without a sync.
    pub(super) deferred: bool,
    pub(super) staged: Staging,
    pub(super) blobs: BTreeMap<Oid, PendingBlob>,
    pub(super) user: Vec<u8>,
    pub(super) description: Vec<u8>,
    pub(super) extension: Vec<u8>,
    /// The commit in flight of this member, when it handed off a lock. The
    /// leader gives it its tid.
    pub(super) commit: Option<Arc<InFlight>>,
    /// The oids whose records this member merged onto the record of a
    /// commit in flight, with that commit.
    pub(super) onto: BTreeMap<Oid, Arc<InFlight>>,
    /// The lowest tid that the transaction undoes, when it undoes one.
    pub(super) undone: Option<Tid>,
}

/// The outcome of one finish. The member goes back to its transaction. It
/// is `None` only after a panic of the leader.
pub(super) type Outcome = (Option<Member>, Result<Finished>);

/// The finishes of a store that wait for a batch.
#[derive(Default)]
struct Queue {
    /// The finishes that wait for a leader, with their tickets, in arrival
    /// order.
    waiting: Vec<(u64, Member)>,
    /// The outcome of each finish of an ended batch, by ticket.
    ended: HashMap<u64, Outcome>,
    /// True while a leader writes a batch.
    leading: bool,
    /// The ticket of the next finish.
    next: u64,
    /// The waiters of the finishes that wait for the end of a batch.
    sleepers: Vec<Arc<dyn Waiter>>,
}

/// The commit batches of a store.
#[derive(Default)]
pub(crate) struct GroupCommit {
    queue: Mutex<Queue>,
    /// The observer of the store. A leader tells it the size of each
    /// batch that it made durable.
    pub(super) observer: OnceLock<Arc<dyn Observer>>,
}

impl GroupCommit {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // The queue stays valid after a panic: `Lead` ends the lead when
        // it unwinds.
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Commit `member` in a batch of `store`. Return the member and its
    /// outcome when its batch is durable or failed.
    ///
    /// `queued` runs once the member is in the queue, without the queue
    /// mutex, before the finish waits. A finish that starts after it is
    /// later in the commit order.
    pub(super) fn finish<F: FnOnce()>(
        &self,
        store: &Store,
        member: Member,
        queued: Option<F>,
    ) -> Outcome {
        // The waiter comes from the store clock before the member joins the
        // queue: a clock that panics then leaves no member in the queue
        // whose transaction released its locks.
        let waiter = store.clock.waiter();
        let mut queue = self.lock();
        let ticket = queue.next;
        queue.next += 1;
        queue.waiting.push((ticket, member));
        if let Some(queued) = queued {
            drop(queue);
            queued();
            queue = self.lock();
        }
        loop {
            if let Some(outcome) = queue.ended.remove(&ticket) {
                return outcome;
            }
            if !queue.leading {
                // The ticket is still in `waiting`: only a leader takes it
                // out, and a leader gives its outcome before it stops.
                queue.leading = true;
                let batch = mem::take(&mut queue.waiting);
                drop(queue);
                let mut lead = Lead {
                    group: self,
                    tickets: batch.iter().map(|(ticket, _)| *ticket).collect(),
                    outcomes: None,
                };
                lead.outcomes = Some(write_batch(store, batch));
                drop(lead);
                queue = self.lock();
                continue;
            }
            queue.sleepers.push(Arc::clone(&waiter));
            drop(queue);
            // A wake before the wait is not lost.
            waiter.wait(Duration::MAX);
            queue = self.lock();
        }
    }

    /// Return the number of finishes that joined a batch since the store
    /// opened.
    #[cfg(test)]
    pub(super) fn joined(&self) -> u64 {
        self.lock().next
    }
}

/// The batch of a leader. When it drops, it gives each member its outcome,
/// ends the lead and wakes the waiting finishes. After a panic of the
/// leader, the outcome of each member is unknown.
struct Lead<'a> {
    group: &'a GroupCommit,
    tickets: Vec<u64>,
    outcomes: Option<Vec<Outcome>>,
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        let outcomes = self.outcomes.take();
        let mut queue = self.group.lock();
        match outcomes {
            Some(outcomes) => queue
                .ended
                .extend(self.tickets.iter().copied().zip(outcomes)),
            None => queue.ended.extend(self.tickets.iter().map(|&ticket| {
                let error = "the leader of the commit batch panicked";
                (ticket, (None, Err(ZoeError::Backend(error.into()))))
            })),
        }
        queue.leading = false;
        let sleepers = mem::take(&mut queue.sleepers);
        drop(queue);
        for sleeper in sleepers {
            sleeper.wake();
        }
    }
}

/// Write the members of a batch under the commit sequencer, and return
/// the outcome of each, in batch order.
///
/// Each member gets its tid in batch order. A tid that fails its check
/// fails only its member, with `Protocol`. An undo of a transaction at or
/// before the pack watermark fails only its member, with `Undo`: a pack
/// that came after the undo read its records can have removed what the
/// undo gives back. The other members go in one
/// backend write transaction. An error of that write fails each of them,
/// and the store halts. The write commits without a sync when each of
/// these members is a deferred restore.
fn write_batch(store: &Store, batch: Vec<(u64, Member)>) -> Vec<Outcome> {
    let mut sequencer = store.lock_sequencer();
    let Some(start) = *sequencer else {
        return batch
            .into_iter()
            .map(|(_, member)| (Some(member), Err(ZoeError::Halted)))
            .collect();
    };
    let time = store.clock.now();
    let now = timestamp(time);
    // The expiries of the ephemeral writes of the batch count from here.
    let now_ms = kv::millis(time);
    let mut last = start;
    // The number of commits that the sync of the batch made durable.
    let mut synced = 0;
    let mut members: Vec<(Member, Result<Finished>)> = batch
        .into_iter()
        .map(|(_, member)| {
            let tid = check_undone(store, member.undone).and_then(|()| match member.tid {
                Some(tid) => check_restore_tid(tid, last).map(|()| tid),
                None => next_tid(now, last),
            });
            // A member commits after each commit that it merged onto.
            let tid = tid.and_then(|tid| {
                if member.onto.values().all(|commit| commit.tid().is_some()) {
                    Ok(tid)
                } else {
                    Err(ZoeError::Protocol(
                        "a commit that this commit merged onto failed",
                    ))
                }
            });
            if let Some(commit) = &member.commit {
                match &tid {
                    Ok(tid) => commit.assign(*tid),
                    Err(_) => commit.end(false),
                }
            }
            let finished = tid.map(|tid| Finished {
                tid,
                previous: mem::replace(&mut last, tid),
            });
            (member, finished)
        })
        .collect();
    if last != start {
        let deferred = members
            .iter()
            .all(|(member, finished)| member.deferred || finished.is_err());
        // The sequencer stays empty if the write below fails.
        *sequencer = None;
        // Only a batch that syncs gets the span `fsync`.
        let span = if deferred {
            tracing::Span::none()
        } else {
            let commits = members.iter().filter(|(_, f)| f.is_ok()).count();
            tracing::info_span!("fsync", commits)
        };
        match span.in_scope(|| write(store, &mut members, deferred, now_ms)) {
            Ok(intents) => {
                if deferred {
                    // A crash before the next durable commit loses this
                    // batch. The intent files let the next open remove
                    // its blob files.
                    for intent in intents {
                        store.blobs().keep(intent);
                    }
                } else {
                    for intent in intents {
                        store.blobs().finished(intent);
                    }
                    // The sync of this batch covers the deferred commits
                    // before it.
                    store.blobs().release_kept();
                }
                for (member, finished) in &members {
                    if let Ok(finished) = finished {
                        // A running pack marks what these records refer to
                        // before its next removal.
                        let oids = member.staged.iter().map(|(oid, _)| oid);
                        store.packing.note(finished.tid, oids);
                    }
                }
                *sequencer = Some(last);
                if !deferred {
                    synced = members.iter().filter(|(_, f)| f.is_ok()).count();
                }
            }
            Err(error) => fail(&mut members, error),
        }
    }
    drop(sequencer);
    if synced > 0
        && let Some(observer) = store.observer()
    {
        observer.batch_synced(synced);
    }
    members
        .into_iter()
        .map(|(member, finished)| (Some(member), finished))
        .collect()
}

/// Check that `undone`, the lowest tid that a member undoes, is after the
/// pack watermark. The caller holds the commit sequencer, so no pack batch
/// commits between the check and the write of the member.
pub(super) fn check_undone(store: &Store, undone: Option<Tid>) -> Result<()> {
    match undone {
        Some(undone) if undone <= store.pack_watermark()? => Err(ZoeError::Undo {
            oid: None,
            reason: undo::PACKED,
        }),
        _ => Ok(()),
    }
}

/// Place the blobs of the members that have a tid, then write these
/// members in one backend write transaction and commit it, without a sync
/// when `deferred` is true. `now_ms` is the time of the write, in
/// milliseconds since the Unix epoch. Return the intent files of the blobs.
fn write(
    store: &Store,
    members: &mut [(Member, Result<Finished>)],
    deferred: bool,
    now_ms: u64,
) -> Result<Vec<Option<String>>> {
    let mut intents = Vec::new();
    for (member, finished) in members.iter_mut() {
        let Ok(finished) = finished else { continue };
        if let Some(oid) = member.staged.last_oid() {
            store.raise_next_oid(oid);
        }
        // The blob files are durable in their place before the backend
        // commit, so a blob record never lacks its file.
        intents.push(store.blobs().place(finished.tid, &mut member.blobs)?);
    }
    let mut write = store.backend.begin_write()?;
    for (member, finished) in members.iter_mut() {
        let Ok(finished) = finished else { continue };
        let tid = finished.tid;
        // An attached payload goes into its frame as it is. A spilled
        // pickle without one is read back here, one at a time. A record
        // merged onto a commit in flight comes after the record of that
        // commit.
        let onto = &member.onto;
        let mut payloads = member.staged.payloads().map(|record| {
            record.map(|(oid, serial, payload)| {
                let serial = onto
                    .get(&oid)
                    .and_then(|commit| commit.tid())
                    .unwrap_or(serial);
                (oid, serial, payload)
            })
        });
        write.put_payloads(tid, &mut payloads)?;
        for (oid, _) in member.staged.iter() {
            if let Some(blob) = member.blobs.get(&oid) {
                write.put_blob(oid, tid, blob.record())?;
            }
        }
        write.put_txn(&TxnMeta {
            tid,
            user: mem::take(&mut member.user),
            description: mem::take(&mut member.description),
            extension: mem::take(&mut member.extension),
            oid_count: member.staged.len() as u64,
            status: TxnStatus::Committed,
        })?;
        write_kv(&mut *write, &member.staged, tid, now_ms)?;
    }
    if deferred {
        write.commit_deferred()?;
    } else {
        write.commit()?;
    }
    Ok(intents)
}

/// Write the namespace writes of `staged`, the staging of the member with
/// the tid `tid`, at the time `now_ms`.
///
/// An ephemeral set writes its row with the expiry `now_ms` plus its TTL,
/// and the key of that expiry in the index of the expiries. A delete
/// removes the row: the index key of the row stays, and the sweep removes
/// it. Each durable namespace that the member writes gets `tid` in its
/// generation row.
fn write_kv(write: &mut dyn WriteTxn, staged: &Staging, tid: Tid, now_ms: u64) -> Result<()> {
    let mut durable = BTreeSet::new();
    for entry in staged.kv_writes() {
        let (flavour, row, value, ttl_ms) = entry?;
        match (flavour, value) {
            (_, None) => write.remove_kv(flavour, row)?,
            (Flavour::Ephemeral, Some(value)) => {
                let expiry = now_ms.saturating_add(ttl_ms);
                write.put_kv(flavour, row, &format::encode_kv_ephemeral(expiry, &value))?;
                write.put_kv_expiry(&format::kv_expiry_key(expiry, row))?;
            }
            (Flavour::Durable, Some(value)) => write.put_kv(flavour, row, &value)?,
        }
        if flavour == Flavour::Durable
            && let Some((name, _)) = format::split_kv_key(row)
        {
            durable.insert(name);
        }
    }
    for name in durable {
        let generation = format::encode_kv_generation(tid);
        write.put_kv(Flavour::Durable, &format::kv_key(name, b""), &generation)?;
    }
    Ok(())
}

/// Fail each member that has a tid with `error`. The last such member gets
/// `error` itself, and the others a copy.
fn fail(members: &mut [(Member, Result<Finished>)], error: ZoeError) {
    let mut written = members
        .iter_mut()
        .map(|(_, finished)| finished)
        .filter(|finished| finished.is_ok())
        .peekable();
    let mut error = Some(error);
    while let Some(finished) = written.next() {
        let own = match (written.peek(), error.as_ref()) {
            (Some(_), Some(shared)) => copy(shared),
            _ => error.take().expect("the last member takes the error"),
        };
        *finished = Err(own);
    }
}

/// Return a copy of `error` for another member of a failed batch. The copy
/// has the message of `error`. An I/O error keeps its kind.
fn copy(error: &ZoeError) -> ZoeError {
    match error {
        ZoeError::Io(error) => ZoeError::Io(io::Error::new(error.kind(), error.to_string())),
        ZoeError::Backend(inner) => ZoeError::Backend(inner.to_string().into()),
        other => ZoeError::Backend(other.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    use zoe_pickle::btrees::new_bucket_state;
    use zoe_pickle::python::{Heap, Value};

    use super::*;
    use crate::commit::Txn;
    use crate::lock::LockCancel;
    use crate::merge::{Resolution, Resolved};
    use crate::testing::{CountingVfs, record, wait_until};
    use crate::vfs::MemVfs;

    /// Begin a transaction that writes `data` for the new oid `oid`, and
    /// vote.
    fn voted<'s>(store: &'s Store, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, data).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// Finish `txns`, each on a thread of its own, while the test holds the
    /// commit sequencer. The first finish leads a batch of its own and
    /// waits for the sequencer. The others join in the order of `txns`
    /// and wait for the next batch. Call `first` when the first finish
    /// returns. Return the outcomes in the order of `txns`.
    fn finish_in_two_batches<'s>(
        store: &'s Store,
        txns: Vec<Txn<&'s Store>>,
        first: impl FnOnce(),
    ) -> Vec<Result<Finished>> {
        let sequencer = store.lock_sequencer();
        let joined = store.group.joined();
        thread::scope(|scope| {
            let mut finishes = Vec::new();
            for (n, txn) in (1..).zip(txns) {
                finishes.push(scope.spawn(move || txn.finish_after()));
                wait_until(|| store.group.joined() == joined + n);
            }
            drop(sequencer);
            let mut finishes = finishes.into_iter();
            let mut outcomes = vec![finishes.next().unwrap().join().unwrap()];
            first();
            outcomes.extend(finishes.map(|finish| finish.join().unwrap()));
            outcomes
        })
    }

    #[test]
    fn concurrent_finishes_share_one_sync() {
        const FINISHES: u64 = 16;
        let vfs = CountingVfs::default();
        let store = Store::open_in(vfs.clone(), crate::clock::SystemClock).unwrap();
        let txns = (0..FINISHES)
            .map(|_| voted(&store, store.new_oid().unwrap(), b"pickle"))
            .collect();
        let before = vfs.syncs();
        let finished = finish_in_two_batches(&store, txns, || {});
        let syncs = vfs.syncs() - before;

        // One sync for the first batch, one for the others. A file that
        // grows can add one more.
        assert!(
            syncs <= 3,
            "{FINISHES} concurrent finishes took {syncs} syncs of the database",
        );
        // Each finish committed after the one before it.
        let mut previous = finished[0].as_ref().unwrap().previous;
        for finish in &finished {
            let finish = finish.as_ref().unwrap();
            assert_eq!(finish.previous, previous);
            assert!(finish.tid > previous);
            previous = finish.tid;
        }
        assert_eq!(store.last_transaction(), previous);
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_restore_tid_that_fails_its_check_fails_only_its_member() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        // The restore tid is far after the clock, so it is after the first
        // commit also on a slow build.
        let tid = timestamp(std::time::SystemTime::now() + Duration::from_secs(3600));
        let first = store.new_oid().unwrap();
        let txns = vec![
            voted(&store, first, b"first"),
            restore(&store, tid, Oid(100), b"a"),
            restore(&store, tid, Oid(101), b"b"),
            voted(&store, store.new_oid().unwrap(), b"after"),
        ];
        let finished = finish_in_two_batches(&store, txns, || {});

        assert_eq!(finished[1].as_ref().unwrap().tid, tid);
        assert!(matches!(finished[2], Err(ZoeError::Protocol(_))));
        let after = finished[3].as_ref().unwrap();
        assert_eq!(after.previous, tid);
        assert!(after.tid > tid);
        assert!(matches!(
            store.load(Oid(101)),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(store.locks.is_empty());
        // The store does not halt.
        voted(&store, store.new_oid().unwrap(), b"later")
            .finish()
            .unwrap();
    }

    /// Begin a restore at `tid` that restores `data` for `oid`, and vote.
    fn restore<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        txn.restore(oid, Some(data)).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// Begin a deferred restore at `tid` that restores `data` for `oid`,
    /// and vote.
    fn deferred<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store
            .tpc_begin_restore_deferred(tid, b"", b"", b"")
            .unwrap();
        txn.restore(oid, Some(data)).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// An observer that keeps the size of each synced batch.
    #[derive(Default)]
    struct Batches(std::sync::Mutex<Vec<usize>>);

    impl Observer for Batches {
        fn batch_synced(&self, commits: usize) {
            self.0.lock().unwrap().push(commits);
        }
    }

    #[test]
    fn a_batch_of_deferred_restores_does_not_sync() {
        let vfs = CountingVfs::default();
        let store = Store::open_in(vfs.clone(), crate::clock::SystemClock).unwrap();
        let batches = Arc::new(Batches::default());
        assert!(store.set_observer(batches.clone()));
        let before = vfs.syncs();
        for n in 1..=10 {
            deferred(&store, Tid(n), Oid(n), b"pickle")
                .finish()
                .unwrap();
        }
        assert_eq!(vfs.syncs(), before);
        assert_eq!(store.last_transaction(), Tid(10));
        assert_eq!(store.load(Oid(10)).unwrap(), (b"pickle".to_vec(), Tid(10)));
        // A batch without a sync and `Store::sync` call no observer method.
        store.sync().unwrap();
        assert!(vfs.syncs() > before);
        assert!(batches.0.lock().unwrap().is_empty());
        assert_eq!(store.last_transaction(), Tid(10));
    }

    #[test]
    fn a_batch_with_a_durable_member_makes_its_deferred_members_durable() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        // The restore tid is far after the clock, so the normal commit
        // after it gets a later tid.
        let tid = timestamp(std::time::SystemTime::now() + Duration::from_secs(3600));
        // The first restore commits in a batch of its own, without a sync.
        // The second one shares its batch with a normal commit.
        let txns = vec![
            deferred(&store, Tid(5), Oid(100), b"alone"),
            deferred(&store, tid, Oid(101), b"shared"),
            voted(&store, store.new_oid().unwrap(), b"durable"),
        ];
        let finished = finish_in_two_batches(&store, txns, || {});
        let last = finished[2].as_ref().unwrap().tid;
        assert!(last > tid);

        let image = vfs.crash_now(|_| 0);
        let reopened = Store::open_vfs(image).unwrap();
        assert_eq!(reopened.last_transaction(), last);
        assert_eq!(reopened.load(Oid(100)).unwrap().1, Tid(5));
        assert_eq!(reopened.load(Oid(101)).unwrap().1, tid);
        drop(store);
    }

    #[test]
    fn a_copy_of_an_error_keeps_its_message() {
        let backend = ZoeError::Backend(Box::new(io::Error::other("disk full")));
        assert_eq!(copy(&backend).to_string(), "backend error: disk full");
        let full = ZoeError::Io(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
        let copied = copy(&full);
        assert!(matches!(&copied, ZoeError::Io(e) if e.kind() == io::ErrorKind::StorageFull));
        assert_eq!(copied.to_string(), full.to_string());
        let corrupt = ZoeError::CorruptTxn { tid: Tid(7) };
        assert_eq!(
            copy(&corrupt).to_string(),
            "backend error: corrupt transaction row: tid Tid(7)"
        );
    }

    #[test]
    fn every_crash_image_keeps_each_acknowledged_batch() {
        // The record of the filesystem starts after the store is created.
        let created = MemVfs::new();
        drop(Store::open_vfs(created.clone()).unwrap());
        let vfs = created.durable_copy();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oids: Vec<Oid> = (0..4).map(|_| store.new_oid().unwrap()).collect();
        let txns = oids
            .iter()
            .map(|&oid| voted(&store, oid, b"pickle"))
            .collect();
        // The first batch is acknowledged before the second one ends.
        let mark = || vfs.mark("first acknowledged");
        for finish in finish_in_two_batches(&store, txns, mark) {
            finish.unwrap();
        }
        vfs.mark("acknowledged");

        // A crash does not close the store, so the images end here.
        let images = vfs.crash_images();
        drop(store);
        assert!(images.iter().any(|image| image.is_after("acknowledged")));
        let all: BTreeSet<Oid> = oids.iter().copied().collect();
        let first: BTreeSet<Oid> = oids[..1].iter().copied().collect();
        for image in &images {
            let store = Store::open_vfs(image.vfs())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            let found: BTreeSet<Oid> = oids
                .iter()
                .copied()
                .filter(|&oid| store.load(oid).is_ok())
                .collect();
            // A batch is whole or absent, and no acknowledged commit is
            // lost.
            if image.is_after("acknowledged") {
                assert_eq!(found, all, "{image:?}");
            } else if image.is_after("first acknowledged") {
                assert!(found == first || found == all, "{image:?}: {found:?}");
            } else {
                assert!(
                    found.is_empty() || found == first || found == all,
                    "{image:?}: {found:?}"
                );
            }
        }
    }

    /// Return the record of a `BTrees.Length.Length` of `value`.
    fn length(value: i128) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.Length", "Length");
        record(&heap, class, Value::Int(value))
    }

    /// Return the record of an `IIBucket` of `items`.
    fn ii_bucket(items: &[(i128, i128)]) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.IIBTree", "IIBucket");
        let flat = items
            .iter()
            .flat_map(|&(k, v)| [Value::Int(k), Value::Int(v)])
            .collect();
        let state = new_bucket_state(&mut heap, flat, None);
        record(&heap, class, state)
    }

    /// Return the record of an `IOBucket` that maps `key` to a dict of
    /// `entries`. Tier 1 cannot compare two dicts without the posting-dict
    /// rule, so it gives no answer on a change of the dict.
    fn io_dict(key: i128, entries: &[(i128, i128)]) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.IOBTree", "IOBucket");
        let dict = entries
            .iter()
            .map(|&(k, v)| (Value::Int(k), Value::Int(v)))
            .collect();
        let flat = vec![Value::Int(key), heap.new_dict(dict)];
        let state = new_bucket_state(&mut heap, flat, None);
        record(&heap, class, state)
    }

    /// Commit `data` as a new object. Return its oid and its serial.
    fn created(store: &Store, data: &[u8]) -> (Oid, Tid) {
        let oid = store.new_oid().unwrap();
        let tid = voted(store, oid, data).finish().unwrap();
        (oid, tid)
    }

    /// Begin a transaction that writes `data` for `oid`, read at `serial`.
    fn writing<'s>(store: &'s Store, oid: Oid, serial: Tid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, serial, data).unwrap();
        txn
    }

    /// Return the serial of the record before the current record of `oid`,
    /// as the frame of the record gives it.
    fn previous_serial(store: &Store, oid: Oid) -> Tid {
        store
            .backend
            .load_before(oid, Tid::MAX)
            .unwrap()
            .unwrap()
            .prev
    }

    #[test]
    fn a_vote_merges_onto_a_commit_whose_batch_waits() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let batches = Arc::new(Batches::default());
        store.set_observer(batches.clone());
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        let (first, second) = thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.vote().map(|()| txn)
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            // The batch of the first finish waits for the commit sequencer,
            // so the store does not hold the first commit.
            let sequencer = store.lock_sequencer();
            let joined = store.group.joined();
            let first = scope.spawn(move || first.finish_after());
            let second = second.join().unwrap().unwrap();
            assert_eq!(second.natively_resolved(), [oid]);
            assert!(!first.is_finished());
            let second = scope.spawn(move || second.finish_after());
            wait_until(|| store.group.joined() == joined + 2);
            drop(sequencer);
            (
                first.join().unwrap().unwrap(),
                second.join().unwrap().unwrap(),
            )
        });
        assert_eq!(second.previous, first.tid);
        assert_eq!(store.load(oid).unwrap(), (length(2), second.tid));
        assert_eq!(previous_serial(&store, oid), first.tid);
        assert_eq!(*batches.0.lock().unwrap(), [1, 1]);
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_chain_of_merges_on_a_hot_length_shares_one_sync() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let batches = Arc::new(Batches::default());
        store.set_observer(batches.clone());
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        let finished = thread::scope(|scope| {
            let voters: Vec<_> = (1..=2)
                .map(|n| {
                    let store = &store;
                    let voter = scope.spawn(move || {
                        let mut txn = writing(store, oid, read, &length(1));
                        txn.vote().unwrap();
                        txn
                    });
                    wait_until(|| store.locks.waiting(oid) == n);
                    voter
                })
                .collect();
            let sequencer = store.lock_sequencer();
            let joined = store.group.joined();
            let mut finishes = vec![scope.spawn(move || first.finish_after())];
            // Each finish hands the lock to the next vote.
            for (n, voter) in (2..).zip(voters) {
                let txn = voter.join().unwrap();
                finishes.push(scope.spawn(move || txn.finish_after()));
                wait_until(|| store.group.joined() == joined + n);
            }
            drop(sequencer);
            finishes
                .into_iter()
                .map(|finish| finish.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(store.load(oid).unwrap(), (length(3), finished[2].tid));
        assert_eq!(previous_serial(&store, oid), finished[1].tid);
        assert_eq!(
            store
                .load_before(oid, finished[2].tid)
                .unwrap()
                .unwrap()
                .data,
            length(2)
        );
        // The first commit has a batch of its own. The two merges onto it
        // share the next sync.
        assert_eq!(*batches.0.lock().unwrap(), [1, 2]);
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_refusal_onto_a_commit_in_flight_fails_at_once() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &ii_bucket(&[(1, 1)]));
        let mut first = writing(&store, oid, read, &ii_bucket(&[(1, 2)]));
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &ii_bucket(&[(1, 3)]));
                let voted = txn.vote();
                (voted, txn.conflict_serials(), txn.resolutions().to_vec())
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let sequencer = store.lock_sequencer();
            let first = scope.spawn(move || first.finish());
            let (voted, serials, resolutions) = second.join().unwrap();
            assert!(matches!(voted, Err(ZoeError::Conflict { oid: o }) if o == oid));
            // The commit in flight has no tid yet.
            assert_eq!(serials, None);
            assert!(matches!(
                resolutions[..],
                [Resolution {
                    outcome: Resolved::Refused(_),
                    applied: true,
                    ..
                }]
            ));
            assert!(!first.is_finished());
            drop(sequencer);
            first.join().unwrap().unwrap();
        });
        assert_eq!(store.load(oid).unwrap().0, ii_bucket(&[(1, 2)]));
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_conflict_that_tier_1_does_not_decide_waits_for_the_durable_commit() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &io_dict(1, &[(1, 1)]));
        let mut first = writing(&store, oid, read, &io_dict(1, &[(1, 1), (2, 1)]));
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &io_dict(1, &[(1, 1), (3, 1)]));
                let conflicts = txn.vote_resolving().unwrap();
                (txn, conflicts)
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let sequencer = store.lock_sequencer();
            let first = scope.spawn(move || first.finish());
            // The vote has the lock, and waits for the first commit.
            wait_until(|| store.locks.waiting(oid) == 0);
            thread::sleep(Duration::from_millis(50));
            assert!(!second.is_finished());
            drop(sequencer);
            let tid = first.join().unwrap().unwrap();
            let (txn, conflicts) = second.join().unwrap();
            let [(conflict, record)] = &conflicts[..] else {
                panic!("{conflicts:?}");
            };
            // The caller gets the committed record, as without the
            // hand-off.
            assert_eq!(*conflict, oid);
            assert_eq!(record.tid, tid);
            assert_eq!(&record.data[..], &io_dict(1, &[(1, 1), (2, 1)])[..]);
            txn.abort();
        });
        assert!(store.locks.is_empty());
    }

    /// An observer that keeps the lock wait of each vote.
    #[derive(Default)]
    struct Waits(std::sync::Mutex<Vec<Duration>>);

    impl Observer for Waits {
        fn locks_acquired(&self, wait: Duration) {
            self.0.lock().unwrap().push(wait);
        }
    }

    #[test]
    fn the_lock_wait_of_a_vote_counts_its_wait_for_a_durable_commit() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &io_dict(1, &[(1, 1)]));
        let waits = Arc::new(Waits::default());
        store.set_observer(waits.clone());
        let mut first = writing(&store, oid, read, &io_dict(1, &[(1, 1), (2, 1)]));
        first.vote().unwrap();
        let held = Duration::from_millis(100);
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &io_dict(1, &[(1, 1), (3, 1)]));
                txn.vote_resolving().unwrap();
                txn.abort();
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let sequencer = store.lock_sequencer();
            let first = scope.spawn(move || first.finish());
            // The vote has the lock, and waits for the sync of the first
            // commit, which the sequencer holds.
            wait_until(|| store.locks.waiting(oid) == 0);
            thread::sleep(held);
            drop(sequencer);
            first.join().unwrap().unwrap();
            second.join().unwrap();
        });
        let waits = waits.0.lock().unwrap();
        // The first vote waited for nothing. The second one waited at least
        // as long as the sync was held.
        assert_eq!(waits.len(), 2, "{waits:?}");
        assert!(waits[1] >= held, "{waits:?}");
    }

    #[test]
    fn a_vote_that_fails_after_a_hand_off_passes_it_to_the_next_vote() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let (other, other_read) = created(&store, &length(0));
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        // A transaction holds the other oid, so the second vote waits for
        // it after the hand-off.
        let mut blocker = writing(&store, other, other_read, &length(5));
        blocker.vote().unwrap();
        let cancel = LockCancel::new();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.store(other, other_read, &length(1)).unwrap();
                txn.set_lock_cancel(cancel.clone());
                txn.vote()
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let third = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.vote().map(|()| txn)
            });
            wait_until(|| store.locks.waiting(oid) == 2);
            let sequencer = store.lock_sequencer();
            let joined = store.group.joined();
            let first = scope.spawn(move || first.finish_after());
            wait_until(|| store.locks.waiting(other) == 1);
            cancel.cancel();
            assert!(second.join().unwrap().is_err());
            // The third vote gets the lock with the same hand-off, and
            // merges onto the first commit.
            let third = third.join().unwrap().unwrap();
            assert_eq!(third.natively_resolved(), [oid]);
            let third = scope.spawn(move || third.finish_after());
            wait_until(|| store.group.joined() == joined + 2);
            drop(sequencer);
            let first = first.join().unwrap().unwrap();
            let third = third.join().unwrap().unwrap();
            assert_eq!(previous_serial(&store, oid), first.tid);
            assert_eq!(store.load(oid).unwrap(), (length(2), third.tid));
        });
        blocker.abort();
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_read_current_check_waits_while_a_hand_off_is_in_flight() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.vote().map(|()| txn)
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let sequencer = store.lock_sequencer();
            let first = scope.spawn(move || first.finish());
            // The second vote merges, then aborts. The store still does not
            // hold the first commit.
            second.join().unwrap().unwrap().abort();
            let reader = scope.spawn(|| {
                let (other, _) = (store.new_oid().unwrap(), ());
                let mut txn = writing(&store, other, Tid::ZERO, &length(7));
                txn.check_current_serial(oid, read).unwrap();
                txn.vote()
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            thread::sleep(Duration::from_millis(50));
            assert!(!reader.is_finished());
            drop(sequencer);
            first.join().unwrap().unwrap();
            // The reader reads the serial of the first commit.
            assert!(matches!(
                reader.join().unwrap(),
                Err(ZoeError::ReadConflict { oid: o }) if o == oid
            ));
        });
        assert!(store.locks.is_empty());
    }

    /// An observer that holds the leader in `batch_synced` while `armed`:
    /// its batch is visible, and its members have no outcome yet.
    #[derive(Default)]
    struct Gate {
        armed: std::sync::atomic::AtomicBool,
        held: std::sync::atomic::AtomicBool,
        open: std::sync::atomic::AtomicBool,
    }

    impl Observer for Gate {
        fn batch_synced(&self, _: usize) {
            if self.armed.load(Ordering::SeqCst) {
                self.held.store(true, Ordering::SeqCst);
                while !self.open.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// Hold the first commit visible, before its finish ends its hand-off,
    /// and let a transaction that read it write `third` (`None`: a
    /// deletion). Return the outcome of its vote and the oids that tier 1
    /// merged.
    fn after_a_visible_hand_off(third: Option<Vec<u8>>) -> Result<Vec<Oid>> {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let gate = Arc::new(Gate::default());
        store.set_observer(gate.clone());
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.vote().map(|()| txn)
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            gate.armed.store(true, Ordering::SeqCst);
            let first = scope.spawn(move || first.finish());
            // The second vote took the hand-off, and gives the lock back
            // with it.
            second.join().unwrap().unwrap().abort();
            wait_until(|| gate.held.load(Ordering::SeqCst));
            let (data, serial) = store.load(oid).unwrap();
            assert_eq!(data, length(1));
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            match &third {
                Some(data) => txn.store(oid, serial, data).unwrap(),
                None => txn.delete(oid, serial).unwrap(),
            }
            let voted = txn.vote().map(|()| txn.natively_resolved());
            gate.open.store(true, Ordering::SeqCst);
            first.join().unwrap().unwrap();
            voted
        })
    }

    #[test]
    fn a_deletion_that_read_a_commit_in_flight_passes() {
        let voted = after_a_visible_hand_off(None);
        assert!(voted.is_ok(), "{voted:?}");
    }

    #[test]
    fn a_store_that_read_a_commit_in_flight_passes_without_a_merge() {
        let voted = after_a_visible_hand_off(Some(length(5)));
        assert_eq!(voted.unwrap(), Vec::<Oid>::new());
    }

    #[test]
    fn a_read_current_check_of_an_oid_with_a_commit_in_flight_fails() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let (oid, read) = created(&store, &length(0));
        let mut first = writing(&store, oid, read, &length(1));
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = writing(&store, oid, read, &length(1));
                txn.check_current_serial(oid, read).unwrap();
                txn.vote().map(drop)
            });
            wait_until(|| store.locks.waiting(oid) == 1);
            let sequencer = store.lock_sequencer();
            let first = scope.spawn(move || first.finish());
            let voted = second.join().unwrap();
            drop(sequencer);
            first.join().unwrap().unwrap();
            // The first commit changed the oid after the read.
            assert!(
                matches!(voted, Err(ZoeError::ReadConflict { oid: o }) if o == oid),
                "{voted:?}"
            );
        });
        assert!(store.locks.is_empty());
    }

    /// A clock whose `waiter` panics once it is armed.
    #[derive(Clone, Default)]
    struct PanickingClock(Arc<std::sync::atomic::AtomicBool>);

    impl crate::clock::Clock for PanickingClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::now()
        }

        fn waiter(&self) -> Arc<dyn Waiter> {
            assert!(!self.0.load(Ordering::SeqCst), "the clock fails");
            crate::clock::SystemClock.waiter()
        }
    }

    #[test]
    fn a_clock_that_panics_in_a_finish_leaves_no_member_in_the_queue() {
        let clock = PanickingClock::default();
        let store = Store::open_full(
            MemVfs::new(),
            clock.clone(),
            crate::store::Options::default(),
        )
        .unwrap();
        let (first_oid, second_oid) = (store.new_oid().unwrap(), store.new_oid().unwrap());
        let first = voted(&store, first_oid, b"first");
        let second = voted(&store, second_oid, b"second");
        thread::scope(|scope| {
            // The first finish leads a batch, and waits for the sequencer.
            let sequencer = store.lock_sequencer();
            let joined = store.group.joined();
            let first = scope.spawn(move || first.finish());
            wait_until(|| store.group.joined() == joined + 1);
            clock.0.store(true, Ordering::SeqCst);
            // The second finish panics before it joins the queue, and its
            // transaction releases its locks.
            assert!(scope.spawn(move || second.finish()).join().is_err());
            assert_eq!(store.group.joined(), joined + 1);
            drop(sequencer);
            first.join().unwrap().unwrap();
        });
        assert!(matches!(
            store.load(second_oid),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(store.locks.is_empty());
    }
}
