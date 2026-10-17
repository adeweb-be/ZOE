//! The resolution log of ADR 0042: a file of JSON lines, one for each
//! result of tier 1 in a vote. `--resolution-log` names it.
//!
//! The lines hold the user and the description of both transactions, and
//! the keys that tier 1 merged, so they never go to the server log. The
//! file is opened in append mode at the start, and each vote writes its
//! lines in one write. The file is never synced: it is not in the store.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// An open resolution log.
pub(crate) struct ResolutionLog {
    path: PathBuf,
    file: Mutex<File>,
    /// True after a write failed: the server logs the first failure only.
    failed: AtomicBool,
}

impl ResolutionLog {
    /// Opens the file `path` in append mode, and creates it if necessary.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(ResolutionLog {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            failed: AtomicBool::new(false),
        })
    }

    /// Writes `lines`, each with a line feed, in one write. A failed write
    /// never fails the vote: the server logs a warning at the first one.
    pub(crate) fn write(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = file.write_all(text.as_bytes())
            && !self.failed.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "a write of the resolution log failed; later failures are not logged"
            );
        }
    }
}
