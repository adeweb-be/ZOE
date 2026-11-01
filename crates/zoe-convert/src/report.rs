//! The report of a conversion: the counts, the problem lines and the kind
//! of each problem.

use std::collections::BTreeMap;

use zoe_core::Tid;

/// The report lists at most this many problems about single transactions,
/// records and blobs, then a count of the others.
pub(crate) const MAX_PROBLEMS: usize = 100;

/// The report of a conversion.
///
/// The counts describe the source, up to where the conversion stopped. A
/// conversion that verifies has no problems.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The transactions of the source.
    pub transactions: u64,
    /// The transactions that this run copied.
    pub copied: u64,
    /// The data records of the source. A record without data counts.
    pub records: u64,
    /// The blob files of the source.
    pub blobs: u64,
    /// The tid of the last transaction of the source. It is `Tid::ZERO`
    /// when the source has no transaction.
    pub last_tid: Tid,
    /// The keys of the durable KV namespaces of a store that the
    /// conversion into a `Data.fs` left out: a `Data.fs` holds no
    /// namespace (ADR 0043). A conversion from a `Data.fs` has none.
    pub durable_keys: u64,
    /// What the verification found, one problem per line. The lines about
    /// the whole source or the whole file come first: where a read of a
    /// `Data.fs` stopped before the end of the file, and the blob records
    /// without a blob directory. At most 100 lines about single
    /// transactions, records and blobs follow, and a last line counts the
    /// others.
    pub problems: Vec<String>,
    /// The count of the problems of each kind, with no cap. A kind without
    /// a problem has no entry.
    counts: BTreeMap<ProblemKind, u64>,
}

impl Report {
    /// Return an empty report: no transaction and no problem.
    pub(crate) fn empty() -> Self {
        Self {
            transactions: 0,
            copied: 0,
            records: 0,
            blobs: 0,
            last_tid: Tid::ZERO,
            durable_keys: 0,
            problems: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    /// Return the number of problems of the kind `kind`. The count has no
    /// cap.
    pub fn count(&self, kind: ProblemKind) -> u64 {
        self.counts.get(&kind).copied().unwrap_or(0)
    }

    /// Return the number of problems. This is the sum of the counts of the
    /// kinds, not the number of lines in `problems`.
    pub fn problem_count(&self) -> u64 {
        self.counts.values().sum()
    }

    /// Return each kind that has a problem, with its count, in the order of
    /// `ProblemKind`.
    pub fn counts(&self) -> impl Iterator<Item = (ProblemKind, u64)> + '_ {
        self.counts.iter().map(|(&kind, &count)| (kind, count))
    }
}

/// The kind of a problem in the report of a conversion. Each problem has
/// exactly one kind.
///
/// The source is the side that the conversion reads, and the target is
/// the side that it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProblemKind {
    /// The read of the `Data.fs` stopped before the end of the file.
    Stopped,
    /// The `Data.fs` has blob records, and no blob directory was given.
    /// This is one problem, whatever the number of blob records.
    NoBlobDirectory,
    /// One side has a transaction that the other side does not have.
    MissingTransaction,
    /// The user, the description or the extension of a transaction
    /// differs.
    MetadataDiffers,
    /// One side of a transaction has a record of an oid that the other
    /// side does not have.
    MissingRecord,
    /// A pickle of the store differs from the pickle in the file.
    RecordDiffers,
    /// The blob directory has no blob file for a blob record.
    MissingBlobFile,
    /// The store has no blob for a blob record of the source.
    MissingBlob,
    /// A blob of the store and its blob file have different sizes or
    /// different bytes.
    BlobDiffers,
    /// The store does not read a blob, or its blob file.
    UnreadableBlob,
    /// The transaction iterator of the store failed.
    UnreadableStore,
}

impl ProblemKind {
    /// Return the name of the problems of this kind, in the plural, for a
    /// line of a report.
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stops before the end of the file",
            Self::NoBlobDirectory => "no blob directory",
            Self::MissingTransaction => "missing transactions",
            Self::MetadataDiffers => "users, descriptions or extensions that differ",
            Self::MissingRecord => "missing records",
            Self::RecordDiffers => "records that differ",
            Self::MissingBlobFile => "missing blob files",
            Self::MissingBlob => "blobs that the store does not have",
            Self::BlobDiffers => "blobs that differ",
            Self::UnreadableBlob => "blobs that the store does not read",
            Self::UnreadableStore => "failed reads of the store",
        }
    }
}

/// The problems that a verification found.
///
/// A summary problem is about the whole source. The summary problems come
/// first, and the list keeps all of them. The list keeps `MAX_PROBLEMS`
/// other problems, and a last line counts the others.
///
/// `add` and `add_summary` count each problem under its kind, with no
/// cap. `push` and `summary` give a problem without a kind.
#[derive(Default)]
pub(crate) struct Problems {
    summary: Vec<String>,
    list: Vec<String>,
    more: u64,
    counts: BTreeMap<ProblemKind, u64>,
}

impl Problems {
    pub(crate) fn summary(&mut self, problem: String) {
        self.summary.push(problem);
    }

    pub(crate) fn push(&mut self, problem: String) {
        if self.list.len() < MAX_PROBLEMS {
            self.list.push(problem);
        } else {
            self.more += 1;
        }
    }

    /// Add a summary problem of the kind `kind`.
    pub(crate) fn add_summary(&mut self, kind: ProblemKind, problem: String) {
        *self.counts.entry(kind).or_default() += 1;
        self.summary(problem);
    }

    /// Add a problem of the kind `kind`.
    pub(crate) fn add(&mut self, kind: ProblemKind, problem: String) {
        *self.counts.entry(kind).or_default() += 1;
        self.push(problem);
    }

    pub(crate) fn finish(mut self) -> Vec<String> {
        self.summary.append(&mut self.list);
        if self.more > 0 {
            self.summary.push(format!("{} more problems", self.more));
        }
        self.summary
    }

    /// Put the lines and the counts of the problems into `report`.
    pub(crate) fn finish_into(mut self, report: &mut Report) {
        report.counts = std::mem::take(&mut self.counts);
        report.problems = self.finish();
    }
}
