//! The reading of the log at an open (ADR 0056, sections 11 and 14).
//!
//! The open reads the header of each segment, then replays each segment in
//! use in epoch order, one entry after the other. A passing header gives
//! the length of its entry, so the replay goes to the next entry by that
//! length, never by a scan. It reads the body of an entry only when the
//! entry holds a commit after the folded tid F.
//!
//! The leader writes an entry only after the sync of the entry before it,
//! and starts a new use only after the sync of the last entry of the use
//! before. So only the last entry of the log can be cut by a crash, and a
//! valid entry after a damaged one proves the damage: the open scans for
//! such evidence, and never replays an entry that a scan found. An I/O
//! error on a read of the log fails the open: it is never a torn tail.

use std::io;

use redb::StorageBackend;

use crate::error::{Result, ZoeError};
use crate::format::wal::{
    self, ALIGN, Commit, ENTRY_HEADER_LEN, ENTRY_MAGIC, EntryHeader, SEGMENT_HEADER_LEN,
    SegmentHeader, SegmentRead, SegmentState,
};
use crate::types::Tid;

use super::table::LogEntry;

/// A segment file that the open reads with positional reads.
pub(super) trait LogFile {
    fn len(&self) -> io::Result<u64>;
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()>;
}

impl LogFile for Box<dyn StorageBackend> {
    fn len(&self) -> io::Result<u64> {
        StorageBackend::len(&**self)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        StorageBackend::read(&**self, offset, out)
    }
}

impl LogFile for std::fs::File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, out, offset)
    }
}

/// What the open found in one segment file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A header that passes and says "spare", with its epoch.
    Spare(u64),
    /// A segment in use, with the epoch of its use.
    InUse {
        epoch: u64,
        /// Its header failed: the epoch comes from its first entry, and the
        /// file is never used again.
        damaged: bool,
        /// The last tid of its entries that the open read.
        last: Tid,
        /// It holds a commit after the folded tid.
        unfolded: bool,
    },
    /// A header that failed with no entry at byte 32 that passes, and no
    /// commit after the folded tid.
    Damaged,
}

/// One segment file of the open.
pub(super) struct Found<F> {
    pub(super) number: u64,
    pub(super) file: F,
    pub(super) len: u64,
    pub(super) kind: Kind,
}

/// What the replay of the log gives.
pub(super) struct Replayed<F> {
    pub(super) files: Vec<Found<F>>,
    /// The entries with commits after the folded tid, in commit order, with
    /// those commits. Empty when the replay keeps no entry.
    pub(super) entries: Vec<(LogEntry, Vec<Commit>)>,
    /// The previous tid and the last tid of an entry that the open cut
    /// while its header passed and its last tid is after the folded tid.
    pub(super) cut: Option<(Tid, Tid)>,
    /// The greatest epoch that the open read.
    pub(super) max_epoch: u64,
    /// The last tid of the replay, at least the folded tid.
    pub(super) last: Tid,
    /// The log holds a commit after the folded tid.
    pub(super) unfolded: bool,
}

/// Read the log of `files`, the segment files of `wal/` by number, after
/// the folded tid `folded`. With `keep`, keep the entries with commits
/// after it.
pub(super) fn replay<F: LogFile>(
    files: Vec<(u64, F)>,
    folded: Tid,
    keep: bool,
) -> Result<Replayed<F>> {
    let mut max_epoch = 0;
    let mut found = Vec::with_capacity(files.len());
    for (number, file) in files {
        let len = file.len()?;
        let mut head = [0; SEGMENT_HEADER_LEN as usize];
        let read = if len >= SEGMENT_HEADER_LEN {
            file.read(0, &mut head)?;
            wal::decode_segment_header(&head)
        } else {
            SegmentRead::Damaged
        };
        let kind = match read {
            SegmentRead::Unreadable(text) => return Err(ZoeError::Format(text)),
            SegmentRead::Header(SegmentHeader { state, epoch }) => {
                max_epoch = max_epoch.max(epoch);
                match state {
                    SegmentState::Spare => Kind::Spare(epoch),
                    SegmentState::InUse => Kind::InUse {
                        epoch,
                        damaged: false,
                        last: Tid::ZERO,
                        unfolded: false,
                    },
                }
            }
            SegmentRead::Damaged => damaged(&file, len, folded, &mut max_epoch)?,
        };
        found.push(Found {
            number,
            file,
            len,
            kind,
        });
    }

    let mut order: Vec<usize> = (0..found.len())
        .filter(|&i| matches!(found[i].kind, Kind::InUse { .. }))
        .collect();
    order.sort_by_key(|&i| match found[i].kind {
        Kind::InUse { epoch, .. } => (epoch, found[i].number),
        _ => unreachable!(),
    });
    let mut chain = Chain {
        folded,
        expected: None,
        entries: Vec::new(),
        cut: None,
        keep,
    };
    for i in order {
        let Found {
            file, len, kind, ..
        } = &mut found[i];
        let Kind::InUse {
            epoch,
            last,
            unfolded,
            ..
        } = kind
        else {
            unreachable!()
        };
        (*last, *unfolded) = chain.segment(file, *len, *epoch, &mut max_epoch)?;
    }
    let last = chain.expected.unwrap_or(folded).max(folded);
    Ok(Replayed {
        files: found,
        unfolded: chain.expected.is_some(),
        entries: chain.entries,
        cut: chain.cut,
        max_epoch,
        last,
    })
}

/// Read a segment whose header failed. When the entry at byte 32 passes
/// both checksums, the segment is in use with the epoch of that entry.
/// Else scan the file for evidence with any epoch: an entry found there
/// with a commit after `folded` fails the open with `CorruptTxn`.
fn damaged<F: LogFile>(file: &F, len: u64, folded: Tid, max_epoch: &mut u64) -> Result<Kind> {
    let first = read_header(file, SEGMENT_HEADER_LEN, len)?;
    if let Some(header) = first {
        *max_epoch = (*max_epoch).max(header.epoch);
        if body_passes(file, SEGMENT_HEADER_LEN, len, &header)? {
            return Ok(Kind::InUse {
                epoch: header.epoch,
                damaged: true,
                last: Tid::ZERO,
                unfolded: false,
            });
        }
    }
    let from = match first.and_then(|header| header.len()) {
        Some(entry) => SEGMENT_HEADER_LEN.saturating_add(entry),
        None => SEGMENT_HEADER_LEN + ALIGN,
    };
    if let Some(evidence) = scan(file, len, from, None, folded, max_epoch)? {
        let tid = match first {
            Some(header) => header.last,
            None => cited(&evidence, folded),
        };
        return Err(ZoeError::CorruptTxn { tid });
    }
    Ok(Kind::Damaged)
}

/// The tid that a `CorruptTxn` names for evidence found after an entry
/// whose header failed: the previous tid of the entry found when it is
/// after the folded tid, else its last tid.
fn cited(evidence: &EntryHeader, folded: Tid) -> Tid {
    if evidence.previous > folded {
        evidence.previous
    } else {
        evidence.last
    }
}

/// The chain of the entries after the folded tid, across the segments.
struct Chain {
    folded: Tid,
    /// The last tid of the entry before, once an entry after the folded tid
    /// was read.
    expected: Option<Tid>,
    entries: Vec<(LogEntry, Vec<Commit>)>,
    cut: Option<(Tid, Tid)>,
    keep: bool,
}

impl Chain {
    /// Replay the segment `file` of `len` bytes, in use with `epoch`, from
    /// byte 32. Return the last tid of its entries and if it holds a commit
    /// after the folded tid.
    fn segment<F: LogFile>(
        &mut self,
        file: &F,
        len: u64,
        epoch: u64,
        max_epoch: &mut u64,
    ) -> Result<(Tid, bool)> {
        let folded = self.folded;
        let (mut last, mut unfolded) = (Tid::ZERO, false);
        let mut offset = SEGMENT_HEADER_LEN;
        loop {
            if offset.saturating_add(ENTRY_HEADER_LEN as u64) > len {
                return Ok((last, unfolded));
            }
            let header = read_header(file, offset, len)?;
            if let Some(header) = &header {
                *max_epoch = (*max_epoch).max(header.epoch);
            }
            let Some(header) = header.filter(|header| header.epoch == epoch) else {
                // A header that fails, or of another use: the segment ends
                // here unless a later entry of the use proves a damage.
                let from = offset + ALIGN;
                if let Some(evidence) = scan(file, len, from, Some(epoch), folded, max_epoch)? {
                    let tid = cited(&evidence, folded);
                    return Err(ZoeError::CorruptTxn { tid });
                }
                return Ok((last, unfolded));
            };
            let end = header.len().and_then(|entry| offset.checked_add(entry));
            if header.last <= folded {
                // A folded entry: no body is read.
                if self.expected.is_some() {
                    return Err(ZoeError::CorruptTxn {
                        tid: header.previous,
                    });
                }
                last = last.max(header.last);
                match end {
                    Some(end) => offset = end,
                    None => return Ok((last, unfolded)),
                }
                continue;
            }
            let whole = end.is_some_and(|end| end <= len);
            let bytes = if whole {
                let mut bytes = vec![0; (end.expect("an end") - offset) as usize];
                file.read(offset, &mut bytes)?;
                let body = &bytes[ENTRY_HEADER_LEN..ENTRY_HEADER_LEN + header.body_len as usize];
                header
                    .body_passes(&bytes[..ENTRY_HEADER_LEN], body)
                    .then_some(bytes)
            } else {
                None
            };
            let Some(bytes) = bytes else {
                // The entry is cut: by the end of the file, or a body that
                // fails. A later entry of the use proves a damage.
                let from = end.unwrap_or(u64::MAX);
                if scan(file, len, from, Some(epoch), folded, max_epoch)?.is_some() {
                    return Err(ZoeError::CorruptTxn { tid: header.last });
                }
                self.cut = Some((header.previous, header.last));
                return Ok((last, unfolded));
            };
            let (ops, commits) = wal::parse(&bytes, &header).map_err(|rule| {
                ZoeError::Format(format!(
                    "an entry of the write-ahead log breaks a rule of its format: {rule}"
                ))
            })?;
            let follows = match self.expected {
                Some(expected) => header.previous == expected,
                None => {
                    header.previous == folded || commits.iter().any(|commit| commit.tid == folded)
                }
            };
            if !follows {
                return Err(ZoeError::CorruptTxn {
                    tid: header.previous,
                });
            }
            self.expected = Some(header.last);
            last = header.last;
            unfolded = true;
            if self.keep {
                let after: Vec<Commit> = commits
                    .into_iter()
                    .filter(|commit| commit.tid > folded)
                    .collect();
                self.entries.push((LogEntry { bytes, ops }, after));
            }
            offset = end.expect("an end");
        }
    }
}

/// Read the header of the entry at `offset` of `file`. Return `None` when
/// it does not pass, or when the file ends before it.
fn read_header<F: LogFile>(file: &F, offset: u64, len: u64) -> io::Result<Option<EntryHeader>> {
    if offset.saturating_add(ENTRY_HEADER_LEN as u64) > len {
        return Ok(None);
    }
    let mut bytes = [0; ENTRY_HEADER_LEN];
    file.read(offset, &mut bytes)?;
    Ok(EntryHeader::decode(&bytes))
}

/// Tell if the body of the entry at `offset` of `file`, whose header is
/// `header`, is in the file and passes its checksum.
fn body_passes<F: LogFile>(
    file: &F,
    offset: u64,
    len: u64,
    header: &EntryHeader,
) -> io::Result<bool> {
    let Some(end) = offset
        .checked_add(ENTRY_HEADER_LEN as u64)
        .and_then(|at| at.checked_add(header.body_len))
        .filter(|&end| end <= len)
    else {
        return Ok(false);
    };
    let mut bytes = vec![0; (end - offset) as usize];
    file.read(offset, &mut bytes)?;
    let (head, body) = bytes.split_at(ENTRY_HEADER_LEN);
    Ok(header.body_passes(head, body))
}

/// The bytes that one read of a scan takes.
const SCAN_CHUNK: usize = 1 << 20;

/// Scan `file` from `from` to its end, at each multiple of 8, for an entry
/// whose header and body pass, of `epoch` when it is given, with a commit
/// after `folded`. Return the header of the first one.
fn scan<F: LogFile>(
    file: &F,
    len: u64,
    from: u64,
    epoch: Option<u64>,
    folded: Tid,
    max_epoch: &mut u64,
) -> io::Result<Option<EntryHeader>> {
    let Some(mut at) = from.checked_next_multiple_of(ALIGN) else {
        return Ok(None);
    };
    let mut chunk = Vec::new();
    while at.saturating_add(ENTRY_HEADER_LEN as u64) <= len {
        // Each read overlaps the next by a header, so a header is whole in
        // the chunk where it starts.
        let n = (len - at).min((SCAN_CHUNK + ENTRY_HEADER_LEN) as u64) as usize;
        chunk.resize(n, 0);
        file.read(at, &mut chunk)?;
        let mut i = 0;
        while i + ENTRY_HEADER_LEN <= n && i < SCAN_CHUNK {
            if chunk[i..i + 4] == ENTRY_MAGIC
                && let Some(header) = EntryHeader::decode(&chunk[i..])
            {
                *max_epoch = (*max_epoch).max(header.epoch);
                let offset = at + i as u64;
                if epoch.is_none_or(|epoch| header.epoch == epoch)
                    && header.last > folded
                    && body_passes(file, offset, len, &header)?
                {
                    return Ok(Some(header));
                }
            }
            i += ALIGN as usize;
        }
        at += i as u64;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::wal::{EntryBuilder, OpKind, encode_segment_header};
    use crate::types::Oid;

    /// A segment file in memory.
    struct Bytes(Vec<u8>);

    impl LogFile for Bytes {
        fn len(&self) -> io::Result<u64> {
            Ok(self.0.len() as u64)
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            let at = offset as usize;
            out.copy_from_slice(&self.0[at..at + out.len()]);
            Ok(())
        }
    }

    /// Return the sealed entry of one commit `tid` of epoch `epoch` after
    /// `previous`.
    fn entry(epoch: u64, previous: Tid, tid: Tid) -> Vec<u8> {
        let mut builder = EntryBuilder::new(0);
        builder
            .txn(tid, |out| out.extend_from_slice(b"row"))
            .unwrap();
        builder
            .record(OpKind::Record, Oid(1), tid, |out| {
                out.extend_from_slice(b"data")
            })
            .unwrap();
        let mut entry = builder.finish();
        entry.seal(epoch, previous);
        entry.bytes
    }

    /// Return a segment in use with `epoch` and the entries of the tids
    /// `tids`, chained from `Tid::ZERO`, with `tail` zeros after them.
    fn segment(epoch: u64, tids: &[u64], tail: usize) -> (Vec<u8>, Vec<u64>) {
        let header = SegmentHeader {
            state: SegmentState::InUse,
            epoch,
        };
        let mut bytes = encode_segment_header(header).to_vec();
        let mut offsets = Vec::new();
        let mut previous = Tid::ZERO;
        for &tid in tids {
            offsets.push(bytes.len() as u64);
            bytes.extend(entry(epoch, previous, Tid(tid)));
            previous = Tid(tid);
        }
        bytes.resize(bytes.len() + tail, 0);
        (bytes, offsets)
    }

    fn run(bytes: Vec<u8>, folded: u64) -> Result<Replayed<Bytes>> {
        replay(vec![(1, Bytes(bytes))], Tid(folded), true)
    }

    #[test]
    fn the_replay_keeps_the_commits_after_the_folded_tid() {
        let (bytes, _) = segment(3, &[10, 20, 30], 64);
        let replayed = run(bytes, 10).unwrap();
        assert_eq!(replayed.entries.len(), 2);
        assert_eq!(replayed.last, Tid(30));
        assert_eq!(replayed.max_epoch, 3);
        assert_eq!(replayed.cut, None);
        assert!(replayed.unfolded);
    }

    #[test]
    fn a_damaged_last_entry_is_cut_and_a_damaged_middle_entry_fails() {
        let (bytes, offsets) = segment(3, &[10, 20, 30], 64);
        let mut last = bytes.clone();
        last[offsets[2] as usize + ENTRY_HEADER_LEN + 2] ^= 1;
        let replayed = run(last, 0).unwrap();
        assert_eq!(replayed.cut, Some((Tid(20), Tid(30))));
        assert_eq!(replayed.last, Tid(20));

        let mut middle = bytes;
        middle[offsets[1] as usize + ENTRY_HEADER_LEN + 2] ^= 1;
        assert!(matches!(
            run(middle, 0),
            Err(ZoeError::CorruptTxn { tid: Tid(20) })
        ));
    }

    #[test]
    fn a_damaged_header_is_read_from_its_first_entry() {
        let (mut bytes, _) = segment(7, &[10, 20], 64);
        bytes[20] ^= 1;
        let replayed = run(bytes, 0).unwrap();
        assert_eq!(replayed.last, Tid(20));
        assert!(matches!(
            replayed.files[0].kind,
            Kind::InUse {
                epoch: 7,
                damaged: true,
                ..
            }
        ));
    }

    #[test]
    fn a_broken_chain_fails_the_open() {
        // The entry names a previous tid that the log does not hold.
        let header = SegmentHeader {
            state: SegmentState::InUse,
            epoch: 1,
        };
        let mut bytes = encode_segment_header(header).to_vec();
        bytes.extend(entry(1, Tid(15), Tid(20)));
        assert!(matches!(
            run(bytes.clone(), 10),
            Err(ZoeError::CorruptTxn { tid: Tid(15) })
        ));
        assert!(!run(bytes, 15).unwrap().entries.is_empty());
    }

    #[test]
    fn a_spare_holds_no_entry_that_counts() {
        let (mut bytes, _) = segment(4, &[10], 0);
        let spare = SegmentHeader {
            state: SegmentState::Spare,
            epoch: 4,
        };
        bytes[..32].copy_from_slice(&encode_segment_header(spare));
        let replayed = run(bytes, 0).unwrap();
        assert!(!replayed.unfolded);
        assert_eq!(replayed.files[0].kind, Kind::Spare(4));
    }
}
