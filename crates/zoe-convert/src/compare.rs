//! The comparisons that the checks of the conversions and of the backups
//! share.
//!
//! [`TxnJoin`] walks the transactions of a `Data.fs` and of a store
//! together, in tid order. [`diff_records`] walks the records of two
//! transactions together, in oid order. [`compare_txn`] compares a
//! transaction of a `Data.fs` with one of a store, and [`check_blob`]
//! compares a blob of the store with its blob file.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::iter::Peekable;
use std::path::Path;

use zoe_core::{Oid, Store, Tid, TxnInfo, ZoeError};

use crate::filestorage::{Cut, DataRecord, End, Reader, TxnHeader};
use crate::fsutil::{CHUNK, io_error};
use crate::report::Problems;
use crate::{Error, ProblemKind};

/// The verification compares the first blob, then one blob in this many,
/// byte for byte. It checks the size of every other blob. The copy hashes
/// each blob as it writes it, so the sample checks the read path of the
/// store against the source.
pub(crate) const BLOB_SAMPLE_STRIDE: u64 = 16;

/// A transaction of a `Data.fs` with its records.
pub(crate) type FileTxn = (TxnHeader, Vec<DataRecord>);

/// One step of a [`TxnJoin`].
pub(crate) enum TxnPair {
    /// The file and the store both have a transaction of this tid.
    Both(FileTxn, TxnInfo),
    /// The file has a transaction that the store does not have.
    FileOnly(FileTxn),
    /// The store has a transaction that the file does not have, or the
    /// transaction iterator of the store failed.
    StoreOnly(zoe_core::Result<TxnInfo>),
}

/// A walk over the transactions of a `Data.fs` and of a store together, in
/// tid order. It holds one transaction of each side at a time.
///
/// An error of the store iterator comes as soon as the iterator gives it,
/// before the next transaction of the file. The walk reads the next
/// transaction of the file only when a step needs it.
pub(crate) struct TxnJoin<'r, I: Iterator<Item = zoe_core::Result<TxnInfo>>> {
    reader: &'r mut Reader,
    /// The next transaction of the file, when the walk read it.
    file: Option<FileTxn>,
    /// True when the reader has no more transactions.
    file_done: bool,
    store: Peekable<I>,
}

impl<'r, I: Iterator<Item = zoe_core::Result<TxnInfo>>> TxnJoin<'r, I> {
    /// Start a walk over the transactions of `reader` and of `store`.
    pub(crate) fn new(reader: &'r mut Reader, store: I) -> Self {
        Self {
            reader,
            file: None,
            file_done: false,
            store: store.peekable(),
        }
    }

    /// Return the next step, or `None` when both sides end. An error of
    /// the file stops the walk.
    pub(crate) fn next(&mut self) -> Result<Option<TxnPair>, Error> {
        if let Some(error) = self.store.next_if(Result::is_err) {
            return Ok(Some(TxnPair::StoreOnly(error)));
        }
        if self.file.is_none() && !self.file_done {
            self.file = read_txn(self.reader)?;
            self.file_done = self.file.is_none();
        }
        let file = self.file.as_ref().map(|(header, _)| header.tid);
        let store = match self.store.peek() {
            Some(Ok(txn)) => Some(txn.tid),
            _ => None,
        };
        let pair = match (file, store) {
            (Some(file), Some(store)) if file == store => self
                .file
                .take()
                .zip(self.store.next().and_then(Result::ok))
                .map(|(file, txn)| TxnPair::Both(file, txn)),
            (Some(file), store) if store.is_none_or(|store| file < store) => {
                self.file.take().map(TxnPair::FileOnly)
            }
            _ => self.store.next().map(TxnPair::StoreOnly),
        };
        Ok(pair)
    }
}

/// Read the next transaction of `reader` with its records.
pub(crate) fn read_txn(reader: &mut Reader) -> Result<Option<FileTxn>, Error> {
    let Some(header) = reader.next_txn()? else {
        return Ok(None);
    };
    let records = reader.records(&header).collect::<Result<_, _>>()?;
    Ok(Some((header, records)))
}

/// One oid of a [`diff_records`] walk.
pub(crate) enum RecordDiff<'a> {
    /// Both sides have a record of the oid. Each data is `None` for a
    /// deletion.
    Both(Oid, Option<&'a [u8]>, Option<&'a [u8]>),
    /// Only the first side has a record of the oid.
    FirstOnly(Oid),
    /// Only the second side has a record of the oid.
    SecondOnly(Oid),
}

/// Walk the records of two transactions together, in oid order. Each side
/// gives the oid and the data of its records in ascending oid order, one
/// record for each oid.
pub(crate) fn diff_records<'a>(
    first: impl IntoIterator<Item = (Oid, Option<&'a [u8]>)>,
    second: impl IntoIterator<Item = (Oid, Option<&'a [u8]>)>,
) -> impl Iterator<Item = RecordDiff<'a>> {
    let mut first = first.into_iter().peekable();
    let mut second = second.into_iter().peekable();
    std::iter::from_fn(move || {
        let a = first.peek().map(|&(oid, _)| oid);
        let b = second.peek().map(|&(oid, _)| oid);
        match (a, b) {
            (None, None) => None,
            (Some(a), Some(b)) if a == b => {
                let (oid, data) = first.next()?;
                let (_, other) = second.next()?;
                Some(RecordDiff::Both(oid, data, other))
            }
            (Some(a), b) if b.is_none_or(|b| a < b) => {
                first.next();
                Some(RecordDiff::FirstOnly(a))
            }
            _ => second.next().map(|(oid, _)| RecordDiff::SecondOnly(oid)),
        }
    })
}

/// Return the oid and the data of each record of the transaction `txn` of
/// a store, in oid order. The data is `None` for a deletion.
pub(crate) fn txn_records(txn: &TxnInfo) -> impl Iterator<Item = (Oid, Option<&[u8]>)> {
    txn.records
        .iter()
        .map(|record| (record.oid, record.data.as_deref()))
}

/// Describe a stop of the reader before the end of the file. A clean end
/// and a commit in progress at the end are not problems.
pub(crate) fn stop_problem(end: End) -> Option<String> {
    let End::Cut { pos, cut } = end else {
        return None;
    };
    let what = match cut {
        Cut::Truncated => format!("the Data.fs ends inside the transaction at offset {pos}"),
        Cut::Junk => format!(
            "the Data.fs holds no valid transaction from offset {pos} to its end, \
             as after a crash"
        ),
        Cut::Commit => format!(
            "the transaction at offset {pos} has the status c, \
             but it is not a whole last transaction"
        ),
    };
    Some(format!("{what}: the conversion stops there"))
}

/// Compare the transaction `header` of the file and its `records` with
/// the transaction `txn` of the store.
pub(crate) fn compare_txn(
    header: &TxnHeader,
    records: &[DataRecord],
    txn: &TxnInfo,
    problems: &mut Problems,
) {
    let tid = header.tid.hex();
    for (name, file, store) in [
        ("user", &header.user, &txn.user),
        ("description", &header.description, &txn.description),
        ("extension", &header.extension, &txn.extension),
    ] {
        if file != store {
            problems.add(
                ProblemKind::MetadataDiffers,
                format!("transaction {tid}: the {name} differs"),
            );
        }
    }

    // The last record of an oid wins, as in a restore transaction.
    let file: BTreeMap<Oid, Option<&[u8]>> = records
        .iter()
        .map(|record| (record.oid, record.data.as_deref()))
        .collect();
    // The records that only the store has come after the others.
    let mut extra = Vec::new();
    for diff in diff_records(file, txn_records(txn)) {
        match diff {
            RecordDiff::Both(oid, data, stored) if stored != data => problems.add(
                ProblemKind::RecordDiffers,
                format!("transaction {tid}: the record of oid {} differs", oid.hex()),
            ),
            RecordDiff::Both(..) => {}
            RecordDiff::FirstOnly(oid) => problems.add(
                ProblemKind::MissingRecord,
                format!(
                    "transaction {tid}: the store lacks the record of oid {}",
                    oid.hex()
                ),
            ),
            RecordDiff::SecondOnly(oid) => extra.push(oid),
        }
    }
    for oid in extra {
        problems.add(
            ProblemKind::MissingRecord,
            format!(
                "transaction {tid}: the store has a record of oid {}, which the Data.fs does not have",
                oid.hex()
            ),
        );
    }
}

/// Check that `target` holds the blob of `oid` at `tid` with `size`
/// bytes. When `sample` is true, also compare it byte for byte with the
/// blob file `source`, and check its hash. Return the kind and the text of
/// the problem found.
pub(crate) fn check_blob(
    target: &Store,
    oid: Oid,
    tid: Tid,
    source: &Path,
    size: u64,
    sample: bool,
) -> Result<Option<(ProblemKind, String)>, Error> {
    let unreadable = |text: String| Ok(Some((ProblemKind::UnreadableBlob, text)));
    let path = match target.blob_path(oid, tid) {
        Ok(path) => path,
        Err(ZoeError::NotFound { .. }) => {
            return Ok(Some((
                ProblemKind::MissingBlob,
                "the store has no blob".into(),
            )));
        }
        Err(error) => return unreadable(format!("the store does not read the blob: {error}")),
    };
    match fs::metadata(&path) {
        Ok(meta) if meta.len() == size => {}
        Ok(meta) => {
            return Ok(Some((
                ProblemKind::BlobDiffers,
                format!(
                    "the sizes differ: {} bytes in the store, {size} in the blob file",
                    meta.len()
                ),
            )));
        }
        Err(error) => return unreadable(format!("the blob file of the store: {error}")),
    }
    if !sample {
        return Ok(None);
    }

    let io = |error| io_error(source, error);
    let mut original = File::open(source).map_err(io)?;
    let mut copy = match target.open_blob(oid, tid) {
        Ok(copy) => copy,
        Err(error) => return unreadable(format!("the store does not open the blob: {error}")),
    };
    let (mut want, mut got) = (vec![0; CHUNK], vec![0; CHUNK]);
    loop {
        let n = fill(&mut original, &mut want).map_err(io)?;
        // The reader of the store checks the size and the hash when it
        // reaches the end.
        let m = match fill(&mut copy, &mut got) {
            Ok(m) => m,
            Err(error) => return unreadable(format!("the blob of the store: {error}")),
        };
        if want[..n] != got[..m] {
            return Ok(Some((
                ProblemKind::BlobDiffers,
                "the blob of the store differs from the blob file".into(),
            )));
        }
        if n == 0 {
            return Ok(None);
        }
    }
}

/// Read from `reader` until `buf` is full or the reader ends. Return the
/// count of bytes read.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match reader.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(n)
}
