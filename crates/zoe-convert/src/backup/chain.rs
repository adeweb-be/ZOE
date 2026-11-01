//! The chain of dump files of a backup directory, and their names.

use std::fs;
use std::path::{Path, PathBuf};

use zoe_core::Tid;

use super::damaged;
use super::dump::read_header;
use crate::Error;
use crate::fsutil::io_error;

/// The suffix of the name of a dump file.
const SUFFIX: &str = ".zoedump";

/// One dump file of a backup directory.
pub(super) struct DumpFile {
    pub(super) path: PathBuf,
    /// The last tid of the dump before it, or `Tid::ZERO`.
    pub(super) after: Tid,
    /// The last tid that the dump covers.
    pub(super) last: Tid,
}

/// Return the name of the dump file of the tids after `after` up to
/// `last`.
pub(super) fn dump_name(after: Tid, last: Tid) -> String {
    format!("{:016x}-{:016x}{SUFFIX}", after.0, last.0)
}

/// Return the two tids of the name of a dump file, or `None` for another
/// name.
pub(super) fn parse_name(name: &str) -> Option<(Tid, Tid)> {
    let (after, last) = name.strip_suffix(SUFFIX)?.split_once('-')?;
    let tid = |hex: &str| {
        let digits = hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        digits.then(|| u64::from_str_radix(hex, 16).ok().map(Tid))?
    };
    Some((tid(after)?, tid(last)?))
}

/// Return the dump files of `dir` in order. Each one must start after the
/// last tid of the one before it, and the first one at the start.
///
/// The call reads the header of each dump file and checks that it names
/// the tids of the file name. Names of other forms do not count.
pub(super) fn chain(dir: &Path) -> Result<Vec<DumpFile>, Error> {
    let entries = fs::read_dir(dir).map_err(|source| io_error(dir, source))?;
    let mut dumps = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        let Some((after, last)) = entry.file_name().to_str().and_then(parse_name) else {
            continue;
        };
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error(&path, source))?;
        if !kind.is_file() {
            return Err(damaged(&path, "not a regular file".to_owned()));
        }
        dumps.push(DumpFile { path, after, last });
    }
    dumps.sort_by_key(|dump| (dump.after, dump.last));

    let mut end: Option<Tid> = None;
    for dump in &dumps {
        let reason = match end {
            None if dump.after != Tid::ZERO => Some(format!(
                "the full dump is missing: the first dump starts after tid {}",
                dump.after.hex()
            )),
            Some(end) if dump.after < end => Some(format!(
                "the dumps overlap: this dump starts after tid {}, before the end {} of the dump before it",
                dump.after.hex(),
                end.hex()
            )),
            Some(end) if dump.after > end => Some(format!(
                "a dump is missing: this dump starts after tid {}, and the dump before it ends at tid {}",
                dump.after.hex(),
                end.hex()
            )),
            // Only a full dump of an empty store covers no tid.
            _ if dump.last < dump.after || (dump.last == dump.after && end.is_some()) => {
                Some("the dump covers no tid".to_owned())
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return Err(damaged(&dump.path, reason));
        }
        let header = read_header(&dump.path)?;
        if header != (dump.after, dump.last) {
            let reason = format!(
                "the header names the tids {} and {}, not those of the file name",
                header.0.hex(),
                header.1.hex()
            );
            return Err(damaged(&dump.path, reason));
        }
        end = Some(dump.last);
    }
    Ok(dumps)
}
