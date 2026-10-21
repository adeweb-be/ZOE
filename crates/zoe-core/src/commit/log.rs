//! What the tiers did in a transaction, and the lines of the resolution
//! log.

use std::ops::Deref;
use std::time::SystemTime;

use super::Txn;
use crate::merge::{self, Resolution, Resolved, Verification};
use crate::store::Store;
use crate::types::{Oid, Tid};

impl<S: Deref<Target = Store>> Txn<S> {
    /// Return what tiers 1 and 2 did in the undos of the transaction, then
    /// in the last vote that took the locks: one entry for each three-way
    /// merge of an undo, and one for each stale written oid of the vote, of
    /// a class of a tier whose mode is not `Off`, in oid order. With the
    /// split merge, the vote also gives one entry for each stale
    /// read-current node that rule 1 lets through, and one for the
    /// read-current node on which it fails when rule 1 gives no answer for
    /// it. The vote that comes after `vote_resolving` keeps the entries of
    /// `vote_resolving`.
    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }

    /// Return the lines of the resolution log (ADR 0042) of what the tiers
    /// did in the undos and in the last vote that took the locks: one JSON
    /// object for each entry of `resolutions`, at `time`, for the storage `storage` of a server.
    /// The user and the description of the committed transaction come from
    /// the history of the oid; when the store no longer has that record,
    /// the line gives null for them.
    pub fn resolution_log_lines(&self, time: SystemTime, storage: Option<&str>) -> Vec<String> {
        self.resolution_log_lines_of(0..self.resolutions.len(), time, storage)
    }

    /// Return the number of entries of `resolutions` that the undos of the
    /// transaction gave. They come first, before the entries of the vote.
    pub fn undo_resolution_count(&self) -> usize {
        self.undo_resolutions
    }

    /// Return the lines of the resolution log of the entries `range` of
    /// `resolutions`, as `resolution_log_lines` does. A skin that writes
    /// the lines of each undo when it ends, and those of the vote after it,
    /// writes each line once.
    pub fn resolution_log_lines_of(
        &self,
        range: std::ops::Range<usize>,
        time: SystemTime,
        storage: Option<&str>,
    ) -> Vec<String> {
        self.resolutions[range]
            .iter()
            .map(|resolution| {
                // A conflict with a commit in flight names that commit. Its
                // tid can be unknown at the vote: the line then gives 0.
                if resolution.committed == Tid::ZERO
                    && let Some(handoff) = self.handed.get(&resolution.oid)
                {
                    let commit = &handoff.commit;
                    let mut resolution = resolution.clone();
                    resolution.committed = commit.tid().unwrap_or(Tid::ZERO);
                    let context = merge::LogContext {
                        time,
                        storage,
                        user: &self.user,
                        description: &self.description,
                        committed_user: Some(&commit.user),
                        committed_description: Some(&commit.description),
                    };
                    return merge::log_line(&resolution, &context);
                }
                let committed = self.committed_entry(resolution.oid, resolution.committed);
                let context = merge::LogContext {
                    time,
                    storage,
                    user: &self.user,
                    description: &self.description,
                    committed_user: committed.as_ref().map(|entry| &entry.user[..]),
                    committed_description: committed.as_ref().map(|entry| &entry.description[..]),
                };
                merge::log_line(resolution, &context)
            })
            .collect()
    }

    /// Return what the vote after `vote_resolving` found for each merge of
    /// shadow mode that the caller resolved: whether the record of the
    /// caller is the merged record (ADR 0046).
    pub fn verifications(&self) -> &[Verification] {
        &self.verifications
    }

    /// Return the lines of the resolution log of `verifications`, at
    /// `time`, for the storage `storage` of a server.
    pub fn verification_log_lines(&self, time: SystemTime, storage: Option<&str>) -> Vec<String> {
        self.verifications
            .iter()
            .map(|verification| {
                let committed = self.committed_entry(verification.oid, verification.committed);
                let context = merge::LogContext {
                    time,
                    storage,
                    user: &self.user,
                    description: &self.description,
                    committed_user: committed.as_ref().map(|entry| &entry.user[..]),
                    committed_description: committed.as_ref().map(|entry| &entry.description[..]),
                };
                merge::verification_line(verification, &context)
            })
            .collect()
    }

    /// Return the oids that tier 1 merged in the last vote that took the
    /// locks and passed. Their staged records hold the merged records, over
    /// the serial of the committed records.
    pub fn natively_resolved(&self) -> Vec<Oid> {
        // A node that rule 1 of the split merge let through has no merged
        // record: the transaction did not store it.
        self.resolutions[self.undo_resolutions..]
            .iter()
            .filter(|resolution| {
                resolution.applied
                    && resolution.outcome == Resolved::Merged
                    && resolution.split != Some(1)
            })
            .map(|resolution| resolution.oid)
            .collect()
    }

    /// Return the history entry of `oid` at `tid`, when the store still has
    /// it.
    fn committed_entry(&self, oid: Oid, tid: Tid) -> Option<crate::read::HistoryEntry> {
        self.store
            .history(oid, HISTORY_SEARCH)
            .ok()
            .and_then(|entries| entries.into_iter().find(|entry| entry.tid == tid))
    }
}

/// The most records of the history of an oid that a line of the resolution
/// log reads to find the committed record. Other commits can come after
/// the vote, once it released its locks.
const HISTORY_SEARCH: usize = 8;
