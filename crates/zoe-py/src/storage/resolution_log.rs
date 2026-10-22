//! The resolution log of ADR 0042: a file of JSON lines, one for each
//! result of a tier in a vote or an undo.

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use pyo3::prelude::*;
use zoe_client::lock;

use super::SharedTxn;

/// The resolution log of ADR 0042: a file of JSON lines, one for each
/// result of tier 1 in a vote. It is opened in append mode, each vote
/// writes its lines in one write, and it is never synced.
pub(super) struct ResolutionLog {
    file: Mutex<File>,
    /// True after a write failed: the storage warns at the first one.
    failed: AtomicBool,
}

impl ResolutionLog {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(ResolutionLog {
            file: Mutex::new(file),
            failed: AtomicBool::new(false),
        })
    }

    /// Writes the lines of what the tiers did in the vote of `txn`. The
    /// entries of the undos come first, and `_undo` wrote them already, so
    /// they are left out. A failed write never fails the vote. Returns the
    /// error of the first failed write, for the log of the storage.
    pub(super) fn write(&self, txn: &SharedTxn) -> Option<String> {
        let vote = txn.undo_resolution_count()..txn.resolutions().len();
        self.write_lines(txn.resolution_log_lines_of(vote, SystemTime::now(), None))
    }

    /// Writes the lines of the entries `range` of `txn`, that an undo gave,
    /// as `write` does.
    pub(super) fn write_undo(&self, txn: &SharedTxn, range: std::ops::Range<usize>) -> Option<String> {
        self.write_lines(txn.resolution_log_lines_of(range, SystemTime::now(), None))
    }

    /// Writes the lines of the checks of the merges of shadow mode of the
    /// vote after `_vote_resolving` (ADR 0046), as `write` does.
    pub(super) fn write_verifications(&self, txn: &SharedTxn) -> Option<String> {
        self.write_lines(txn.verification_log_lines(SystemTime::now(), None))
    }

    fn write_lines(&self, lines: Vec<String>) -> Option<String> {
        if lines.is_empty() {
            return None;
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        let mut file = lock(&self.file);
        match io::Write::write_all(&mut *file, text.as_bytes()) {
            Err(error) if !self.failed.swap(true, Ordering::Relaxed) => Some(error.to_string()),
            _ => None,
        }
    }
}

/// Log a failed write of the resolution log as a warning of the logger
/// `zoe.storage`. Later failures are not logged.
pub(super) fn warn_log_failure(py: Python<'_>, error: &str) {
    let message =
        format!("a write of the resolution log failed; later failures are not logged: {error}");
    let logged = py
        .import("logging")
        .and_then(|logging| logging.call_method1("getLogger", ("zoe.storage",)))
        .and_then(|logger| logger.call_method1("warning", (message,)));
    if let Err(error) = logged {
        error.write_unraisable(py, None);
    }
}
