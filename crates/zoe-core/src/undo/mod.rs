//! Undo: a transaction that stores, for each object of an earlier
//! transaction T, the state before T.
//!
//! `Txn::undo` stages the undo of T in a transaction of its own. For each
//! oid of T, it reads the state before T, the state of T and the current
//! state, as the transaction sees it: the record that it staged for the
//! oid, or else the committed record.
//!
//! - When the current state is the state of T, the undo stages the state
//!   before T over the current serial. When T created the object, it
//!   stages a deletion. A blob gets the blob file of the state before T,
//!   by a hard link, or a copy in a new file where the filesystem cannot
//!   link: no blob file is written again. When the current blob is not the
//!   blob of T, a later transaction changed it, and the undo fails.
//! - Else the undo is a three-way merge, as in ZODB: the state of T is the
//!   base, the current state is the committed one, and the state before T
//!   is the new one. Tier 1 merges a class of BTrees or `Length` in Rust.
//!   A refusal of tier 1 fails the undo with `Undo`. When no tier gives an
//!   answer, the undo gives the oid to its caller as an [`UndoConflict`]:
//!   the in-process storage resolves it in Python, and `zoe serve`, which
//!   runs no Python, fails the undo.
//!
//! The undo takes no lock. The vote locks its oids as for `store`, and
//! checks each serial: a transaction that changed an oid after the undo
//! read it is a conflict of the vote, as for any store.
//!
//! Nothing is undoable at or before the last pack (`pack_watermark`). The
//! finish checks it again under the commit sequencer, so a pack that runs
//! between the undo and its finish fails the undo. The undo log,
//! `Store::undo_log`, gives the transactions after it, newest first. An
//! undo id is the base64 text of the tid, as in FileStorage.

use crate::backend::TxnMeta;
use crate::error::Result;
use crate::store::Store;
use crate::types::{Oid, Tid};

/// What `Txn::undo` staged for an undone transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Undone {
    /// The oids of the undone transaction, in ascending order. The commit
    /// of the undo writes each of them.
    pub oids: Vec<Oid>,
    /// The oids whose undo no tier resolves. The caller resolves each one
    /// and stores the result over `serial`, or fails the undo.
    pub conflicts: Vec<UndoConflict>,
}

/// An oid of an undo whose three-way merge no tier resolves.
///
/// ZODB's resolver takes it as `tryToResolveConflict(oid, serial, undone,
/// before, current)`: the state of the undone transaction is the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoConflict {
    /// The oid.
    pub oid: Oid,
    /// The serial of the current state. A resolved pickle is stored over
    /// it.
    pub serial: Tid,
    /// The current state, as the transaction sees it.
    pub current: Vec<u8>,
    /// The tid of the undone transaction.
    pub undone: Tid,
    /// The state before the undone transaction.
    pub before: Vec<u8>,
}

/// The reason of an undo of a transaction at or before the last pack.
pub(crate) const PACKED: &str = "the transaction is at or before the last pack";

/// The reason of an undo of an object whose state before the undone
/// transaction a pack removed: no reader could reach the object.
pub(crate) const BEFORE_PACKED: &str = "a pack removed the state before the transaction";

/// The reason of an undo of a transaction that the store does not have.
pub(crate) const NO_TRANSACTION: &str = "the store has no such transaction";

/// The reason of an undo of the creation of an object that a later
/// transaction changed. FileStorage gives the same text.
pub(crate) const CREATION_CHANGED: &str =
    "Can't undo an add transaction followed by conflicting transactions.";

/// The reason of an undo that a later transaction stops. FileStorage
/// gives the same text.
pub(crate) const CHANGED_LATER: &str = "Some data were modified by a later transaction";

impl Store {
    /// Return the rows of at most `limit` transactions before `before`
    /// that an undo can undo, newest first: the transactions after the
    /// last pack. `Tid::MAX` starts at the last transaction.
    ///
    /// The rows of one call come from one snapshot, and the watermark from
    /// another read. A caller that reads the log in parts gives the tid of
    /// the last row of a part as `before` of the next.
    pub fn undo_log(&self, before: Tid, limit: usize) -> Result<Vec<TxnMeta>> {
        let watermark = self.pack_watermark()?;
        let mut rows = self.backend.txns_before(before, limit)?;
        rows.retain(|row| row.tid > watermark);
        Ok(rows)
    }
}

/// Return the undo id of the transaction `tid`: the base64 text of its 8
/// bytes, as `undoLog` of FileStorage gives it.
pub fn undo_id(tid: Tid) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = tid.0.to_be_bytes();
    let mut id = Vec::with_capacity(12);
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        let symbols = chunk.len() + 1;
        for i in 0..4 {
            if i < symbols {
                id.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]);
            } else {
                id.push(b'=');
            }
        }
    }
    id
}

/// Return the tid of the undo id `id`: the base64 text of `undo_id`, with
/// or without ASCII white space around it, or the 8 bytes of the tid.
/// Return `None` for any other id.
pub fn undo_tid(id: &[u8]) -> Option<Tid> {
    if let Ok(bytes) = <[u8; 8]>::try_from(id) {
        return Some(Tid(u64::from_be_bytes(bytes)));
    }
    let text = id.trim_ascii();
    if text.len() != 12 || &text[11..] != b"=" {
        return None;
    }
    let mut n: u128 = 0;
    for &symbol in &text[..11] {
        let value = match symbol {
            b'A'..=b'Z' => symbol - b'A',
            b'a'..=b'z' => symbol - b'a' + 26,
            b'0'..=b'9' => symbol - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        n = n << 6 | u128::from(value);
    }
    // 11 symbols hold 66 bits: the 64 bits of the tid, then 2 zero bits.
    if n & 3 != 0 {
        return None;
    }
    Some(Tid((n >> 2) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undo_id_is_the_base64_text_of_the_tid() {
        // base64.encodebytes(bytes.fromhex("03e4b1c2d5f60718")).rstrip()
        let tid = Tid(0x03e4_b1c2_d5f6_0718);
        assert_eq!(undo_id(tid), b"A+SxwtX2Bxg=");
        assert_eq!(undo_tid(b"A+SxwtX2Bxg="), Some(tid));
        assert_eq!(undo_tid(b"A+SxwtX2Bxg=\n"), Some(tid));
        for tid in [
            Tid::ZERO,
            Tid(1),
            Tid(u64::MAX - 1),
            Tid(0x0102_0304_0506_0708),
        ] {
            assert_eq!(undo_tid(&undo_id(tid)), Some(tid));
        }
    }

    #[test]
    fn an_undo_id_can_be_the_bytes_of_the_tid() {
        assert_eq!(undo_tid(&5u64.to_be_bytes()), Some(Tid(5)));
    }

    #[test]
    fn another_id_names_no_transaction() {
        for id in [
            &b""[..],
            b"A+SxwtX2Bx",
            b"A+SxwtX2Bxg",
            b"A+SxwtX2Bx*=",
            b"A+SxwtX2Bxh=",
        ] {
            assert_eq!(undo_tid(id), None, "{id:?}");
        }
    }
}
