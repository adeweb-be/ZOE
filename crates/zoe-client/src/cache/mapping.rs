//! A mapping of memory that the cache sees as a slice of atomic words.
//!
//! This module holds the only `unsafe` code of the crate: the `mmap` and
//! the `munmap` of the mapping, and the view of its bytes as `AtomicU64`.
//! Every read and write of the cache goes through these atomics.

#![allow(unsafe_code)]

use std::io;
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicU64;

/// A mapping of `words` words of 8 bytes, aligned to a page.
pub(crate) struct Mapping {
    ptr: NonNull<AtomicU64>,
    words: usize,
}

// SAFETY: the mapping holds only atomics, and no thread owns it. The
// pointer stays valid until the drop.
unsafe impl Send for Mapping {}
// SAFETY: as above. A shared reference gives only `&[AtomicU64]`.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `words` words of private anonymous memory. The kernel gives
    /// them as zeros, and gives a page only when the cache first writes it.
    pub(crate) fn anonymous(words: usize) -> io::Result<Mapping> {
        let len = words
            .checked_mul(8)
            .filter(|len| *len > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no size for the cache"))?;
        // SAFETY: a new mapping at an address that the kernel picks. It
        // overlaps no memory of the process.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
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
    pub(crate) fn words(&self) -> &[AtomicU64] {
        // SAFETY: the mapping holds `words` words of 8 bytes, aligned to a
        // page, readable and writable, until the drop. `AtomicU64` has the
        // size and the alignment of `u64`, and every bit pattern is valid.
        // The code accesses the words only as atomics.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.words) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping came from `mmap` with this address and this
        // length. No reference to its words outlives `self`.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.words * 8);
        }
    }
}
