//! `Txn::undo`: the undo of an earlier transaction, staged in this one.
//! `crate::undo` describes it.

use std::ops::Deref;
use std::sync::Arc;

use crate::backend::Record;
use crate::error::{Result, ZoeError};
use crate::merge::policy::Rules;
use crate::merge::{self, Outcome, Resolution, Resolved};
use crate::store::Store;
use crate::types::{Oid, Tid};
use crate::undo::{self, UndoConflict, Undone};

use super::{Txn, undo_tiers};

impl<S: Deref<Target = Store>> Txn<S> {
    /// Stage the undo of the transaction `tid`: for each oid of `tid`, the
    /// state before `tid`. Return the oids, and the conflicts that no tier
    /// resolves.
    ///
    /// The call reads the current state of each oid as this transaction
    /// sees it: the record that it staged for the oid, else the committed
    /// record. So a second undo in the same transaction undoes over the
    /// first one.
    ///
    /// - When the current state is the state of `tid`, the call stages the
    ///   state before `tid` over the serial of the current state, or a
    ///   deletion when `tid` created the object. A blob gets the blob file
    ///   of the state before `tid` by a hard link, or a copy into a new
    ///   file where the filesystem cannot link, so no blob file is written
    ///   again.
    /// - Else the tiers merge the three states, as the vote merges a
    ///   conflict (`tiers`), with the state of `tid` as the base, the
    ///   current state as the committed one and the state before `tid` as
    ///   the new one: tier 1 for a class of BTrees or `Length`, tier 2 for
    ///   a class that the merge policy names. The call stages the merged
    ///   record over the serial of the current state. A refusal fails the
    ///   call with `Undo`. When no tier gives an answer, the oid goes
    ///   into `conflicts`, and the call stages nothing for it: the caller
    ///   resolves it and stores the result over its serial, or fails.
    /// - An undo of the creation of an object, of a deletion, or of a blob,
    ///   that a later transaction changed fails with `Undo`. A blob is
    ///   changed when the current blob record is not the blob record of
    ///   `tid`, also when the pickles are the same, as the pickles of the
    ///   revisions of one blob are. So the undo never drops a later
    ///   revision of a blob, where FileStorage gives the earlier blob back.
    ///
    /// A transaction at or before the last pack, one that the store does
    /// not have, or one whose state before is gone by a pack, fails the
    /// call with `Undo`. A pack that runs during the call can remove the
    /// records that it reads: a read that then fails gives `Undo` too. The
    /// vote checks the pack watermark again after it takes the locks, and
    /// the finish under the commit sequencer: a pack after this call can
    /// remove what the undo gives back. A failed call can leave the undo of
    /// the oids before the one that failed it staged, so the caller then
    /// aborts the transaction.
    ///
    /// The call holds every record of `tid` in memory at once, as the
    /// transaction iterator does for one transaction. The tiers run here
    /// without the locks of the vote: the vote locks the staged oids and
    /// checks their serials as for `store`, so a commit after the call is a
    /// conflict of the vote. The pickles count toward the size limits, as
    /// for `store`. A restore transaction refuses the call with
    /// `Protocol`, and so does a transaction after its vote. A call from
    /// another thread fails with `WrongThread`.
    pub fn undo(&mut self, tid: Tid) -> Result<Undone> {
        self.check_thread()?;
        self.store.writable()?;
        if self.is_restore() {
            return Err(ZoeError::Protocol("undo in a restore transaction"));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("undo after vote"));
        }
        // No transaction has the tid 0, also in a store that was never
        // packed.
        if tid == Tid::ZERO {
            return Err(refused(None, undo::NO_TRANSACTION));
        }
        if tid <= self.store.pack_watermark()? {
            return Err(refused(None, undo::PACKED));
        }
        // From here the transaction can hold a part of the undo, so its
        // vote and its finish check the watermark.
        self.undone = Some(self.undone.map_or(tid, |lowest| lowest.min(tid)));
        self.undo_records(tid)
            .map_err(|error| self.packed_since(tid, error))
    }

    /// Stage the undo of each record of the transaction `tid`.
    fn undo_records(&mut self, tid: Tid) -> Result<Undone> {
        let records = match self.store.backend.txn_records(tid)? {
            Some((row, records)) if row.oid_count == records.len() as u64 => records,
            Some(_) => return Err(ZoeError::CorruptTxn { tid }),
            None => return Err(refused(None, undo::NO_TRANSACTION)),
        };
        let mut undone = Undone::default();
        for (oid, data) in records {
            undone.oids.push(oid);
            if let Some(conflict) = self.undo_oid(oid, tid, &data)? {
                undone.conflicts.push(conflict);
            }
        }
        Ok(undone)
    }

    /// Return the error of an undo of `tid` whose reads failed with `error`.
    /// A pack during the reads can remove the records of `tid`, or the
    /// records before them. So when `tid` is now at or before the pack
    /// watermark, a missing record, a transaction that the store no longer
    /// has, and a record count that does not match give `Undo`, not a
    /// corrupt store.
    fn packed_since(&self, tid: Tid, error: ZoeError) -> ZoeError {
        let pack_can_explain = match &error {
            ZoeError::CorruptTxn { .. } => true,
            // The blob file of a record before `tid`, which a pack removed.
            ZoeError::CorruptBlob { tid: blob, .. } => *blob < tid,
            ZoeError::Undo { reason, .. } => {
                *reason == undo::NO_TRANSACTION || *reason == undo::BEFORE_PACKED
            }
            _ => false,
        };
        match self.store.pack_watermark() {
            Ok(watermark) if pack_can_explain && tid <= watermark => refused(None, undo::PACKED),
            _ => error,
        }
    }

    /// Stage the undo of `oid`, whose record in the transaction `tid` is
    /// `undone`. Return the conflict when no tier resolves it.
    fn undo_oid(&mut self, oid: Oid, tid: Tid, undone: &[u8]) -> Result<Option<UndoConflict>> {
        let before = self.before(oid, tid)?;
        // The serial of the current state, its pickle and its blob record.
        let (serial, current, blob) = match self.staged.get(oid)? {
            Some((serial, pickle)) => {
                let blob = self.blobs.get(&oid).map(|blob| *blob.record());
                (serial, pickle.into_owned(), blob)
            }
            None => match self.store.backend.load_before(oid, Tid::MAX)? {
                Some(record) => {
                    let blob = self.store.backend.load_blob(oid, record.tid)?;
                    (record.tid, record.data, blob)
                }
                // The transaction `tid` wrote the oid, so it has a record.
                None => return Err(ZoeError::CorruptTxn { tid }),
            },
        };
        if current == undone {
            // The revisions of a blob have the same pickle, so the blob
            // records tell if a later transaction changed the blob.
            let own = self.staged.record(oid).is_none() && serial == tid;
            if !own && blob != self.store.backend.load_blob(oid, tid)? {
                return Err(refused(Some(oid), undo::CHANGED_LATER));
            }
            self.stage_before(oid, serial, before.as_ref())?;
            return Ok(None);
        }
        let before = match before {
            Some(record) if !record.data.is_empty() => record,
            Some(_) => return Err(refused(Some(oid), undo::CHANGED_LATER)),
            None => return Err(refused(Some(oid), undo::CREATION_CHANGED)),
        };
        if undone.is_empty()
            || current.is_empty()
            || self.store.backend.load_blob(oid, tid)?.is_some()
            || self.store.backend.load_blob(oid, before.tid)?.is_some()
        {
            return Err(refused(Some(oid), undo::CHANGED_LATER));
        }
        if self.merge_undo(oid, tid, serial, undone, &current, &before.data)? {
            return Ok(None);
        }
        Ok(Some(UndoConflict {
            oid,
            serial,
            current,
            undone: tid,
            before: before.data,
        }))
    }

    /// Return the record of `oid` before its record in the transaction
    /// `tid`, or `None` when `tid` created the object. A record before
    /// that a pack removed fails with `Undo`.
    fn before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>> {
        let backend = &self.store.backend;
        let prev = match backend.load_before(oid, Tid(tid.0 + 1))? {
            Some(record) if record.tid == tid => record.prev,
            _ => return Err(ZoeError::CorruptTxn { tid }),
        };
        if prev == Tid::ZERO {
            return Ok(None);
        }
        match backend.load_before(oid, tid)? {
            Some(record) if record.tid == prev => Ok(Some(record)),
            _ => Err(refused(Some(oid), undo::BEFORE_PACKED)),
        }
    }

    /// Stage the state `before` of `oid` over `serial`, with its blob, or a
    /// deletion when `before` is `None` or a deletion.
    fn stage_before(&mut self, oid: Oid, serial: Tid, before: Option<&Record>) -> Result<()> {
        let (data, blob) = match before {
            Some(record) if !record.data.is_empty() => {
                let blob = match self.store.backend.load_blob(oid, record.tid)? {
                    Some(blob) => Some(self.store.blobs().link(
                        Arc::downgrade(&self.store.backend),
                        oid,
                        record.tid,
                        blob,
                    )?),
                    None => None,
                };
                (&record.data[..], blob)
            }
            _ => (&[][..], None),
        };
        let stored_bytes = self.check_size(oid, data)?;
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, false)?;
        self.stored_bytes = stored_bytes;
        match blob {
            Some(blob) => {
                self.blobs.insert(oid, blob);
            }
            None => {
                self.blobs.remove(&oid);
            }
        }
        Ok(())
    }

    /// Run the tiers on the undo of `oid` in the transaction `tid`, as the
    /// vote runs them on a conflict: `undone` is the base, `current` the
    /// committed state at `serial`, and `before` the new state. Tier 1
    /// merges a class of BTrees or `Length`, and tier 2 a class that the
    /// merge policy names. Stage the merged record over `serial` and return
    /// true when a tier merges in mode `on`. Return false when it gives no
    /// answer, or runs in mode `shadow`, and fail with `Undo` when it
    /// refuses.
    ///
    /// The entry of the merge goes to `resolutions`, before the entries of
    /// the vote. The merge policy is read only for an undo that merges.
    fn merge_undo(
        &mut self,
        oid: Oid,
        tid: Tid,
        serial: Tid,
        undone: &[u8],
        current: &[u8],
        before: &[u8],
    ) -> Result<bool> {
        let policy = self.store.merge_policy()?;
        let rules = Rules {
            config: &self.store.structural_merges,
            policy: policy.as_deref(),
        };
        let Some((mode, merged)) = undo_tiers(undone, current, before, &rules) else {
            return Ok(false);
        };
        let mut resolution = Resolution::of(oid, mode, rules.version(), &merged, serial, tid);
        resolution.undo = true;
        let staged = match (mode, merged.outcome) {
            (merge::Mode::On, Outcome::Merged(pickle)) => match self.check_size(oid, &pickle) {
                Ok(stored_bytes) => {
                    let area = &self.store.backend.staging;
                    self.staged.put(area, oid, serial, &pickle, false)?;
                    self.stored_bytes = stored_bytes;
                    resolution.applied = true;
                    true
                }
                Err(_) => {
                    resolution.outcome = Resolved::Undecided("a merged record over a size limit");
                    false
                }
            },
            (merge::Mode::On, Outcome::Refused(_)) => {
                resolution.applied = true;
                self.push_undo_resolution(resolution);
                return Err(refused(Some(oid), undo::CHANGED_LATER));
            }
            _ => false,
        };
        self.push_undo_resolution(resolution);
        Ok(staged)
    }

    /// Add the entry of a merge of an undo to `resolutions`. The entries of
    /// the undos come first, and the vote keeps them.
    fn push_undo_resolution(&mut self, resolution: Resolution) {
        self.resolutions.insert(self.undo_resolutions, resolution);
        self.undo_resolutions += 1;
    }
}

/// Return the error of an undo that `reason` refuses.
fn refused(oid: Option<Oid>, reason: &'static str) -> ZoeError {
    ZoeError::Undo { oid, reason }
}
