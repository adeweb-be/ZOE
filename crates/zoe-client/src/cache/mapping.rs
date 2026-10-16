//! A mapping of memory that the cache sees as a slice of atomic words.
//!
//! This module holds the only `unsafe` code of the crate: the `mmap`, the
//! `msync` and the `munmap` of the mapping, the view of its bytes as
//! `AtomicU64`, the `fallocate` and `posix_fallocate` of a cache file on
//! Linux, and its `lseek` and the boot session of the kernel on macOS.
//! Every read and
//! write of the cache goes through these atomics. Each item with `unsafe`
//! code allows it on its own.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
#[cfg(not(target_os = "linux"))]
use std::os::unix::fs::FileExt;
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicU64;

/// A mapping of `words` words of 8 bytes, aligned to a page.
pub(crate) struct Mapping {
    ptr: NonNull<AtomicU64>,
    words: usize,
}

// SAFETY: the mapping holds only atomics, and no thread owns it. The
// pointer stays valid until the drop.
#[allow(unsafe_code)]
unsafe impl Send for Mapping {}
// SAFETY: as above. A shared reference gives only `&[AtomicU64]`.
#[allow(unsafe_code)]
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `words` words of private anonymous memory. The kernel gives
    /// them as zeros, and gives a page only when the cache first writes it.
    pub(crate) fn anonymous(words: usize) -> io::Result<Mapping> {
        Mapping::map(words, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1)
    }

    /// Map the first `words` words of `file`, shared: the writes go to the
    /// file. The file must hold at least `words` words, and no process may
    /// make it shorter while the mapping lives.
    pub(crate) fn shared(file: &File, words: usize) -> io::Result<Mapping> {
        Mapping::map(words, libc::MAP_SHARED, file.as_raw_fd())
    }

    #[allow(unsafe_code)]
    fn map(words: usize, flags: libc::c_int, fd: libc::c_int) -> io::Result<Mapping> {
        let len = words
            .checked_mul(8)
            .filter(|len| *len > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no size for the cache"))?;
        // SAFETY: a new mapping at an address that the kernel picks. It
        // overlaps no memory of the process. A file descriptor that is not
        // valid makes `mmap` fail. A shared mapping of a file has two more
        // preconditions. The file holds `len` bytes while the mapping
        // lives: an access past its end raises SIGBUS, which kills the
        // process but is not undefined behavior. The `flock` of the cache
        // keeps the other clients of ZOE out, and the cut of the file by
        // another program is a known limit. And every writer of the file
        // writes through atomics: the writers of this crate do, and other
        // programs are outside this contract.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast::<AtomicU64>())
            .ok_or_else(|| io::Error::other("mmap gave a null address"))?;
        Ok(Mapping { ptr, words })
    }

    /// Return the words of the mapping.
    #[allow(unsafe_code)]
    pub(crate) fn words(&self) -> &[AtomicU64] {
        // SAFETY: the mapping holds `words` words of 8 bytes, aligned to a
        // page, readable and writable, until the drop. Its length is at
        // most `isize::MAX` bytes: `mmap` gave a mapping of that length.
        // `AtomicU64` has the size and the alignment of `u64`, and every bit
        // pattern is valid. The code accesses the words only as atomics,
        // also when another mapping of the same file writes them.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.words) }
    }

    /// Write the first `words` words of a shared mapping to its file, and
    /// wait for the end of the write.
    #[allow(unsafe_code)]
    pub(crate) fn sync(&self, words: usize) -> io::Result<()> {
        let len = words.min(self.words) * 8;
        // SAFETY: the range starts at the address of the mapping, which is
        // aligned to a page, and stays inside it. `msync` reads no memory
        // that Rust owns.
        let synced = unsafe { libc::msync(self.ptr.as_ptr().cast(), len, libc::MS_SYNC) };
        if synced != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Give `file` the disk space of its first `len` bytes, so a write through a
/// shared mapping never finds the disk full.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn allocate(file: &File, len: u64) -> io::Result<()> {
    let len = libc::off_t::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the cache is too large"))?;
    // SAFETY: `posix_fallocate` takes a file descriptor and two integers. A
    // descriptor that is not valid makes it fail.
    let error = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    Ok(())
}

/// Make the first `len` bytes of `file` zero, in place: the length of the
/// file does not change, so a mapping of it never finds its end moved.
/// Without `FALLOC_FL_ZERO_RANGE` in the file system, punch a hole that
/// keeps the size. Give the disk space back with `allocate` after it.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn zero(file: &File, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let len = libc::off_t::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the cache is too large"))?;
    for mode in [
        libc::FALLOC_FL_ZERO_RANGE,
        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
    ] {
        // SAFETY: `fallocate` takes a file descriptor and three integers. A
        // descriptor that is not valid makes it fail.
        if unsafe { libc::fallocate(file.as_raw_fd(), mode, 0, len) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EOPNOTSUPP) {
            return Err(error);
        }
    }
    Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
}

/// Give `file` the disk space of its first `len` bytes, so a write through a
/// shared mapping never finds the disk full. macOS has no `posix_fallocate`:
/// write zeros into each hole of the range. The data of the file does not
/// change.
#[cfg(not(target_os = "linux"))]
pub(crate) fn allocate(file: &File, len: u64) -> io::Result<()> {
    if file.metadata()?.len() < len {
        file.set_len(len)?;
    }
    let mut at = 0;
    while at < len {
        let hole = seek(file, at, libc::SEEK_HOLE)?.min(len);
        if hole >= len {
            break;
        }
        let data = match seek(file, hole, libc::SEEK_DATA) {
            Ok(data) => data.min(len),
            // No data after the hole: the hole goes to the end of the file.
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => len,
            Err(error) => return Err(error),
        };
        write_zeros(file, hole, data)?;
        at = data;
    }
    Ok(())
}

/// Make the first `len` bytes of `file` zero, in place: the length of the
/// file does not change, so a mapping of it never finds its end moved.
/// macOS has no `fallocate`: write the zeros.
#[cfg(not(target_os = "linux"))]
pub(crate) fn zero(file: &File, len: u64) -> io::Result<()> {
    write_zeros(file, 0, len)
}

/// Write zeros to the bytes of `file` from `start` to `end`.
#[cfg(not(target_os = "linux"))]
fn write_zeros(file: &File, start: u64, end: u64) -> io::Result<()> {
    const CHUNK: u64 = 1 << 20;
    let zeros = vec![0u8; CHUNK.min(end.saturating_sub(start)) as usize];
    let mut at = start;
    while at < end {
        let n = CHUNK.min(end - at) as usize;
        file.write_all_at(&zeros[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

/// Return the offset of the next hole or the next data of `file` at or
/// after `offset`, as `whence` asks. A file system without holes gives the
/// end of the file as the next hole.
#[cfg(not(target_os = "linux"))]
#[allow(unsafe_code)]
fn seek(file: &File, offset: u64, whence: libc::c_int) -> io::Result<u64> {
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the cache is too large"))?;
    // SAFETY: `lseek` takes a file descriptor and two integers. A descriptor
    // that is not valid makes it fail. The cache reads and writes its file
    // only at explicit offsets, so the move of the offset of the file
    // changes nothing else.
    let found = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if found < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(found as u64)
}

/// Return the boot session UUID of the kernel as text, or `None` when the
/// kernel does not give it.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn boot_session() -> Option<String> {
    let mut buffer = [0u8; 64];
    let mut len = buffer.len();
    // SAFETY: the name is a C string. The buffer holds `len` writable
    // bytes: `sysctlbyname` writes at most `len` bytes into it, and then
    // stores the number of bytes that it wrote in `len`. It reads no new
    // value, so the last two arguments are null and 0.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let text = buffer.get(..len)?;
    let text = text.strip_suffix(&[0]).unwrap_or(text);
    String::from_utf8(text.to_vec()).ok()
}

impl Drop for Mapping {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: the mapping came from `mmap` with this address and this
        // length. No reference to its words outlives `self`.
        let unmapped = unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.words * 8) };
        debug_assert_eq!(unmapped, 0, "munmap of the cache failed");
    }
}
