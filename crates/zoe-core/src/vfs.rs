//! The simulated filesystem of the crash tests.
//!
//! `MemVfs` keeps its files in memory and records every write, length
//! change and sync. `MemVfs::crash_images` replays that record and returns
//! the states that a crash can leave on disk. A redb database runs on a
//! `MemFile`, so a test can crash a whole store after every operation.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

/// One recorded operation.
#[derive(Clone, Debug)]
enum Op {
    Write {
        file: String,
        offset: u64,
        data: Vec<u8>,
    },
    SetLen {
        file: String,
        len: u64,
    },
    Sync {
        file: String,
    },
    Mark {
        name: String,
    },
}

#[derive(Debug, Default)]
struct State {
    /// The files when the record starts.
    initial: BTreeMap<String, Vec<u8>>,
    /// The files as the program sees them.
    files: BTreeMap<String, Vec<u8>>,
    /// Every operation since the record started, in order.
    log: Vec<Op>,
}

/// An in-memory filesystem that records every operation.
///
/// Clones share the same files and the same record.
#[derive(Clone, Debug, Default)]
pub struct MemVfs {
    state: Arc<Mutex<State>>,
}

impl MemVfs {
    /// Return an empty filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_files(files: BTreeMap<String, Vec<u8>>) -> Self {
        let state = State {
            initial: files.clone(),
            files,
            log: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in a test thread must not hide the files from the others.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Return the file `name`. The file is empty until something writes it.
    pub fn file(&self, name: &str) -> MemFile {
        MemFile {
            vfs: self.clone(),
            name: name.to_owned(),
        }
    }

    /// Record a named point. `CrashImage::is_after` tells if a crash came
    /// after it.
    pub fn mark(&self, name: &str) {
        self.lock().log.push(Op::Mark {
            name: name.to_owned(),
        });
    }

    /// Return the states that a crash can leave on disk.
    ///
    /// For every crash point (after 0, 1, ... n operations), the synced
    /// operations are on disk. The pending operations (not followed by a
    /// sync of their file) can be on disk or not, in any subset: all, none,
    /// each prefix, each single one, and the last one torn in half.
    pub fn crash_images(&self) -> Vec<CrashImage> {
        let state = self.lock();
        let mut images = Vec::new();
        for point in 0..=state.log.len() {
            let ops = &state.log[..point];
            let (synced, pending) = split_at_syncs(ops);
            let marks: Vec<String> = ops
                .iter()
                .filter_map(|op| match op {
                    Op::Mark { name } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            let base = apply(&state.initial, ops, &synced, None);
            let mut push = |variant: Variant, files| {
                images.push(CrashImage {
                    point,
                    variant,
                    marks: marks.clone(),
                    files,
                });
            };

            push(Variant::SyncedOnly, base.clone());
            if pending.is_empty() {
                continue;
            }
            push(Variant::AllPending, apply(&base, ops, &pending, None));
            for n in 1..pending.len() {
                push(
                    Variant::PendingPrefix(n),
                    apply(&base, ops, &pending[..n], None),
                );
            }
            if pending.len() > 1 {
                for &i in &pending {
                    push(Variant::OnlyPending(i), apply(&base, ops, &[i], None));
                }
            }
            if let Some(&last) = pending.last()
                && matches!(&ops[last], Op::Write { data, .. } if data.len() > 1)
            {
                push(Variant::Torn(last), apply(&base, ops, &pending, Some(last)));
            }
        }
        images
    }
}

/// Split the indexes of the file operations in `ops` into the synced ones
/// and the pending ones.
fn split_at_syncs(ops: &[Op]) -> (Vec<usize>, Vec<usize>) {
    let mut last_sync: HashMap<&str, usize> = HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        if let Op::Sync { file } = op {
            last_sync.insert(file, i);
        }
    }
    let mut synced = Vec::new();
    let mut pending = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let file = match op {
            Op::Write { file, .. } | Op::SetLen { file, .. } => file,
            Op::Sync { .. } | Op::Mark { .. } => continue,
        };
        if last_sync.get(file.as_str()).is_some_and(|&s| i < s) {
            synced.push(i);
        } else {
            pending.push(i);
        }
    }
    (synced, pending)
}

/// Apply the operations `which` of `ops` to a copy of `files`, in order.
/// The write at index `torn` lands with only the first half of its bytes.
fn apply(
    files: &BTreeMap<String, Vec<u8>>,
    ops: &[Op],
    which: &[usize],
    torn: Option<usize>,
) -> BTreeMap<String, Vec<u8>> {
    let mut files = files.clone();
    for &i in which {
        match &ops[i] {
            Op::Write { file, offset, data } => {
                let data = if torn == Some(i) {
                    &data[..data.len() / 2]
                } else {
                    &data[..]
                };
                write_at(files.entry(file.clone()).or_default(), *offset, data);
            }
            Op::SetLen { file, len } => {
                files
                    .entry(file.clone())
                    .or_default()
                    .resize(to_usize(*len), 0);
            }
            Op::Sync { .. } | Op::Mark { .. } => {}
        }
    }
    files
}

/// Write `data` at `offset`. Like a real file, a write past the end
/// extends the file with zeros.
fn write_at(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = to_usize(offset);
    let end = start + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[start..end].copy_from_slice(data);
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).expect("the file fits in memory")
}

/// One file of a `MemVfs`. It is a redb storage backend.
#[derive(Clone, Debug)]
pub struct MemFile {
    vfs: MemVfs,
    name: String,
}

impl redb::StorageBackend for MemFile {
    fn len(&self) -> io::Result<u64> {
        let state = self.vfs.lock();
        Ok(state.files.get(&self.name).map_or(0, |f| f.len() as u64))
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let state = self.vfs.lock();
        let empty = Vec::new();
        let bytes = state.files.get(&self.name).unwrap_or(&empty);
        let start = to_usize(offset);
        let end = start + out.len();
        if end > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("read past the end of {}", self.name),
            ));
        }
        out.copy_from_slice(&bytes[start..end]);
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut state = self.vfs.lock();
        state
            .files
            .entry(self.name.clone())
            .or_default()
            .resize(to_usize(len), 0);
        state.log.push(Op::SetLen {
            file: self.name.clone(),
            len,
        });
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        self.vfs.lock().log.push(Op::Sync {
            file: self.name.clone(),
        });
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = self.vfs.lock();
        write_at(
            state.files.entry(self.name.clone()).or_default(),
            offset,
            data,
        );
        state.log.push(Op::Write {
            file: self.name.clone(),
            offset,
            data: data.to_vec(),
        });
        Ok(())
    }
}

/// Which pending operations of a crash point reached the disk.
#[derive(Clone, Copy)]
enum Variant {
    SyncedOnly,
    AllPending,
    PendingPrefix(usize),
    OnlyPending(usize),
    Torn(usize),
}

impl fmt::Display for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Variant::SyncedOnly => write!(f, "synced operations only"),
            Variant::AllPending => write!(f, "all pending operations"),
            Variant::PendingPrefix(n) => write!(f, "the first {n} pending operations"),
            Variant::OnlyPending(i) => write!(f, "only pending operation {i}"),
            Variant::Torn(i) => write!(f, "pending operations, operation {i} torn"),
        }
    }
}

/// A state that a crash can leave on disk.
pub struct CrashImage {
    point: usize,
    variant: Variant,
    marks: Vec<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl CrashImage {
    /// Return a filesystem with the files of this image and an empty
    /// record.
    pub fn vfs(&self) -> MemVfs {
        MemVfs::with_files(self.files.clone())
    }

    /// Tell if the crash came after the mark `name`.
    pub fn is_after(&self, name: &str) -> bool {
        self.marks.iter().any(|m| m == name)
    }
}

impl fmt::Debug for CrashImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "crash image after operation {} ({})",
            self.point, self.variant
        )
    }
}

#[cfg(test)]
mod tests {
    use redb::{ReadableDatabase, StorageBackend, TableDefinition};

    use super::*;

    fn contents(image: &CrashImage, name: &str) -> Vec<u8> {
        image.files.get(name).cloned().unwrap_or_default()
    }

    #[test]
    fn synced_writes_are_in_every_later_image() {
        let vfs = MemVfs::new();
        let file = vfs.file("f");
        file.write(0, b"synced").unwrap();
        file.sync_data().unwrap();
        file.write(6, b"+pending").unwrap();

        let images = vfs.crash_images();
        let after_sync: Vec<_> = images.iter().filter(|i| i.point >= 2).collect();
        assert!(
            after_sync
                .iter()
                .all(|i| contents(i, "f").starts_with(b"synced"))
        );
        assert!(after_sync.iter().any(|i| contents(i, "f") == b"synced"));
        assert!(
            after_sync
                .iter()
                .any(|i| contents(i, "f") == b"synced+pending")
        );
    }

    #[test]
    fn a_pending_write_can_land_without_an_earlier_one() {
        let vfs = MemVfs::new();
        let file = vfs.file("f");
        file.write(0, b"a").unwrap();
        file.write(1, b"b").unwrap();

        let images = vfs.crash_images();
        assert!(images.iter().any(|i| contents(i, "f") == b"\0b"));
    }

    #[test]
    fn a_torn_write_keeps_its_first_half() {
        let vfs = MemVfs::new();
        vfs.file("f").write(0, b"abcd").unwrap();

        let images = vfs.crash_images();
        assert!(images.iter().any(|i| contents(i, "f") == b"ab"));
    }

    #[test]
    fn marks_split_the_images() {
        let vfs = MemVfs::new();
        vfs.file("f").write(0, b"x").unwrap();
        vfs.mark("done");
        vfs.file("f").write(1, b"y").unwrap();

        let images = vfs.crash_images();
        assert!(
            images
                .iter()
                .filter(|i| i.point <= 1)
                .all(|i| !i.is_after("done"))
        );
        assert!(
            images
                .iter()
                .filter(|i| i.point >= 2)
                .all(|i| i.is_after("done"))
        );
    }

    #[test]
    fn an_image_starts_a_fresh_record() {
        let vfs = MemVfs::new();
        let file = vfs.file("f");
        file.write(0, b"x").unwrap();
        file.sync_data().unwrap();

        let image = vfs.crash_images().pop().unwrap();
        let reopened = image.vfs();
        assert_eq!(reopened.file("f").len().unwrap(), 1);
        assert_eq!(reopened.crash_images().len(), 1);
    }

    /// redb with immediate durability survives every crash image: the
    /// database opens, and a commit that returned is never lost.
    #[test]
    fn redb_survives_every_crash_image() {
        const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("simulation");

        // Create the database first and record only the commit. A crash
        // during the creation is a different case: see the next test.
        let created = MemVfs::new();
        drop(
            redb::Builder::new()
                .create_with_backend(created.file("db"))
                .unwrap(),
        );
        let vfs = MemVfs::with_files(created.lock().files.clone());
        {
            let db = redb::Builder::new()
                .create_with_backend(vfs.file("db"))
                .unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(TABLE)
                .unwrap()
                .insert(1, &b"value"[..])
                .unwrap();
            txn.commit().unwrap();
            vfs.mark("committed");
        }

        let images = vfs.crash_images();
        assert!(images.len() > 10);
        for image in &images {
            let db = redb::Builder::new()
                .create_with_backend(image.vfs().file("db"))
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            let txn = db.begin_read().unwrap();
            let found = match txn.open_table(TABLE) {
                Ok(table) => table.get(1).unwrap().map(|v| v.value().to_vec()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => panic!("{image:?}: {e}"),
            };
            match found {
                Some(value) => assert_eq!(value, b"value", "{image:?}"),
                None => assert!(!image.is_after("committed"), "{image:?} lost a commit"),
            }
        }
    }

    /// A crash while redb creates a database can leave a file that redb
    /// refuses to open. So the store must create its database atomically.
    #[test]
    fn a_crash_during_creation_can_leave_an_unreadable_file() {
        let vfs = MemVfs::new();
        drop(
            redb::Builder::new()
                .create_with_backend(vfs.file("db"))
                .unwrap(),
        );

        let refused = vfs.crash_images().iter().any(|image| {
            redb::Builder::new()
                .create_with_backend(image.vfs().file("db"))
                .is_err()
        });
        assert!(refused);
    }
}
