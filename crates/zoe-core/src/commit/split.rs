//! The split merge of ADR 0048 in the vote that takes the locks.
//!
//! The first conflict of a vote whose merge policy sets `split_merges` to
//! `on` or `shadow` makes a [`Plan`] for the whole transaction. The plan
//! runs the rules of `merge::split` on each node of BTrees that the
//! transaction stored and that is stale, or whose split bucket is stale,
//! and on each stale node that the transaction only marked read-current.
//!
//! In mode `on`, the vote stages the records of each merge over the serials
//! of the committed records, lets each node of rule 1 pass, and fails on a
//! refusal. The plan keeps the reason of each stale read-current node for
//! which rule 1 gives no answer, for the entry of a read conflict. In mode
//! `shadow`, the vote goes on as without the rules, and the entries of the
//! stale oids of the plan carry the outcome of the rules. A commit with no
//! conflict makes no plan.
//!
//! Before the rules read the store, the plan waits until each commit in
//! flight of a hand-off is durable or failed.
//!
//! Rule 3 reads and writes the new bucket of the other side only under its
//! exclusive lock, which the plan takes without a wait (ADR 0049). The plan
//! gives these locks to the vote. A group of records that the vote does
//! not take releases its locks at once. In mode `shadow`, the plan takes
//! no lock and writes nothing.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::time::Duration;

use super::{READ_CHUNK, Txn};
use crate::error::Result;
use crate::lock::{Held, Waited};
use crate::merge::policy::Rules;
use crate::merge::split::{self, Source, Split, Written};
use crate::merge::{Mode, Reason, Resolution, Resolved};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// What the split merge decided for the stale oids of a vote.
#[derive(Default)]
pub(super) struct Plan {
    /// The stale written oids whose conflict the plan decided: the vote
    /// stages their merged records.
    pub(super) covered: BTreeSet<Oid>,
    /// The stale read-current oids that rule 1 lets through.
    pub(super) passed: BTreeSet<Oid>,
    /// The records to stage: the oid, the serial of the committed record
    /// and the merged pickle.
    pub(super) records: Vec<(Oid, Tid, Vec<u8>)>,
    /// The stored oids that the merges resolve without a record: the vote
    /// takes them out of the staging.
    pub(super) dropped: Vec<Oid>,
    /// The oids of `records` that the transaction did not store.
    pub(super) unstored: Vec<Oid>,
    /// The locks that rule 3 took, for the vote to hold.
    pub(super) locked: Vec<Held>,
    /// The entries of the resolution log of the plan, in mode `on`.
    pub(super) resolutions: Vec<Resolution>,
    /// The node of a refusal, with its current serial and the serial that
    /// the transaction read: the vote fails on it.
    pub(super) refused: Option<(Oid, Tid, Tid)>,
    /// In mode `shadow`, what the rules give for each stale oid of a node,
    /// and for each stale read-current node: rule 1 lets it through or
    /// gives no answer.
    pub(super) shadows: BTreeMap<Oid, Shadow>,
    /// In mode `on`, the stale read-current nodes for which rule 1 gives no
    /// answer: the class of each one and the reason.
    pub(super) unanswered: BTreeMap<Oid, (String, &'static str)>,
    /// The transaction size with the merged pickles.
    pub(super) stored_bytes: u64,
    /// The time that the plan waited for commits in flight.
    pub(super) waited: Duration,
}

/// What the rules give for a stale oid in mode `shadow`.
pub(super) struct Shadow {
    /// The outcome of the rules.
    pub(super) outcome: Resolved,
    /// The rule.
    pub(super) rule: u8,
    /// The class of the oid, as `module.name`.
    pub(super) class: String,
}

/// Returns the mode of two switches together: `Off` when one is off, else
/// `Shadow` when one is in shadow mode, else `On`.
fn both(a: Mode, b: Mode) -> Mode {
    match (a, b) {
        (Mode::Off, _) | (_, Mode::Off) => Mode::Off,
        (Mode::Shadow, _) | (_, Mode::Shadow) => Mode::Shadow,
        _ => Mode::On,
    }
}

/// The records of a transaction and of its store, as the rules read them.
struct TxnSource<'t, S: Deref<Target = Store>> {
    txn: &'t Txn<S>,
    /// The locks that the rules took.
    locked: RefCell<Vec<Held>>,
    /// True while the rules run in mode `shadow`: they take no lock.
    shadow: Cell<bool>,
}

impl<S: Deref<Target = Store>> TxnSource<'_, S> {
    /// Release the locks that the rules took after the first `kept`.
    fn release_from(&self, kept: usize) {
        let mut locked = self.locked.borrow_mut();
        if locked.len() > kept {
            let released = locked.split_off(kept);
            self.txn.store.locks.release(&released);
        }
    }
}

impl<S: Deref<Target = Store>> Source for TxnSource<'_, S> {
    fn staged(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>> {
        match self.txn.staged.record(oid) {
            Some(staged) if !staged.restored && staged.pickle_len() > 0 => {}
            _ => return Ok(None),
        }
        Ok(self
            .txn
            .staged
            .get(oid)?
            .map(|(serial, pickle)| (serial, pickle.into_owned())))
    }

    fn stores(&self, oid: Oid) -> bool {
        self.txn.staged.record(oid).is_some()
    }

    fn current(&self, oid: Oid) -> Result<Option<(Tid, Vec<u8>)>> {
        Ok(self
            .txn
            .store
            .backend
            .load_before(oid, Tid::MAX)?
            .filter(|record| !record.data.is_empty())
            .map(|record| (record.tid, record.data)))
    }

    fn at(&self, oid: Oid, serial: Tid) -> Result<Option<Vec<u8>>> {
        self.txn.store.backend.load_serial(oid, serial)
    }

    fn read_current(&self, oid: Oid) -> bool {
        self.txn.read_current.contains_key(&oid)
    }

    fn server_writes(&self) -> bool {
        self.txn.server_writes
    }

    fn lock(&self, oid: Oid) -> Result<bool> {
        if self.shadow.get() {
            return Ok(true);
        }
        let mut locked = self.locked.borrow_mut();
        if locked.iter().any(|held| held.oid == oid) {
            return Ok(true);
        }
        let store: &Store = &self.txn.store;
        match store.locks.try_exclusive(oid, &*store.clock) {
            Some(held) => {
                locked.push(held);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl<S: Deref<Target = Store>> Txn<S> {
    /// Makes the plan of the split merge of the vote, with the settings of
    /// `rules`, for a transaction of `stored_bytes`. The vote holds its
    /// locks.
    pub(super) fn split_plan(&self, rules: &Rules<'_>, stored_bytes: u64) -> Result<Plan> {
        let source = TxnSource {
            txn: self,
            locked: RefCell::new(Vec::new()),
            shadow: Cell::new(false),
        };
        match self.plan_with(&source, rules, stored_bytes) {
            Ok(mut plan) => {
                plan.locked = source.locked.into_inner();
                Ok(plan)
            }
            Err(error) => {
                // A plan that fails gives its locks back.
                source.release_from(0);
                Err(error)
            }
        }
    }

    /// Makes the plan of `split_plan` with the records of `source`.
    fn plan_with(
        &self,
        source: &TxnSource<'_, S>,
        rules: &Rules<'_>,
        stored_bytes: u64,
    ) -> Result<Plan> {
        let mut plan = Plan {
            stored_bytes,
            ..Plan::default()
        };
        let mode = rules.split_merges();
        if mode == Mode::Off {
            return Ok(plan);
        }
        // The rules read the store, so the store must hold the last commit
        // of each oid that came with a hand-off. A late wait gives no plan:
        // the vote goes on as without the rules.
        let store: &Store = &self.store;
        for (&oid, handoff) in &self.handed {
            if handoff.commit.ended().is_some() {
                continue;
            }
            let started = store.observer().map(|_| store.clock.monotonic());
            let waited = handoff.commit.wait(
                oid,
                self.lock_deadline,
                &*store.clock,
                self.lock_cancel.as_ref(),
            );
            if let Some(started) = started {
                plan.waited += store.clock.monotonic().saturating_sub(started);
            }
            if matches!(waited, Waited::TimedOut | Waited::Cancelled) {
                return Ok(plan);
            }
        }
        // The written oids that the rules can merge, with the serial that
        // the transaction read and the current serial.
        let staged: Vec<(Oid, Tid)> = self
            .staged
            .iter()
            .filter(|(_, staged)| {
                !staged.restored && staged.pickle_len() > 0 && staged.serial != Tid::ZERO
            })
            .map(|(oid, staged)| (oid, staged.serial))
            .collect();
        let mut serials = BTreeMap::new();
        for chunk in staged.chunks(READ_CHUNK) {
            let current = self.current_serials(chunk.iter().map(|&(oid, _)| oid))?;
            for (&(oid, read), current) in chunk.iter().zip(current) {
                serials.insert(oid, (read, current.unwrap_or(Tid::ZERO)));
            }
        }
        let stale = |oid: &Oid| serials.get(oid).is_some_and(|(read, now)| read != now);
        // The nodes that the transaction stored. A stale bucket can be the
        // anchor of a split of a node that is not stale.
        let mut nodes = Vec::new();
        let mut stale_bucket = false;
        for &(oid, _) in &staged {
            let Some((_, pickle)) = self.staged.get(oid)? else {
                continue;
            };
            if let Some(class) = split::node_class(&pickle) {
                nodes.push((oid, class));
            } else if stale(&oid) && split::is_bucket(&pickle) {
                stale_bucket = true;
            }
        }
        let mut claimed = BTreeSet::new();
        for (oid, class) in nodes {
            if !stale(&oid) && !stale_bucket {
                continue;
            }
            let node_mode = both(mode, rules.tier1_mode(&class));
            if node_mode == Mode::Off {
                continue;
            }
            source.shadow.set(node_mode == Mode::Shadow);
            let kept = source.locked.borrow().len();
            match split::written_node(source, oid)? {
                Split::Merged { rule, records } => {
                    let group_mode = records.iter().fold(node_mode, |all, record| {
                        both(all, rules.tier1_mode(&record.class))
                    });
                    if group_mode == Mode::Off
                        || records.iter().any(|record| claimed.contains(&record.oid))
                    {
                        source.release_from(kept);
                        continue;
                    }
                    if group_mode == Mode::Shadow {
                        source.release_from(kept);
                        for record in records
                            .iter()
                            .filter(|record| record.read != record.current)
                        {
                            plan.shadows.insert(
                                record.oid,
                                Shadow {
                                    outcome: Resolved::Merged,
                                    rule,
                                    class: record.class.clone(),
                                },
                            );
                        }
                        continue;
                    }
                    let Some(size) = records.iter().try_fold(plan.stored_bytes, |size, record| {
                        self.merged_size(record.oid, &record.pickle, size)
                    }) else {
                        source.release_from(kept);
                        continue;
                    };
                    plan.stored_bytes = size;
                    for record in records {
                        claimed.insert(record.oid);
                        if record.read != record.current {
                            plan.covered.insert(record.oid);
                        }
                        plan.resolutions.push(entry(&record, rule, rules));
                        if record.dropped {
                            plan.dropped.push(record.oid);
                            continue;
                        }
                        if self.staged.record(record.oid).is_none() {
                            plan.unstored.push(record.oid);
                        }
                        plan.records
                            .push((record.oid, record.current, record.pickle));
                    }
                }
                Split::Refused {
                    rule,
                    reason,
                    oid,
                    class,
                } => {
                    source.release_from(kept);
                    // The refusal names the stale record of its range, the
                    // record that the vote reports as in conflict.
                    let Some(&(read, current)) = serials.get(&oid) else {
                        continue;
                    };
                    // The mode of the record of the refusal joins the mode
                    // of the node, as for a merged group.
                    let group_mode = both(node_mode, rules.tier1_mode(&class));
                    if group_mode == Mode::Off {
                        continue;
                    }
                    if group_mode == Mode::Shadow {
                        plan.shadows.insert(
                            oid,
                            Shadow {
                                outcome: Resolved::Refused(reason),
                                rule,
                                class,
                            },
                        );
                        continue;
                    }
                    plan.resolutions
                        .push(refusal(oid, class, read, current, rule, reason, rules));
                    plan.refused = Some((oid, current, read));
                    return Ok(plan);
                }
                Split::Passed | Split::Undecided(_) => source.release_from(kept),
            }
        }
        source.shadow.set(false);
        // Rule 1: the stale nodes that the transaction only read.
        let read: Vec<(Oid, Tid)> = self
            .read_current
            .iter()
            .filter(|&(oid, _)| self.staged.record(*oid).is_none())
            .map(|(&oid, &read)| (oid, read))
            .collect();
        for chunk in read.chunks(READ_CHUNK) {
            let current = self.current_serials(chunk.iter().map(|&(oid, _)| oid))?;
            for (&(oid, read), current) in chunk.iter().zip(current) {
                let current = current.unwrap_or(Tid::ZERO);
                if current == read {
                    continue;
                }
                let Some((_, record)) = source.current(oid)? else {
                    continue;
                };
                let Some(class) = split::node_class(&record) else {
                    continue;
                };
                let node_mode = both(mode, rules.tier1_mode(&class));
                if node_mode == Mode::Off {
                    continue;
                }
                let outcome = match split::read_node(source, oid, read)? {
                    Split::Passed => Resolved::Merged,
                    Split::Undecided(why) => Resolved::Undecided(why),
                    Split::Merged { .. } | Split::Refused { .. } => continue,
                };
                if node_mode == Mode::Shadow {
                    plan.shadows.insert(
                        oid,
                        Shadow {
                            outcome,
                            rule: 1,
                            class,
                        },
                    );
                    continue;
                }
                if let Resolved::Undecided(why) = outcome {
                    plan.unanswered.insert(oid, (class, why));
                    continue;
                }
                plan.passed.insert(oid);
                let passed = Written {
                    oid,
                    class,
                    read,
                    current,
                    pickle: Vec::new(),
                    keys: Vec::new(),
                    changed: 0,
                    dropped: false,
                };
                plan.resolutions.push(entry(&passed, 1, rules));
            }
        }
        Ok(plan)
    }
}

/// Returns the entry of the resolution log of a record that `rule`
/// merged, or of a node that rule 1 lets through, in mode `on`. The vote
/// marks it applied when it passes.
fn entry(record: &Written, rule: u8, rules: &Rules<'_>) -> Resolution {
    Resolution {
        oid: record.oid,
        tier: 1,
        class: record.class.clone(),
        mode: Mode::On,
        policy: rules.version(),
        outcome: Resolved::Merged,
        applied: false,
        undo: false,
        keys: record.keys.clone(),
        changed: record.changed,
        committed: record.current,
        read: record.read,
        shadow: None,
        fields: Vec::new(),
        split: Some(rule),
    }
}

/// Returns the entry of the resolution log of a refusal of `rule` on the
/// node `oid`, in mode `on`: the outcome of the vote.
fn refusal(
    oid: Oid,
    class: String,
    read: Tid,
    current: Tid,
    rule: u8,
    reason: Reason,
    rules: &Rules<'_>,
) -> Resolution {
    Resolution {
        oid,
        tier: 1,
        class,
        mode: Mode::On,
        policy: rules.version(),
        outcome: Resolved::Refused(reason),
        applied: true,
        undo: false,
        keys: Vec::new(),
        changed: 0,
        committed: current,
        read,
        shadow: None,
        fields: Vec::new(),
        split: Some(rule),
    }
}

/// Returns the entry of the resolution log of a stale read-current node on
/// which the vote fails in mode `on`, when rule 1 gives no answer for it:
/// `why` is the reason of rule 1. The vote fails as without the rules, so
/// the entry is not applied.
pub(super) fn read_conflict(
    oid: Oid,
    class: &str,
    why: &'static str,
    read: Tid,
    current: Tid,
    rules: &Rules<'_>,
) -> Resolution {
    Resolution {
        oid,
        tier: 1,
        class: class.to_owned(),
        mode: Mode::On,
        policy: rules.version(),
        outcome: Resolved::Undecided(why),
        applied: false,
        undo: false,
        keys: Vec::new(),
        changed: 0,
        committed: current,
        read,
        shadow: None,
        fields: Vec::new(),
        split: Some(1),
    }
}

/// Returns the entry of the resolution log of a stale read-current node in
/// mode `shadow`: the vote fails as without the rules, and the shadow
/// outcome tells what rule 1 gives: the node passes, or no answer.
pub(super) fn read_shadow(
    oid: Oid,
    shadow: &Shadow,
    read: Tid,
    current: Tid,
    rules: &Rules<'_>,
) -> Resolution {
    Resolution {
        oid,
        tier: 1,
        class: shadow.class.clone(),
        mode: Mode::Shadow,
        policy: rules.version(),
        outcome: Resolved::Undecided("a read-current oid"),
        applied: false,
        undo: false,
        keys: Vec::new(),
        changed: 0,
        committed: current,
        read,
        shadow: Some(shadow.outcome),
        fields: Vec::new(),
        split: Some(shadow.rule),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use zoe_pickle::btrees::new_bucket_state;
    use zoe_pickle::python::{Heap, Pickler, Value};

    use crate::error::ZoeError;
    use crate::merge::Resolved;
    use crate::store::Store;
    use crate::types::{Oid, Tid};
    use crate::vfs::MemVfs;

    fn record(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
        let mut pickler = Pickler::new(heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    }

    /// Returns a reference to the `IIBucket` `oid`.
    fn reference(heap: &mut Heap, oid: Oid) -> Value {
        let bytes = heap.new_bytes(&oid.0.to_be_bytes());
        let class = heap.new_class("BTrees.IIBTree", "IIBucket");
        let data = heap.new_tuple(vec![bytes, class]);
        heap.new_reference(data).unwrap()
    }

    /// Returns the record of an `IIBucket` of `keys`, each with the value
    /// 1, and its next bucket.
    fn bucket(keys: &[i128], next: Option<Oid>) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.IIBTree", "IIBucket");
        let items = keys.iter().flat_map(|&k| [Value::Int(k), Value::Int(1)]);
        let next = next.map(|oid| reference(&mut heap, oid));
        let state = new_bucket_state(&mut heap, items.collect(), next);
        record(&heap, class, state)
    }

    /// Returns the record of an `IIBTree` whose children are the buckets
    /// `first`, then each separator and its bucket.
    fn node(first: Oid, children: &[(i128, Oid)]) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.IIBTree", "IIBTree");
        let mut items = vec![reference(&mut heap, first)];
        for &(separator, oid) in children {
            items.push(Value::Int(separator));
            items.push(reference(&mut heap, oid));
        }
        let children = heap.new_tuple(items);
        let first = reference(&mut heap, first);
        let state = heap.new_tuple(vec![children, first]);
        record(&heap, class, state)
    }

    fn length(value: i128) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class("BTrees.Length", "Length");
        record(&heap, class, Value::Int(value))
    }

    /// Returns the record of a policy that sets `split_merges` to `mode`,
    /// and the root that names it at `policy`.
    fn policy_and_root(policy: Oid, mode: &str) -> (Vec<u8>, Vec<u8>) {
        let mut heap = Heap::new();
        let class = heap.new_class("zoe.policy", "MergePolicy");
        let classes = heap.new_dict(Vec::new());
        let items = vec![
            (heap.new_str("version"), Value::Int(1)),
            (heap.new_str("classes"), classes),
            (heap.new_str("split_merges"), heap.new_str(mode)),
        ];
        let state = heap.new_dict(items);
        let root = [
            &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\
               \x00dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
            &policy.0.to_be_bytes(),
            b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
        ]
        .concat();
        (record(&heap, class, state), root)
    }

    fn commit(store: &Store, writes: &[(Oid, Tid, Vec<u8>)]) -> Tid {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for (oid, serial, data) in writes {
            txn.store(*oid, *serial, data).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap()
    }

    fn new_oid(store: &Store) -> Oid {
        loop {
            let oid = store.new_oid().unwrap();
            if oid != Oid(0) {
                return oid;
            }
        }
    }

    fn wait_until(done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Two transactions split the two buckets of one node from one
    /// snapshot. The second one votes while the commit of the first one is
    /// in flight, with the lock of the node handed off. With `stale_first`,
    /// a `Length` below the node in oid order conflicts too, so the plan
    /// of the split merge starts before the vote checks the node. In mode
    /// `shadow`, the vote waits as in mode `on`, then fails on the node as
    /// without the rules.
    fn merge_after_a_commit_in_flight(stale_first: bool, mode: &str) {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let policy = new_oid(&store);
        let (policy_record, root) = policy_and_root(policy, mode);
        commit(&store, &[(policy, Tid::ZERO, policy_record)]);
        commit(&store, &[(Oid(0), Tid::ZERO, root)]);
        let counter = new_oid(&store);
        let counted = commit(&store, &[(counter, Tid::ZERO, length(0))]);
        let (n, b0, b1) = (new_oid(&store), new_oid(&store), new_oid(&store));
        let read = commit(
            &store,
            &[
                (n, Tid::ZERO, node(b0, &[(40, b1)])),
                (b0, Tid::ZERO, bucket(&[10, 20, 30], Some(b1))),
                (b1, Tid::ZERO, bucket(&[40, 50, 60], None)),
            ],
        );
        if stale_first {
            commit(&store, &[(counter, counted, length(1))]);
        }
        let (y, x) = (new_oid(&store), new_oid(&store));
        let mut first = store.tpc_begin(b"", b"", b"").unwrap();
        for (oid, serial, data) in [
            (n, read, node(b0, &[(20, y), (40, b1)])),
            (b0, read, bucket(&[10, 15], Some(y))),
            (y, Tid::ZERO, bucket(&[20, 30], Some(b1))),
        ] {
            first.store(oid, serial, &data).unwrap();
        }
        first.vote().unwrap();
        thread::scope(|scope| {
            let second = scope.spawn(|| {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                if stale_first {
                    txn.store(counter, counted, &length(2)).unwrap();
                }
                for (oid, serial, data) in [
                    (n, read, node(b0, &[(40, b1), (50, x)])),
                    (b1, read, bucket(&[40, 45], Some(x))),
                    (x, Tid::ZERO, bucket(&[50, 60], None)),
                ] {
                    txn.store(oid, serial, &data).unwrap();
                }
                let voted = txn.vote_resolving();
                (voted, txn)
            });
            wait_until(|| store.locks.waiting(n) == 1);
            let sequencer = store.lock_sequencer();
            let finish = scope.spawn(move || first.finish());
            // The second vote has the lock of the node, and waits for the
            // durable commit of the first one before the rules read the
            // store.
            wait_until(|| store.locks.waiting(n) == 0);
            thread::sleep(Duration::from_millis(50));
            assert!(!second.is_finished());
            drop(sequencer);
            finish.join().unwrap().unwrap();
            let (voted, txn) = second.join().unwrap();
            if mode == "shadow" {
                assert!(matches!(voted, Err(ZoeError::Conflict { oid }) if oid == n));
                let shadowed = txn.resolutions().iter().any(|resolution| {
                    resolution.oid == n
                        && resolution.split == Some(2)
                        && resolution.shadow == Some(Resolved::Merged)
                });
                assert!(shadowed, "{:?}", txn.resolutions());
                return;
            }
            assert_eq!(voted.unwrap(), []);
            assert!(txn.natively_resolved().contains(&n));
            txn.finish().unwrap();
        });
        assert!(store.locks.is_empty());
        if mode == "shadow" {
            return;
        }
        let (data, _) = store.load(n).unwrap();
        assert_eq!(data, node(b0, &[(20, y), (40, b1), (50, x)]));
        if stale_first {
            assert_eq!(store.load(counter).unwrap().0, length(3));
        }
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_split_merge_waits_for_a_commit_in_flight_of_its_node() {
        merge_after_a_commit_in_flight(false, "on");
    }

    #[test]
    fn a_plan_that_starts_before_the_node_waits_for_its_commit_in_flight() {
        merge_after_a_commit_in_flight(true, "on");
    }

    #[test]
    fn in_shadow_mode_the_plan_waits_for_its_commit_in_flight() {
        merge_after_a_commit_in_flight(false, "shadow");
    }

    /// The store, its node `n` of two buckets `b0` [10, 20, 30] and `b1`
    /// [40, 50, 60] at `read`, and the split of S1 that made `y` [60, 70]
    /// at `s1`.
    fn after_s1() -> (Store, Oid, Oid, Oid, Oid, Tid, Tid) {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        let policy = new_oid(&store);
        let (policy_record, root) = policy_and_root(policy, "on");
        commit(&store, &[(policy, Tid::ZERO, policy_record)]);
        commit(&store, &[(Oid(0), Tid::ZERO, root)]);
        let (n, b0, b1) = (new_oid(&store), new_oid(&store), new_oid(&store));
        let read = commit(
            &store,
            &[
                (n, Tid::ZERO, node(b0, &[(40, b1)])),
                (b0, Tid::ZERO, bucket(&[10, 20, 30], Some(b1))),
                (b1, Tid::ZERO, bucket(&[40, 50, 60], None)),
            ],
        );
        let y = new_oid(&store);
        let s1 = commit(
            &store,
            &[
                (n, read, node(b0, &[(40, b1), (60, y)])),
                (b1, read, bucket(&[40, 50], Some(y))),
                (y, Tid::ZERO, bucket(&[60, 70], None)),
            ],
        );
        (store, n, b0, b1, y, read, s1)
    }

    #[test]
    fn a_node_without_a_record_is_never_handed_off() {
        let (owned, n, b0, b1, y, read, s1) = after_s1();
        let store = &owned;
        // S2 read before S1 and splits b1: the node keeps the record of S1.
        let x = new_oid(&store);
        let mut s2 = store.tpc_begin(b"", b"", b"").unwrap();
        s2.set_server_writes(true);
        for (oid, serial, data) in [
            (n, read, node(b0, &[(40, b1), (50, x)])),
            (b1, read, bucket(&[40, 45], Some(x))),
            (x, Tid::ZERO, bucket(&[50, 60], None)),
        ] {
            s2.store(oid, serial, &data).unwrap();
        }
        s2.vote().unwrap();
        thread::scope(|scope| {
            // T' read after S1, splits b0 and stores the node.
            let z = new_oid(&store);
            let other = scope.spawn(move || {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                for (oid, serial, data) in [
                    (n, s1, node(b0, &[(20, z), (40, b1), (60, y)])),
                    (b0, read, bucket(&[10, 15], Some(z))),
                    (z, Tid::ZERO, bucket(&[20, 30], Some(b1))),
                ] {
                    txn.store(oid, serial, &data).unwrap();
                }
                txn.vote().unwrap();
                txn.finish().unwrap();
                z
            });
            wait_until(|| store.locks.waiting(n) == 1);
            let sequencer = store.lock_sequencer();
            let finish = scope.spawn(move || s2.finish());
            // The finish of S2 is in flight: the node is not staged, so it
            // gives no hand-off, and T' still waits for it.
            thread::sleep(Duration::from_millis(50));
            assert_eq!(store.locks.waiting(n), 1);
            drop(sequencer);
            finish.join().unwrap().unwrap();
            let z = other.join().unwrap();
            assert_eq!(
                store.load(n).unwrap().0,
                node(b0, &[(20, z), (40, b1), (60, y)])
            );
        });
        assert_eq!(store.load(b1).unwrap().0, bucket(&[40, 45, 50], Some(y)));
        assert!(store.load(x).is_err());
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_vote_that_gets_the_node_at_the_hand_off_waits_for_the_kept_bucket() {
        let (owned, n, b0, b1, y, read, s1) = after_s1();
        let store = &owned;
        // S2 read before S1, splits b0 (rule 2) and b1 (rule 3): the node
        // gets a record.
        let (z, x) = (new_oid(&store), new_oid(&store));
        let mut s2 = store.tpc_begin(b"", b"", b"").unwrap();
        s2.set_server_writes(true);
        for (oid, serial, data) in [
            (n, read, node(b0, &[(20, z), (40, b1), (50, x)])),
            (b0, read, bucket(&[10, 15], Some(z))),
            (z, Tid::ZERO, bucket(&[20, 30], Some(b1))),
            (b1, read, bucket(&[40, 45], Some(x))),
            (x, Tid::ZERO, bucket(&[50, 60], None)),
        ] {
            s2.store(oid, serial, &data).unwrap();
        }
        s2.vote().unwrap();
        assert!(s2.wrote_unstored());
        thread::scope(|scope| {
            // T' read after S1, and splits y: it stores the node and y.
            let w = new_oid(&store);
            let other = scope.spawn(move || {
                let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
                txn.set_server_writes(true);
                for (oid, serial, data) in [
                    (n, s1, node(b0, &[(40, b1), (60, y), (70, w)])),
                    (y, s1, bucket(&[60], Some(w))),
                    (w, Tid::ZERO, bucket(&[70, 80], None)),
                ] {
                    txn.store(oid, serial, &data).unwrap();
                }
                txn.vote().unwrap();
                txn.finish().unwrap();
            });
            wait_until(|| store.locks.waiting(n) == 1);
            let sequencer = store.lock_sequencer();
            let finish = scope.spawn(move || s2.finish());
            // T' gets the node at the hand-off, then waits for y, which S2
            // holds until its commit is durable.
            wait_until(|| store.locks.waiting(y) == 1);
            assert_eq!(store.locks.waiting(n), 0);
            drop(sequencer);
            finish.join().unwrap().unwrap();
            other.join().unwrap();
            assert_eq!(
                store.load(n).unwrap().0,
                node(b0, &[(20, z), (40, b1), (60, y), (70, w)])
            );
            assert_eq!(store.load(y).unwrap().0, bucket(&[60], Some(w)));
            assert_eq!(store.load(w).unwrap().0, bucket(&[70, 80], None));
        });
        assert_eq!(store.load(b1).unwrap().0, bucket(&[40, 45, 50], Some(y)));
        assert!(store.locks.is_empty());
    }
}
