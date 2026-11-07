//! The versions of one mirror: a base, one change set for each commit
//! after it, the mirror window and the rebase.
//!
//! A version at tid `v` holds each commit up to `v`. It is the base and
//! its overrides: for a version after the base, the new values of the
//! change sets after the base up to `v`; for a version before the base,
//! the old values of the change sets after `v` up to the base. A query at
//! `v` is served when each commit after `v` has its change set in the
//! mirror: `v` is at or after `floor`.
//!
//! A change set leaves the mirror window when `window_tids` newer change
//! sets exist, or when it is older than `window`. The mirror drops such a
//! change set when a base holds it. The rebase builds a new base at the
//! last tid: a new structure for each index that the change sets touched,
//! one at a time; the other structures stay shared with the old base.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::base::{Base, Index};
use crate::change::{ChangeSet, Overrides, Slot};

/// The versions of a mirror that queries read.
#[derive(Debug)]
pub struct Versions {
    /// The base.
    pub base: Arc<Base>,
    /// The change sets, by ascending tid. Those at or before the tid of the
    /// base give the versions before it.
    pub sets: VecDeque<Arc<ChangeSet>>,
    /// The first tid whose version the mirror holds.
    pub floor: u64,
    /// The overrides of the version at `applied`.
    pub head: Arc<Overrides>,
    /// The last tid that the mirror applied: its version holds each commit
    /// up to it.
    pub applied: u64,
}

/// What a query reads of a version: the base and either the overrides, or
/// the change sets that give them.
pub enum Snapshot {
    /// The overrides themselves.
    Head(Arc<Base>, Arc<Overrides>),
    /// The change sets after the base up to the version, by ascending tid.
    After(Arc<Base>, Vec<Arc<ChangeSet>>),
    /// The change sets after the version up to the base, by ascending tid.
    Before(Arc<Base>, Vec<Arc<ChangeSet>>),
}

impl Snapshot {
    /// The base and the overrides of the version.
    pub fn resolve(self) -> (Arc<Base>, Arc<Overrides>) {
        match self {
            Snapshot::Head(base, head) => (base, head),
            Snapshot::After(base, sets) => {
                let mut over = Overrides::default();
                for set in &sets {
                    over.forward(set);
                }
                (base, Arc::new(over))
            }
            Snapshot::Before(base, sets) => {
                let mut over = Overrides::default();
                for set in sets.iter().rev() {
                    over.backward(set);
                }
                (base, Arc::new(over))
            }
        }
    }
}

impl Versions {
    /// The versions of a new base.
    pub fn new(base: Base) -> Versions {
        let tid = base.tid;
        Versions {
            base: Arc::new(base),
            sets: VecDeque::new(),
            floor: tid,
            head: Arc::new(Overrides::default()),
            applied: tid,
        }
    }

    /// What a query at the version `v` reads, or `None` when the mirror no
    /// longer holds it.
    pub fn snapshot(&self, v: u64) -> Option<Snapshot> {
        if v < self.floor {
            return None;
        }
        let base = Arc::clone(&self.base);
        if v >= self.applied {
            return Some(Snapshot::Head(base, Arc::clone(&self.head)));
        }
        if v >= base.tid {
            let sets = self
                .sets
                .iter()
                .filter(|s| s.tid > base.tid && s.tid <= v)
                .cloned()
                .collect();
            return Some(Snapshot::After(base, sets));
        }
        let sets = self
            .sets
            .iter()
            .filter(|s| s.tid > v && s.tid <= base.tid)
            .cloned()
            .collect();
        Some(Snapshot::Before(base, sets))
    }

    /// Adds the change set of the commit at `set.tid`.
    pub fn push(&mut self, set: ChangeSet) {
        self.applied = set.tid;
        if set.changes.is_empty() && set.lengths.is_empty() {
            return;
        }
        Arc::make_mut(&mut self.head).forward(&set);
        self.sets.push_back(Arc::new(set));
    }

    /// Tells if the change set at place `k` is out of the mirror window.
    fn outside(&self, k: usize, window_tids: usize, window: Duration) -> bool {
        let newer = self.sets.len() - 1 - k;
        newer >= window_tids || self.sets[k].at.elapsed() > window
    }

    /// Tells if a rebase is due: the overrides hold more than `rows` rows,
    /// or a change set out of the window is not in the base yet.
    pub fn rebase_due(&self, rows: usize, window_tids: usize, window: Duration) -> bool {
        if self.head.rows() > rows {
            return true;
        }
        let tid = self.base.tid;
        self.sets
            .iter()
            .position(|s| s.tid > tid)
            .is_some_and(|k| self.outside(k, window_tids, window))
    }

    /// Drops the change sets out of the window that the base holds.
    pub fn prune(&mut self, window_tids: usize, window: Duration) {
        while !self.sets.is_empty()
            && self.sets[0].tid <= self.base.tid
            && self.outside(0, window_tids, window)
        {
            if let Some(set) = self.sets.pop_front() {
                self.floor = self.floor.max(set.tid);
            }
        }
    }

    /// Lays a new base, built at a tid that the mirror applied.
    pub fn rebased(&mut self, base: Base) {
        let tid = base.tid;
        let mut head = Overrides::default();
        for set in self.sets.iter().filter(|s| s.tid > tid) {
            head.forward(set);
        }
        self.base = Arc::new(base);
        self.head = Arc::new(head);
    }

    /// The bytes that the versions take: the base, the change sets and the
    /// overrides.
    pub fn heap_bytes(&self) -> usize {
        self.base.heap_bytes()
            + self.sets.iter().map(|s| s.heap_bytes()).sum::<usize>()
            + self.head.heap_bytes()
    }
}

/// Builds the base of the version of `base` with `head` at `tid`. Each
/// index that `head` touches gets a new structure, one at a time; the
/// others stay shared. `room` tells if the bound has room for the new
/// structures that the rebase holds; when it has none, the rebase stops.
pub fn rebase(
    base: &Base,
    head: &Overrides,
    tid: u64,
    room: &dyn Fn(usize) -> bool,
) -> Result<Base, String> {
    let rows = match head.trees.get(&Slot::Paths) {
        Some(paths) => Arc::new(base.rows.with_changes(paths)),
        None => Arc::clone(&base.rows),
    };
    let mut held = if Arc::ptr_eq(&rows, &base.rows) {
        0
    } else {
        rows.heap_bytes()
    };
    if !room(held) {
        return Err("the memory bound has no room for the rebase".into());
    }
    let mut indexes = base.indexes.clone();
    for (i, entry) in indexes.iter_mut().enumerate() {
        if let Some(length) = head.lengths.get(&i) {
            entry.length = *length;
        }
        let Index::Field(field) = &entry.index else {
            continue;
        };
        let rebuilt = match head.trees.get(&Slot::Index(i)) {
            Some(changed) => field.rebuilt(changed, &rows),
            // Every row keeps its number, so a structure that no change
            // touched stays valid.
            None => continue,
        };
        match rebuilt {
            Ok(index) => {
                held += index.heap_bytes();
                if !room(held) {
                    return Err("the memory bound has no room for the rebase".into());
                }
                entry.index = Index::Field(Arc::new(index));
            }
            Err(e) => entry.index = Index::Unavailable(e),
        }
    }
    Ok(Base {
        tid,
        catalog: base.catalog,
        rows,
        indexes,
        by_name: base.by_name.clone(),
        trees: base.trees,
    })
}
