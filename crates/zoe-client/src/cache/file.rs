//! The cache file: stage 2 of ADR 0034 (proposed). One process maps the
//! file, in the layout of the anonymous mapping.
//!
//! The open takes an exclusive `flock` of the file without a wait, so a
//! second process on the same file fails, as with the `zc.lockfile` of
//! ZEO. It reads the header page, and keeps the entries only when the
//! header is good and the file is trustworthy:
//!
//! - the magic (with the version of the layout), the size of the ring, the
//!   number of slots and the name of the storage are those of the open, and
//!   their checksum is good, and the file has the length of the layout;
//! - the last close was clean and the checksum of the state is good, or
//!   the file was not closed but the kernel did not restart since: a
//!   process that dies leaves its writes in the page cache.
//!
//! Else the open drops every entry: it makes the file zero in place, and
//! gives it the length of the layout. It makes the file shorter only when
//! the layout is smaller. The open never changes a file that does not start
//! with the magic of ZOE (or with a zero word, which an open that died
//! before its first write leaves): such a file is not a cache file of ZOE,
//! and the open fails. Each open gives the file its disk space. The open
//! then marks the file not clean, with the boot id of the kernel, and
//! writes the header page to the disk before any other write.
//!
//! A file with entries serves only after `Cache::verify`: the pool verifies
//! it against the server first.
//!
//! The close writes the whole mapping to the disk, then marks the file
//! clean with the checksum of the state, then writes the header page again.
//! Then it frees the lock: the mapping holds the open file until the unmap,
//! so only an explicit unlock frees the lock at the close. A closed cache
//! never writes its mapping again; it reads it only for `applied` and
//! `Debug`.

use std::fs::{OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

use super::mapping;
use super::{
    Cache, H_BOOT, H_CLEAN, H_HEAD, H_MAGIC, H_RING, H_SLOTS, H_STATE_SUM, H_STATIC_SUM, H_STORAGE,
    HEADER_WORDS, Layout, STATIC_WORDS, words_sum,
};

/// The first bytes of the magic of every version of the layout.
const MAGIC_PREFIX: &[u8; 7] = b"ZOECACH";

/// The file that gives the boot id of the kernel.
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// Return the boot id of the kernel as two words, or `None` when it does
/// not read.
fn boot_id() -> Option<[u64; 2]> {
    let text = std::fs::read_to_string(BOOT_ID).ok()?;
    let hex: String = text.trim().chars().filter(|c| *c != '-').collect();
    let id = u128::from_str_radix(&hex, 16).ok().filter(|id| *id != 0)?;
    Some([(id >> 64) as u64, id as u64])
}

/// Return the checksum of the state: the words 0 to 8 of the header.
fn state_sum(header: &[u64]) -> u64 {
    words_sum(&header[..H_STATE_SUM])
}

/// Tell if the header page `header` of a file of `len` bytes has the static
/// words `fixed` and a good state, so the open keeps the entries.
fn trustworthy(header: &[u64], fixed: &[u64; STATIC_WORDS], len: u64, layout: &Layout) -> bool {
    let static_good = header[H_MAGIC] == fixed[0]
        && header[H_RING] == fixed[1]
        && header[H_SLOTS] == fixed[2]
        && header[H_STORAGE..H_STORAGE + STATIC_WORDS - 3] == fixed[3..]
        && header[H_STATIC_SUM] == words_sum(fixed)
        && len == layout.words as u64 * 8;
    let head = header[H_HEAD];
    let state_good = match header[H_CLEAN] {
        1 => header[H_STATE_SUM] == state_sum(header),
        0 => boot_id().is_some_and(|boot| header[H_BOOT..H_BOOT + 2] == boot),
        _ => false,
    };
    static_good && state_good && head >= layout.ring && head.is_multiple_of(8)
}

impl Cache {
    /// Open the cache file `path` of the storage `storage`, whose ring holds
    /// `size` bytes, as `Cache::new` counts them. Create the file when it
    /// does not exist. The file holds the header, the index and the ring:
    /// its length never changes while it is open.
    ///
    /// The open fails with `io::ErrorKind::WouldBlock` when another open
    /// file holds the lock of the file, in this process or in another, with
    /// `io::ErrorKind::InvalidInput` for a bad size or a name of the storage
    /// longer than 256 bytes, and with `io::ErrorKind::InvalidData` for a
    /// file that is not a cache file of ZOE, such as the `.zec` file of ZEO:
    /// the open does not change it. Any other error is the error of the
    /// file.
    ///
    /// A file with entries does not serve until `Cache::verify`. A new
    /// file, or a file whose entries the open dropped, serves at once, with
    /// `applied` at 0.
    pub fn open(path: &Path, size: u64, storage: &[u8]) -> io::Result<Cache> {
        let layout = Layout::of(size)?;
        let fixed = layout.static_words(storage)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("another open holds the cache file {}", path.display()),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let len = file.metadata()?.len();
        if len > 0 {
            let mut magic = [0u8; 8];
            let read = file.read_exact_at(&mut magic, 0);
            if read.is_err() || (magic != [0; 8] && !magic.starts_with(MAGIC_PREFIX)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is not a cache file of the native client: the open leaves it as it is",
                        path.display()
                    ),
                ));
            }
        }
        let keep = len >= HEADER_WORDS as u64 * 8 && {
            let mut page = vec![0u8; HEADER_WORDS * 8];
            file.read_exact_at(&mut page, 0)?;
            let header: Vec<u64> = page
                .as_chunks::<8>()
                .0
                .iter()
                .map(|chunk| u64::from_le_bytes(*chunk))
                .collect();
            trustworthy(&header, &fixed, len, &layout)
        };
        let target = layout.words as u64 * 8;
        if !keep {
            // Zero in place, so a closed cache of this process that still
            // maps the file never finds its end moved. Cut the file only
            // when the file system cannot zero a range.
            if let Err(error) = mapping::zero(&file, len.min(target)) {
                if error.raw_os_error() != Some(libc::EOPNOTSUPP) {
                    return Err(error);
                }
                file.set_len(0)?;
            }
            if file.metadata()?.len() != target {
                file.set_len(target)?;
            }
        }
        // A copy can make a kept file sparse: a write through the mapping to
        // a hole of a full disk raises SIGBUS.
        mapping::allocate(&file, target)?;
        let cache = Cache::with(layout.words, &layout, Some(file))?;
        if !keep {
            cache.init(&fixed);
        }
        let boot = boot_id().unwrap_or_default();
        cache.word(H_BOOT).store(boot[0], Ordering::Relaxed);
        cache.word(H_BOOT + 1).store(boot[1], Ordering::Relaxed);
        cache.word(H_CLEAN).store(0, Ordering::Relaxed);
        cache.word(H_STATE_SUM).store(0, Ordering::Relaxed);
        cache.map.sync(HEADER_WORDS)?;
        let empty = cache.applied().0 == 0;
        cache.verified.store(empty, Ordering::Release);
        Ok(cache)
    }

    /// Write the mapping to the file, then mark the file clean. The caller
    /// holds the writer lock, or owns the cache, and holds the lock of the
    /// file. An error leaves the file not clean: the next open after a
    /// restart of the kernel drops it.
    pub(super) fn write_clean(&self) {
        if self.map.sync(self.map.words().len()).is_err() {
            return;
        }
        let mut header = [0u64; H_STATE_SUM];
        self.word(H_CLEAN).store(1, Ordering::Relaxed);
        for (i, word) in header.iter_mut().enumerate() {
            *word = self.word(i).load(Ordering::Relaxed);
        }
        self.word(H_STATE_SUM)
            .store(state_sum(&header), Ordering::Relaxed);
        let _ = self.map.sync(HEADER_WORDS);
    }
}

impl Drop for Cache {
    /// A cache file that drops without a close is closed.
    fn drop(&mut self) {
        let file = self
            .file
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if file.is_some() {
            self.write_clean();
        }
    }
}
