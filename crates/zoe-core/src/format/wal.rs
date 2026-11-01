//! The format of the write-ahead log (ADR 0056). This module is a red zone.
//!
//! The log is the directory `wal/` of the store. A segment is a file
//! `wal/<n>.wal`, where `<n>` is 16 lower-case hexadecimal digits. A new
//! segment is prepared as `wal/<n>.new` and renamed when its header is
//! durable. Integers are little-endian.
//!
//! A segment starts with a header of 32 bytes:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0–7 | magic `ZOE-WAL\0` |
//! | 8–11 | version, `u32`: 1 |
//! | 12–15 | state, `u32`: 1 in use, 2 spare |
//! | 16–23 | epoch, `u64` |
//! | 24–31 | xxh3-64, seed 0, of bytes 0 to 23 |
//!
//! A segment in use holds its entries from byte 32. One entry is one commit
//! batch. It starts at a multiple of 8, with a header of 56 bytes:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0–3 | magic `ZWAL` |
//! | 4–7 | op count, `u32` |
//! | 8–15 | body length, `u64` |
//! | 16–23 | epoch, `u64`: the epoch of the use |
//! | 24–31 | previous tid: the last tid of the store before the batch |
//! | 32–39 | last tid: the tid of the last commit of the batch |
//! | 40–47 | body checksum: xxh3-64, seed 0, of bytes 0 to 39 and the body |
//! | 48–55 | header checksum: xxh3-64, seed 0, of bytes 0 to 47 |
//!
//! The body follows, then zeros up to the next multiple of 8. The body is
//! a list of ops, each a kind byte and its fields:
//!
//! | Kind | Op | Fields |
//! | --- | --- | --- |
//! | 1 | transaction row | tid `u64`, length `u32`, the frame of `txns` |
//! | 2 | record | oid `u64`, length `u32`, the frame of `records` |
//! | 3 | blob record | oid `u64`, length `u32`, the frame of `blobs` |
//! | 4 | ephemeral row | key length `u16`, key, length `u32`, the frame |
//! | 5 | durable row | key length `u16`, key, length `u32`, the frame |
//! | 6 | removal of an ephemeral row | key length `u16`, key |
//! | 7 | removal of a durable row | key length `u16`, key |
//! | 8 | expiry key | key length `u16`, the key of `kv_expiry` |
//!
//! Each commit of the batch starts with its transaction row. Each op after
//! it belongs to that commit: a record and a blob record have the tid of
//! the row. The transaction rows come in tid order: the first tid is after
//! the previous tid, and the last one is the last tid of the header. A
//! frame is the value that the tables store under its key, byte for byte.

use std::ops::Range;

use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::kv::Flavour;
use crate::types::{Oid, Tid};

/// The directory of the log in the store directory.
pub(crate) const DIR: &str = "wal";

/// The length of the header of a segment, and the offset of the first
/// entry of a use.
pub(crate) const SEGMENT_HEADER_LEN: u64 = 32;

/// The length of the header of an entry.
pub(crate) const ENTRY_HEADER_LEN: usize = 56;

/// The alignment of an entry.
pub(crate) const ALIGN: u64 = 8;

/// The magic number of a segment.
const SEGMENT_MAGIC: [u8; 8] = *b"ZOE-WAL\0";

/// The magic number of an entry.
pub(crate) const ENTRY_MAGIC: [u8; 4] = *b"ZWAL";

/// The version of the segment format that this code writes and reads.
pub(crate) const VERSION: u32 = 1;

/// The state of a segment whose header starts a use.
const IN_USE: u32 = 1;

/// The state of a segment that holds no entry that counts.
const SPARE: u32 = 2;

/// Return the name of the segment file `number`.
pub(crate) fn segment_name(number: u64) -> String {
    format!("{DIR}/{number:016x}.wal")
}

/// Return the name under which the segment file `number` is prepared.
pub(crate) fn new_name(number: u64) -> String {
    format!("{DIR}/{number:016x}.new")
}

/// The kind of a file name of `wal/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileName {
    /// A segment, `<n>.wal`.
    Segment(u64),
    /// A segment in preparation, `<n>.new`.
    New(u64),
}

/// Read a file name of `wal/`, without the directory part. Any name but 16
/// lower-case hexadecimal digits and `.wal` or `.new` gives `None`.
pub(crate) fn parse_name(name: &str) -> Option<FileName> {
    let (digits, suffix) = name.split_at_checked(16)?;
    let lower = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    if !digits.chars().all(lower) {
        return None;
    }
    let number = u64::from_str_radix(digits, 16).ok()?;
    match suffix {
        ".wal" => Some(FileName::Segment(number)),
        ".new" => Some(FileName::New(number)),
        _ => None,
    }
}

/// The state of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentState {
    /// The header starts a use: the entries of its epoch count.
    InUse,
    /// The segment holds no entry that counts.
    Spare,
}

/// The header of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    pub(crate) state: SegmentState,
    pub(crate) epoch: u64,
}

/// What the first 32 bytes of a segment file say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SegmentRead {
    /// A header that passes its checksum.
    Header(SegmentHeader),
    /// The bytes do not pass as a header: the header is damaged, or the
    /// file is shorter than a header.
    Damaged,
    /// A header that passes its checksum with a version or a state that
    /// this build cannot read. The text tells which.
    Unreadable(String),
}

/// Return the header of a segment.
pub(crate) fn encode_segment_header(header: SegmentHeader) -> [u8; SEGMENT_HEADER_LEN as usize] {
    let state = match header.state {
        SegmentState::InUse => IN_USE,
        SegmentState::Spare => SPARE,
    };
    let mut bytes = [0; SEGMENT_HEADER_LEN as usize];
    bytes[..8].copy_from_slice(&SEGMENT_MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&state.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.epoch.to_le_bytes());
    let checksum = xxh3_64(&bytes[..24]);
    bytes[24..].copy_from_slice(&checksum.to_le_bytes());
    bytes
}

/// Read the header of a segment from the first bytes of its file. A header
/// that fails its checksum is damaged, never a spare.
pub(crate) fn decode_segment_header(bytes: &[u8]) -> SegmentRead {
    let Some(header) = bytes.get(..SEGMENT_HEADER_LEN as usize) else {
        return SegmentRead::Damaged;
    };
    if header[..8] != SEGMENT_MAGIC || xxh3_64(&header[..24]) != u64_at(header, 24) {
        return SegmentRead::Damaged;
    }
    let version = u32_at(header, 8);
    if version != VERSION {
        return SegmentRead::Unreadable(format!(
            "a segment of the write-ahead log has the version {version}, which this build cannot read"
        ));
    }
    let state = match u32_at(header, 12) {
        IN_USE => SegmentState::InUse,
        SPARE => SegmentState::Spare,
        other => {
            return SegmentRead::Unreadable(format!(
                "a segment of the write-ahead log has the state {other}, which this build cannot read"
            ));
        }
    };
    SegmentRead::Header(SegmentHeader {
        state,
        epoch: u64_at(header, 16),
    })
}

/// The header of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntryHeader {
    /// The number of ops of the body.
    pub(crate) ops: u32,
    /// The length of the body, without the zeros of the alignment.
    pub(crate) body_len: u64,
    /// The epoch of the use of the segment.
    pub(crate) epoch: u64,
    /// The last tid of the store before the batch.
    pub(crate) previous: Tid,
    /// The tid of the last commit of the batch.
    pub(crate) last: Tid,
    /// The checksum of header bytes 0 to 39 and the body.
    body_sum: u64,
}

impl EntryHeader {
    /// Read the header of an entry from `bytes`, its first 56 bytes. Return
    /// `None` when the magic or the header checksum does not pass.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let header = bytes.get(..ENTRY_HEADER_LEN)?;
        if header[..4] != ENTRY_MAGIC || xxh3_64(&header[..48]) != u64_at(header, 48) {
            return None;
        }
        Some(Self {
            ops: u32_at(header, 4),
            body_len: u64_at(header, 8),
            epoch: u64_at(header, 16),
            previous: Tid(u64_at(header, 24)),
            last: Tid(u64_at(header, 32)),
            body_sum: u64_at(header, 40),
        })
    }

    /// Return the length of the entry: its header, its body and the zeros
    /// up to the next multiple of 8. `None` when it does not fit a `u64`.
    pub(crate) fn len(&self) -> Option<u64> {
        let end = self.body_len.checked_add(ENTRY_HEADER_LEN as u64)?;
        end.checked_next_multiple_of(ALIGN)
    }

    /// Tell if `body` passes the body checksum. `header` is the 56 bytes of
    /// the header of the entry.
    pub(crate) fn body_passes(&self, header: &[u8], body: &[u8]) -> bool {
        body.len() as u64 == self.body_len && body_sum(&header[..40], body) == self.body_sum
    }
}

/// Return the body checksum of an entry: the xxh3-64 hash of header bytes 0
/// to 39, then the body.
fn body_sum(header: &[u8], body: &[u8]) -> u64 {
    let mut hasher = Xxh3::new();
    hasher.update(header);
    hasher.update(body);
    hasher.digest()
}

/// The kind of an op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    /// A transaction row.
    Txn,
    /// A record.
    Record,
    /// A blob record.
    Blob,
    /// A row of an ephemeral namespace.
    Ephemeral,
    /// A row of a durable namespace.
    Durable,
    /// The removal of a row of an ephemeral namespace.
    RemoveEphemeral,
    /// The removal of a row of a durable namespace.
    RemoveDurable,
    /// A key of the index of the expiries.
    Expiry,
}

impl OpKind {
    fn byte(self) -> u8 {
        match self {
            OpKind::Txn => 1,
            OpKind::Record => 2,
            OpKind::Blob => 3,
            OpKind::Ephemeral => 4,
            OpKind::Durable => 5,
            OpKind::RemoveEphemeral => 6,
            OpKind::RemoveDurable => 7,
            OpKind::Expiry => 8,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => OpKind::Txn,
            2 => OpKind::Record,
            3 => OpKind::Blob,
            4 => OpKind::Ephemeral,
            5 => OpKind::Durable,
            6 => OpKind::RemoveEphemeral,
            7 => OpKind::RemoveDurable,
            8 => OpKind::Expiry,
            _ => return None,
        })
    }

    /// Return the flavour of a namespace op, a write or a removal.
    pub(crate) fn flavour(self) -> Option<Flavour> {
        match self {
            OpKind::Ephemeral | OpKind::RemoveEphemeral => Some(Flavour::Ephemeral),
            OpKind::Durable | OpKind::RemoveDurable => Some(Flavour::Durable),
            _ => None,
        }
    }
}

/// One op of an entry, with the places of its key and its frame in the
/// bytes of the entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Op {
    pub(crate) kind: OpKind,
    /// The tid of the commit of the op.
    pub(crate) tid: Tid,
    /// The oid of a record or of a blob record. 0 for the other ops.
    pub(crate) oid: Oid,
    /// The key of a namespace op. Empty for the other ops.
    pub(crate) key: Range<usize>,
    /// The frame. Empty for a removal and an expiry key.
    pub(crate) frame: Range<usize>,
}

/// One commit of an entry: its tid, its ops as indexes in the op list, and
/// its bytes in the entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Commit {
    pub(crate) tid: Tid,
    pub(crate) ops: Range<usize>,
    pub(crate) bytes: Range<usize>,
}

/// An entry and the list of its ops and commits.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The bytes of the entry: its header, its body and the zeros of the
    /// alignment.
    pub(crate) bytes: Vec<u8>,
    pub(crate) ops: Vec<Op>,
    pub(crate) commits: Vec<Commit>,
}

impl Entry {
    /// Return the last tid of the entry.
    pub(crate) fn last(&self) -> Tid {
        self.commits.last().map_or(Tid::ZERO, |commit| commit.tid)
    }

    /// Write the header of the entry with `epoch` and `previous`, and its
    /// checksums. The body does not change.
    pub(crate) fn seal(&mut self, epoch: u64, previous: Tid) {
        let body_len = self.bytes.len() - ENTRY_HEADER_LEN;
        let ops = u32::try_from(self.ops.len()).expect("an entry holds fewer than 2^32 ops");
        let last = self.last();
        let aligned = (self.bytes.len() as u64).next_multiple_of(ALIGN) as usize;
        let header = &mut self.bytes[..ENTRY_HEADER_LEN];
        header[..4].copy_from_slice(&ENTRY_MAGIC);
        header[4..8].copy_from_slice(&ops.to_le_bytes());
        header[8..16].copy_from_slice(&(body_len as u64).to_le_bytes());
        header[16..24].copy_from_slice(&epoch.to_le_bytes());
        header[24..32].copy_from_slice(&previous.0.to_le_bytes());
        header[32..40].copy_from_slice(&last.0.to_le_bytes());
        let (header, body) = self.bytes.split_at_mut(ENTRY_HEADER_LEN);
        let sum = body_sum(&header[..40], &body[..body_len]);
        header[40..48].copy_from_slice(&sum.to_le_bytes());
        let sum = xxh3_64(&header[..48]);
        header[48..56].copy_from_slice(&sum.to_le_bytes());
        self.bytes.resize(aligned, 0);
    }
}

/// The builder of an entry. The frames go straight into the bytes of the
/// entry: no frame is copied after its encode.
pub(crate) struct EntryBuilder {
    bytes: Vec<u8>,
    ops: Vec<Op>,
    commits: Vec<Commit>,
}

/// Why the builder refused an op: the op does not follow the rules of the
/// format.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refused(pub(crate) &'static str);

impl EntryBuilder {
    /// Return a builder whose bytes have room for `capacity` bytes.
    pub(crate) fn new(capacity: usize) -> Self {
        let mut bytes = Vec::with_capacity(capacity.max(ENTRY_HEADER_LEN));
        bytes.resize(ENTRY_HEADER_LEN, 0);
        Self {
            bytes,
            ops: Vec::new(),
            commits: Vec::new(),
        }
    }

    /// Return the tid of the current commit.
    fn tid(&self) -> Result<Tid, Refused> {
        self.commits
            .last()
            .map(|commit| commit.tid)
            .ok_or(Refused("an op before the transaction row of its commit"))
    }

    /// Add the transaction row of the commit `tid`, whose frame `frame`
    /// appends to the bytes. It starts a new commit, whose tid must be
    /// after the tid of the commit before it.
    pub(crate) fn txn(
        &mut self,
        tid: Tid,
        frame: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), Refused> {
        if self.commits.last().is_some_and(|commit| commit.tid >= tid) {
            return Err(Refused("the transaction rows are not in tid order"));
        }
        let start = self.bytes.len();
        self.bytes.push(OpKind::Txn.byte());
        self.bytes.extend_from_slice(&tid.0.to_le_bytes());
        let frame = self.framed(frame);
        let first = self.ops.len();
        self.ops.push(Op {
            kind: OpKind::Txn,
            tid,
            oid: Oid(0),
            key: 0..0,
            frame,
        });
        self.commits.push(Commit {
            tid,
            ops: first..first + 1,
            bytes: start..self.bytes.len(),
        });
        Ok(())
    }

    /// Add the record or the blob record (`kind`) of `oid` that `tid`
    /// writes, whose frame `frame` appends to the bytes. `tid` must be the
    /// tid of the current commit.
    pub(crate) fn record(
        &mut self,
        kind: OpKind,
        oid: Oid,
        tid: Tid,
        frame: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), Refused> {
        debug_assert!(matches!(kind, OpKind::Record | OpKind::Blob));
        if self.tid()? != tid {
            return Err(Refused("a record of another tid than its commit"));
        }
        self.bytes.push(kind.byte());
        self.bytes.extend_from_slice(&oid.0.to_le_bytes());
        let frame = self.framed(frame);
        self.push(Op {
            kind,
            tid,
            oid,
            key: 0..0,
            frame,
        });
        Ok(())
    }

    /// Add the row `key` of a namespace of `flavour`, whose frame `frame`
    /// appends to the bytes, to the current commit.
    pub(crate) fn kv(
        &mut self,
        flavour: Flavour,
        key: &[u8],
        frame: impl FnOnce(&mut Vec<u8>),
    ) -> Result<(), Refused> {
        let kind = match flavour {
            Flavour::Ephemeral => OpKind::Ephemeral,
            Flavour::Durable => OpKind::Durable,
        };
        let tid = self.tid()?;
        let key = self.keyed(kind, key)?;
        let frame = self.framed(frame);
        self.push(Op {
            kind,
            tid,
            oid: Oid(0),
            key,
            frame,
        });
        Ok(())
    }

    /// Add the removal of the row `key` of a namespace of `flavour` to the
    /// current commit.
    pub(crate) fn removal(&mut self, flavour: Flavour, key: &[u8]) -> Result<(), Refused> {
        let kind = match flavour {
            Flavour::Ephemeral => OpKind::RemoveEphemeral,
            Flavour::Durable => OpKind::RemoveDurable,
        };
        self.key_only(kind, key)
    }

    /// Add `key` of the index of the expiries to the current commit.
    pub(crate) fn expiry(&mut self, key: &[u8]) -> Result<(), Refused> {
        self.key_only(OpKind::Expiry, key)
    }

    fn key_only(&mut self, kind: OpKind, key: &[u8]) -> Result<(), Refused> {
        let tid = self.tid()?;
        let key = self.keyed(kind, key)?;
        self.push(Op {
            kind,
            tid,
            oid: Oid(0),
            key,
            frame: 0..0,
        });
        Ok(())
    }

    /// Write the kind byte, the length of `key` and `key`. Return the place
    /// of the key.
    fn keyed(&mut self, kind: OpKind, key: &[u8]) -> Result<Range<usize>, Refused> {
        let len =
            u16::try_from(key.len()).map_err(|_| Refused("a key longer than 65,535 bytes"))?;
        self.bytes.push(kind.byte());
        self.bytes.extend_from_slice(&len.to_le_bytes());
        let start = self.bytes.len();
        self.bytes.extend_from_slice(key);
        Ok(start..self.bytes.len())
    }

    /// Write the length of a frame, then the frame that `frame` appends.
    /// Return the place of the frame.
    fn framed(&mut self, frame: impl FnOnce(&mut Vec<u8>)) -> Range<usize> {
        let at = self.bytes.len();
        self.bytes.extend_from_slice(&[0; 4]);
        frame(&mut self.bytes);
        let start = at + 4;
        let len = u32::try_from(self.bytes.len() - start).expect("a frame fits in 4 GiB");
        self.bytes[at..start].copy_from_slice(&len.to_le_bytes());
        start..self.bytes.len()
    }

    /// Add `op` to the current commit.
    fn push(&mut self, op: Op) {
        self.ops.push(op);
        let commit = self.commits.last_mut().expect("an op follows a commit");
        commit.ops.end = self.ops.len();
        commit.bytes.end = self.bytes.len();
    }

    /// Return the entry. `Entry::seal` writes its header.
    pub(crate) fn finish(self) -> Entry {
        Entry {
            bytes: self.bytes,
            ops: self.ops,
            commits: self.commits,
        }
    }
}

/// Read the ops of the entry `bytes`, whose header `header` passes its
/// checksums, and check the rules of the format. The places of the keys
/// and of the frames are places in `bytes`. An entry that breaks a rule
/// gives the rule.
pub(crate) fn parse(bytes: &[u8], header: &EntryHeader) -> Result<(Vec<Op>, Vec<Commit>), String> {
    let end = usize::try_from(header.body_len)
        .ok()
        .and_then(|len| len.checked_add(ENTRY_HEADER_LEN))
        .filter(|&end| end <= bytes.len())
        .ok_or("the body runs past the entry")?;
    let mut at = ENTRY_HEADER_LEN;
    let mut ops = Vec::with_capacity(header.ops.min(1 << 16) as usize);
    let mut commits: Vec<Commit> = Vec::new();
    let take = |at: &mut usize, n: usize| -> Result<Range<usize>, String> {
        let start = *at;
        let stop = start
            .checked_add(n)
            .filter(|&stop| stop <= end)
            .ok_or("an op runs past the body")?;
        *at = stop;
        Ok(start..stop)
    };
    for _ in 0..header.ops {
        let start = at;
        let kind = take(&mut at, 1)?;
        let kind = OpKind::from_byte(bytes[kind.start])
            .ok_or_else(|| format!("an op of the kind {}", bytes[kind.start]))?;
        let mut op = Op {
            kind,
            tid: Tid::ZERO,
            oid: Oid(0),
            key: 0..0,
            frame: 0..0,
        };
        match kind {
            OpKind::Txn | OpKind::Record | OpKind::Blob => {
                let id = u64_at(bytes, take(&mut at, 8)?.start);
                let len = u32_at(bytes, take(&mut at, 4)?.start) as usize;
                op.frame = take(&mut at, len)?;
                if kind == OpKind::Txn {
                    op.tid = Tid(id);
                } else {
                    op.oid = Oid(id);
                }
            }
            _ => {
                let len = u16_at(bytes, take(&mut at, 2)?.start) as usize;
                op.key = take(&mut at, len)?;
                if matches!(kind, OpKind::Ephemeral | OpKind::Durable) {
                    let len = u32_at(bytes, take(&mut at, 4)?.start) as usize;
                    op.frame = take(&mut at, len)?;
                }
            }
        }
        if kind == OpKind::Txn {
            let after = commits.last().map_or(header.previous, |commit| commit.tid);
            if op.tid <= after {
                return Err("the transaction rows do not follow the previous tid".to_owned());
            }
            commits.push(Commit {
                tid: op.tid,
                ops: ops.len()..ops.len(),
                bytes: start..start,
            });
        }
        let commit = commits
            .last_mut()
            .ok_or("an op before the transaction row of its commit")?;
        op.tid = commit.tid;
        ops.push(op);
        commit.ops.end = ops.len();
        commit.bytes.end = at;
    }
    if at != end {
        return Err("the body holds bytes after its ops".to_owned());
    }
    match commits.last() {
        Some(commit) if commit.tid == header.last => Ok((ops, commits)),
        Some(_) => Err("the last transaction row is not the last tid of the entry".to_owned()),
        None => Err("an entry without a transaction row".to_owned()),
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spare(epoch: u64) -> SegmentHeader {
        SegmentHeader {
            state: SegmentState::Spare,
            epoch,
        }
    }

    #[test]
    fn a_segment_header_reads_back_and_a_flipped_bit_damages_it() {
        for state in [SegmentState::InUse, SegmentState::Spare] {
            let header = SegmentHeader { state, epoch: 7 };
            let bytes = encode_segment_header(header);
            assert_eq!(&bytes[..8], b"ZOE-WAL\0");
            assert_eq!(decode_segment_header(&bytes), SegmentRead::Header(header));
            for at in 0..bytes.len() {
                let mut damaged = bytes;
                damaged[at] ^= 1;
                assert_eq!(
                    decode_segment_header(&damaged),
                    SegmentRead::Damaged,
                    "{at}"
                );
            }
        }
        assert_eq!(decode_segment_header(&[0; 31]), SegmentRead::Damaged);
        assert_eq!(decode_segment_header(&[0; 32]), SegmentRead::Damaged);
    }

    #[test]
    fn a_header_of_another_version_or_state_is_unreadable() {
        let mut bytes = encode_segment_header(spare(1));
        bytes[8] = 2;
        let sum = xxh3_64(&bytes[..24]);
        bytes[24..].copy_from_slice(&sum.to_le_bytes());
        assert!(matches!(
            decode_segment_header(&bytes),
            SegmentRead::Unreadable(_)
        ));
        let mut bytes = encode_segment_header(spare(1));
        bytes[12] = 3;
        let sum = xxh3_64(&bytes[..24]);
        bytes[24..].copy_from_slice(&sum.to_le_bytes());
        assert!(matches!(
            decode_segment_header(&bytes),
            SegmentRead::Unreadable(_)
        ));
    }

    #[test]
    fn segment_names_read_back() {
        assert_eq!(segment_name(10), "wal/000000000000000a.wal");
        assert_eq!(new_name(10), "wal/000000000000000a.new");
        assert_eq!(
            parse_name("000000000000000a.wal"),
            Some(FileName::Segment(10))
        );
        assert_eq!(parse_name("000000000000000a.new"), Some(FileName::New(10)));
        for other in ["000000000000000A.wal", "0a.wal", "000000000000000a.tmp", ""] {
            assert_eq!(parse_name(other), None, "{other}");
        }
    }

    /// Return an entry of two commits: tid 5 with a record, a blob record
    /// and namespace ops, and tid 6 with a record.
    fn two_commits() -> Entry {
        let mut builder = EntryBuilder::new(0);
        builder
            .txn(Tid(5), |b| b.extend_from_slice(b"row5"))
            .unwrap();
        builder
            .record(OpKind::Record, Oid(1), Tid(5), |b| {
                b.extend_from_slice(b"r1")
            })
            .unwrap();
        builder
            .record(OpKind::Blob, Oid(1), Tid(5), |b| b.extend_from_slice(b"b1"))
            .unwrap();
        builder
            .kv(Flavour::Ephemeral, b"\x01ns", |b| b.extend_from_slice(b"e"))
            .unwrap();
        builder.expiry(b"expiry key").unwrap();
        builder.removal(Flavour::Durable, b"\x01nk").unwrap();
        builder
            .kv(Flavour::Durable, b"\x01n", |b| b.extend_from_slice(b"gen"))
            .unwrap();
        builder
            .txn(Tid(6), |b| b.extend_from_slice(b"row6"))
            .unwrap();
        builder
            .record(OpKind::Record, Oid(2), Tid(6), |b| {
                b.extend_from_slice(b"r2")
            })
            .unwrap();
        let mut entry = builder.finish();
        entry.seal(9, Tid(4));
        entry
    }

    #[test]
    fn an_entry_reads_back_with_its_ops() {
        let entry = two_commits();
        assert_eq!(entry.bytes.len() % 8, 0);
        let header = EntryHeader::decode(&entry.bytes).unwrap();
        assert_eq!(header.ops, 9);
        assert_eq!(header.epoch, 9);
        assert_eq!((header.previous, header.last), (Tid(4), Tid(6)));
        assert_eq!(header.len(), Some(entry.bytes.len() as u64));
        let body = &entry.bytes[ENTRY_HEADER_LEN..ENTRY_HEADER_LEN + header.body_len as usize];
        assert!(header.body_passes(&entry.bytes[..ENTRY_HEADER_LEN], body));
        let (ops, commits) = parse(&entry.bytes, &header).unwrap();
        assert_eq!(ops, entry.ops);
        assert_eq!(commits, entry.commits);
        assert_eq!(&entry.bytes[ops[1].frame.clone()], b"r1");
        assert_eq!(&entry.bytes[ops[3].key.clone()], b"\x01ns");
        assert_eq!(ops[8].tid, Tid(6));
        // The first op is the transaction row: kind 1, tid, length, frame.
        assert_eq!(entry.bytes[ENTRY_HEADER_LEN], 1);
        assert_eq!(u64_at(&entry.bytes, ENTRY_HEADER_LEN + 1), 5);
        assert_eq!(u32_at(&entry.bytes, ENTRY_HEADER_LEN + 9), 4);
    }

    #[test]
    fn a_flipped_bit_fails_the_header_or_the_body() {
        let entry = two_commits();
        let header = EntryHeader::decode(&entry.bytes).unwrap();
        let body_end = ENTRY_HEADER_LEN + header.body_len as usize;
        for at in 0..body_end {
            let mut bytes = entry.bytes.clone();
            bytes[at] ^= 0x10;
            match EntryHeader::decode(&bytes) {
                None => assert!(at < ENTRY_HEADER_LEN, "{at}"),
                Some(read) => {
                    assert!(at >= ENTRY_HEADER_LEN, "{at}");
                    let body = &bytes[ENTRY_HEADER_LEN..body_end];
                    assert!(!read.body_passes(&bytes[..ENTRY_HEADER_LEN], body), "{at}");
                }
            }
        }
    }

    /// Return the entry `bytes` with a new header for `ops` ops and the
    /// tids `previous` and `last`, sealed again.
    fn resealed(entry: &Entry, ops: u32, previous: Tid, last: Tid) -> (Vec<u8>, EntryHeader) {
        let mut bytes = entry.bytes.clone();
        let header = EntryHeader::decode(&bytes).unwrap();
        bytes[4..8].copy_from_slice(&ops.to_le_bytes());
        bytes[24..32].copy_from_slice(&previous.0.to_le_bytes());
        bytes[32..40].copy_from_slice(&last.0.to_le_bytes());
        let body_end = ENTRY_HEADER_LEN + header.body_len as usize;
        let (head, body) = bytes.split_at_mut(ENTRY_HEADER_LEN);
        let sum = body_sum(&head[..40], &body[..body_end - ENTRY_HEADER_LEN]);
        head[40..48].copy_from_slice(&sum.to_le_bytes());
        let sum = xxh3_64(&head[..48]);
        head[48..56].copy_from_slice(&sum.to_le_bytes());
        let header = EntryHeader::decode(&bytes).unwrap();
        (bytes, header)
    }

    #[test]
    fn an_entry_that_breaks_a_rule_is_refused() {
        let entry = two_commits();
        // The first tid is not after the previous tid.
        let (bytes, header) = resealed(&entry, 9, Tid(5), Tid(6));
        assert!(parse(&bytes, &header).is_err());
        // The last tid is not the last transaction row.
        let (bytes, header) = resealed(&entry, 9, Tid(4), Tid(7));
        assert!(parse(&bytes, &header).is_err());
        // An op count that leaves bytes in the body, or runs past it.
        let (bytes, header) = resealed(&entry, 8, Tid(4), Tid(6));
        assert!(parse(&bytes, &header).is_err());
        let (bytes, header) = resealed(&entry, 10, Tid(4), Tid(6));
        assert!(parse(&bytes, &header).is_err());
        // An unknown kind.
        let mut damaged = entry.bytes.clone();
        damaged[ENTRY_HEADER_LEN] = 9;
        let damaged = Entry {
            bytes: damaged,
            ops: Vec::new(),
            commits: Vec::new(),
        };
        let (bytes, header) = resealed(&damaged, 9, Tid(4), Tid(6));
        assert!(parse(&bytes, &header).is_err());
    }

    #[test]
    fn the_builder_refuses_ops_out_of_order() {
        let mut builder = EntryBuilder::new(0);
        assert!(
            builder
                .record(OpKind::Record, Oid(1), Tid(5), |_| {})
                .is_err()
        );
        assert!(builder.expiry(b"k").is_err());
        builder.txn(Tid(5), |_| {}).unwrap();
        assert!(builder.txn(Tid(5), |_| {}).is_err());
        assert!(
            builder
                .record(OpKind::Record, Oid(1), Tid(6), |_| {})
                .is_err()
        );
    }
}
