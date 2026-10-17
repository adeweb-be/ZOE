//! `Txn::undo`: the undo of an earlier transaction, staged in this one.
//! `crate::undo` describes it.

use std::ops::Deref;
use std::sync::Arc;

use crate::backend::Record;
use crate::error::{Result, ZoeError};
use crate::merge::{self, Outcome, Resolution, Resolved};
use crate::store::Store;
use crate::types::{Oid, Tid};
use crate::undo::{self, UndoConflict, Undone};

use super::Txn;

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
    ///   of the state before `tid` by a hard link, so no blob file is
    ///   copied or written again.
    /// - Else tier 1 merges the three states, with the state of `tid` as
    ///   the base, the current state as the committed one and the state
    ///   before `tid` as the new one. The call stages the merged record
    ///   over the serial of the current state. A refusal of tier 1 fails
    ///   the call with `Undo`. When no tier gives an answer, the oid goes
    ///   into `conflicts`, and the call stages nothing for it: the caller
    ///   resolves it and stores the result over its serial, or fails.
    /// - An undo of the creation of an object, of a deletion, or of a blob,
    ///   that a later transaction changed fails with `Undo`.
    ///
    /// A transaction at or before the last pack, one that the store does
    /// not have, or one whose state before is gone by a pack, fails the
    /// call with `Undo`. A failed call can leave
    /// the undo of the oids before the one that failed it staged, so the
    /// caller then aborts the transaction.
    ///
    /// The call takes no lock. The vote locks the staged oids and checks
    /// their serials as for `store`. The pickles count toward the size
    /// limits, as for `store`. A restore transaction refuses the call with
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
        if tid <= self.store.pack_watermark()? {
            return Err(refused(None, undo::PACKED));
        }
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

    /// Stage the undo of `oid`, whose record in the transaction `tid` is
    /// `undone`. Return the conflict when no tier resolves it.
    fn undo_oid(&mut self, oid: Oid, tid: Tid, undone: &[u8]) -> Result<Option<UndoConflict>> {
        let before = self.before(oid, tid)?;
        let (serial, current) = match self.staged.get(oid)? {
            Some((serial, pickle)) => (serial, pickle.into_owned()),
            None => match self.store.backend.load_before(oid, Tid::MAX)? {
                Some(record) => (record.tid, record.data),
                // The transaction `tid` wrote the oid, so it has a record.
                None => return Err(ZoeError::CorruptTxn { tid }),
            },
        };
        if current == undone {
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

    /// Run tier 1 on the undo of `oid` in the transaction `tid`: `undone`
    /// is the base, `current` the committed state at `serial`, and `before`
    /// the new state. Stage the merged record over `serial` and return
    /// true when tier 1 merges in mode `on`. Return false when it gives no
    /// answer, and fail with `Undo` when it refuses.
    ///
    /// The entry of the merge goes to `resolutions`, before the entries of
    /// the vote.
    fn merge_undo(
        &mut self,
        oid: Oid,
        tid: Tid,
        serial: Tid,
        undone: &[u8],
        current: &[u8],
        before: &[u8],
    ) -> Result<bool> {
        let rules = &self.store.structural_merges;
        let Some(class) = merge::tier1_class(before) else {
            return Ok(false);
        };
        let mode = rules.mode(&class);
        if mode == merge::Mode::Off {
            return Ok(false);
        }
        let merged =
            merge::tier1(undone, current, before, rules.posting_dicts).expect("a class of tier 1");
        let mut resolution = Resolution::of(oid, mode, &merged, serial, tid);
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
