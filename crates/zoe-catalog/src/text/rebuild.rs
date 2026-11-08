//! The rebuild of `zoe catalog rebuild-text` (ADR 0060, section 15): the
//! text index of a catalog, built again from the live texts of the deltas
//! at the last tid, in a store folder that no other process holds.
//!
//! The rebuild uses the build of the thread of the text indexes, with 4
//! indexing threads, and writes the new index in `text/<oid>.new`. Then it
//! renames `text/<oid>` to `text/<oid>.old`, renames `text/<oid>.new` to
//! `text/<oid>`, syncs `text/` and removes `text/<oid>.old`. rename(2)
//! does not replace a directory that holds files, so two renames are
//! necessary. The old index stays until the new one is complete.
//!
//! An open of the text index of a catalog, and each rebuild, first removes
//! a `.new` or `.old` directory that a crash left (`clean`). With no
//! `text/<oid>`, the open rebuilds the index.
//!
//! The rebuild reads the store and writes nothing in its tables. Its
//! report holds counts and times, never a text.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use zoe_core::Store;

use super::index;
use super::settings::CatalogSettings;
use super::texts::{self, Halt};

/// The indexing threads of the writer of a rebuild.
const THREADS: usize = 4;

/// A step of a rebuild, for the hook of the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `.new` holds an empty index.
    Created,
    /// The writer holds each document, before its commit.
    Added,
    /// `.new` holds the whole index, committed, and no merge runs.
    Committed,
    /// The old index is in `.old`.
    Retired,
    /// The new index is in `text/<oid>`.
    Placed,
    /// `text/` is synced.
    Synced,
    /// `.old` is removed.
    Removed,
}

impl Step {
    /// Each step, in its order.
    pub const ALL: [Step; 7] = [
        Step::Created,
        Step::Added,
        Step::Committed,
        Step::Retired,
        Step::Placed,
        Step::Synced,
        Step::Removed,
    ];

    /// The name of the step.
    pub fn name(self) -> &'static str {
        match self {
            Step::Created => "created",
            Step::Added => "added",
            Step::Committed => "committed",
            Step::Retired => "retired",
            Step::Placed => "placed",
            Step::Synced => "synced",
            Step::Removed => "removed",
        }
    }
}

/// What the rebuild of one catalog did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// The oid of the catalog.
    pub catalog: u64,
    /// The documents of the new index.
    pub docs: u64,
    /// The tid up to which the new index holds the deltas.
    pub tid: u64,
    /// The time of the rebuild.
    pub time: Duration,
}

/// The catalogs whose text index a rebuild writes, with their settings, in
/// the order of their oids: each registered catalog whose settings in
/// `zoe.text` name a text index in the enhanced mode. With `only`, that
/// catalog alone.
///
/// Settings that do not read fail the call, as does an `only` that names
/// no such catalog. So a rebuild writes nothing when a catalog fails here.
pub fn catalogs(store: &Store, only: Option<u64>) -> Result<Vec<(u64, CatalogSettings)>, String> {
    let registered =
        texts::registered(store).map_err(|e| format!("the read of the catalogs fails: {e}"))?;
    let mut out = Vec::new();
    for (catalog, settings) in registered {
        if only.is_some_and(|o| o != catalog) {
            continue;
        }
        let settings = settings.map_err(|why| {
            format!("the catalog {catalog:#018x}: the settings of zoe.text: {why}")
        })?;
        if !texts::layout_of(&settings).is_empty() {
            out.push((catalog, settings));
        }
    }
    if let Some(catalog) = only
        && out.is_empty()
    {
        return Err(format!(
            "the catalog {catalog:#018x} is not registered, or has no text index in the enhanced mode"
        ));
    }
    Ok(out)
}

/// The directory beside `path` whose name ends in `.suffix`.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Remove the directory `path`. A missing one is no error.
fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Remove the `.new` and the `.old` directory of the index in `path`,
/// which a crash of a rebuild can leave.
pub fn clean(path: &Path) -> io::Result<()> {
    remove(&beside(path, "new"))?;
    remove(&beside(path, "old"))
}

/// Sync the directory `dir`.
fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Rebuild the text index of `catalog` with `settings` in `text/` of the
/// store directory `dir`, from the live texts of `store` at its last tid.
/// The caller holds the lock of the store. `step` gets each step.
///
/// A failure before the first rename removes `.new` and leaves the old
/// index.
pub fn rebuild(
    store: &Store,
    dir: &Path,
    catalog: u64,
    settings: &CatalogSettings,
    step: &dyn Fn(Step),
) -> Result<Rebuilt, String> {
    let start = Instant::now();
    let failed =
        |what: &str, e: &dyn std::fmt::Display| format!("the catalog {catalog:#018x}: {what}: {e}");
    let path = index::directory(dir, catalog);
    let new = beside(&path, "new");
    let old = beside(&path, "old");
    clean(&path).map_err(|e| failed("the removal of a directory of a rebuild fails", &e))?;
    let layout = texts::layout_of(settings);
    let cancel = AtomicBool::new(false);
    let built = loop {
        match texts::build(store, catalog, &layout, &new, THREADS, &cancel, step) {
            Ok(built) => break Ok(built),
            // The caller holds the lock of the store, so no other process
            // packs it; a pack of this process passed the cap.
            Err(Halt::Packed) => continue,
            Err(Halt::Cancelled) => break Err(failed("the build", &"cancelled")),
            Err(Halt::Store(e)) => break Err(failed("a read of the deltas fails", &e)),
            Err(Halt::Index(why)) => break Err(failed("the build", &why)),
        }
    };
    let placed = built.and_then(|built| {
        let (docs, at) = (built.docs, built.at);
        built
            .writer
            .wait_merging_threads()
            .map_err(|e| failed("the merges of the new index fail", &e))?;
        drop(built.index);
        drop(built.cap);
        step(Step::Committed);
        match std::fs::rename(&path, &old) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(failed("the rename of the old index fails", &e));
            }
            _ => {}
        }
        Ok((docs, at))
    });
    let (docs, tid) = match placed {
        Ok(found) => found,
        Err(why) => {
            let _ = remove(&new);
            return Err(why);
        }
    };
    step(Step::Retired);
    std::fs::rename(&new, &path).map_err(|e| failed("the rename of the new index fails", &e))?;
    step(Step::Placed);
    let text = path.parent().unwrap_or(dir);
    sync_dir(text).map_err(|e| failed("the sync of text/ fails", &e))?;
    step(Step::Synced);
    remove(&old).map_err(|e| failed("the removal of the old index fails", &e))?;
    step(Step::Removed);
    Ok(Rebuilt {
        catalog,
        docs,
        tid,
        time: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directories_of_a_rebuild_sit_beside_the_index() {
        let path = Path::new("/srv/store/text/0000000000000010");
        assert_eq!(
            beside(path, "new"),
            Path::new("/srv/store/text/0000000000000010.new")
        );
        assert_eq!(
            beside(path, "old"),
            Path::new("/srv/store/text/0000000000000010.old")
        );
    }

    #[test]
    fn each_step_has_its_own_name() {
        let names: std::collections::BTreeSet<_> = Step::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names.len(), Step::ALL.len());
    }
}
