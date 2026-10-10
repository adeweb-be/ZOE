//! A directory in memory with no record of its operations.
//!
//! `zoe_core::vfs::MemVfs` keeps a copy of every write for the crash
//! simulation, so a long benchmark does not fit in memory with it. This
//! directory keeps only the current bytes of each file, in chunks of
//! 1 MiB, so a growing file is never copied.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, RwLock};

use zoe_core::vfs::Vfs;

/// The size of one chunk of a file.
const CHUNK: usize = 1 << 20;

/// The bytes of one file.
#[derive(Debug, Default)]
struct Chunks {
    len: u64,
    chunks: Vec<Box<[u8]>>,
}

impl Chunks {
    /// Make the file `len` bytes long. New bytes are zero.
    fn resize(&mut self, len: u64) {
        let count = (len as usize).div_ceil(CHUNK);
        if len < self.len {
            self.chunks.truncate(count);
            // Bytes past the new end read as zero if the file grows again.
            let tail = len as usize % CHUNK;
            if tail != 0 {
                self.chunks[count - 1][tail..].fill(0);
            }
        }
        while self.chunks.len() < count {
            self.chunks.push(vec![0; CHUNK].into_boxed_slice());
        }
        self.len = len;
    }

    /// Call `f` with each chunk slice that the range `offset..offset + n`
    /// covers, and with the position of that slice in the range.
    fn spans(offset: u64, n: usize, mut f: impl FnMut(usize, usize, std::ops::Range<usize>)) {
        let mut done = 0;
        while done < n {
            let at = offset as usize + done;
            let (chunk, from) = (at / CHUNK, at % CHUNK);
            let take = (CHUNK - from).min(n - done);
            f(chunk, done, from..from + take);
            done += take;
        }
    }
}

/// An open file of a `RamVfs`.
#[derive(Clone, Debug)]
pub struct RamFile(Arc<RwLock<Chunks>>);

impl redb::StorageBackend for RamFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.read().unwrap_or_else(|e| e.into_inner()).len)
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let file = self.0.read().unwrap_or_else(|e| e.into_inner());
        if offset + out.len() as u64 > file.len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of the file",
            ));
        }
        Chunks::spans(offset, out.len(), |chunk, at, range| {
            let n = range.len();
            out[at..at + n].copy_from_slice(&file.chunks[chunk][range]);
        });
        Ok(())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .resize(len);
        Ok(())
    }

    fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut file = self.0.write().unwrap_or_else(|e| e.into_inner());
        let end = offset + data.len() as u64;
        if end > file.len {
            file.resize(end);
        }
        Chunks::spans(offset, data.len(), |chunk, at, range| {
            let n = range.len();
            file.chunks[chunk][range].copy_from_slice(&data[at..at + n]);
        });
        Ok(())
    }
}

/// A directory in memory. Clones share the same files.
#[derive(Clone, Debug, Default)]
pub struct RamVfs {
    files: Arc<Mutex<HashMap<String, Arc<RwLock<Chunks>>>>>,
}

impl RamVfs {
    /// Return an empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the sum of the lengths of the files.
    pub fn bytes_stored(&self) -> u64 {
        let files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        files
            .values()
            .map(|f| f.read().unwrap_or_else(|e| e.into_inner()).len)
            .sum()
    }
}

impl Vfs for RamVfs {
    type File = RamFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        Ok(self
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name))
    }

    fn open(&self, name: &str) -> io::Result<RamFile> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        Ok(RamFile(Arc::clone(
            files.entry(name.to_owned()).or_default(),
        )))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        let file = files
            .remove(from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, from.to_owned()))?;
        files.insert(to.to_owned(), file);
        Ok(())
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        files
            .remove(name)
            .map(drop)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))
    }

    fn sync_dir(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use redb::StorageBackend;

    use super::*;

    #[test]
    fn writes_across_chunks_read_back() {
        let vfs = RamVfs::new();
        let file = vfs.open("f").unwrap();
        let data: Vec<u8> = (0..3 * CHUNK + 7).map(|i| i as u8).collect();
        file.write(CHUNK as u64 - 3, &data).unwrap();
        assert_eq!(file.len().unwrap(), (4 * CHUNK + 4) as u64);

        let mut out = vec![0; data.len()];
        file.read(CHUNK as u64 - 3, &mut out).unwrap();
        assert_eq!(out, data);
        let mut head = [1; 4];
        file.read(0, &mut head).unwrap();
        assert_eq!(head, [0; 4]);
        assert!(file.read(4 * CHUNK as u64, &mut [0; 5]).is_err());
    }

    #[test]
    fn a_shrunk_file_grows_back_with_zeros() {
        let vfs = RamVfs::new();
        let file = vfs.open("f").unwrap();
        file.write(0, &[7; 100]).unwrap();
        file.set_len(10).unwrap();
        file.set_len(100).unwrap();
        let mut out = [1; 100];
        file.read(0, &mut out).unwrap();
        assert_eq!(&out[..10], &[7; 10]);
        assert_eq!(&out[10..], &[0; 90]);
    }

    #[test]
    fn a_renamed_file_keeps_its_bytes() {
        let vfs = RamVfs::new();
        vfs.open("a").unwrap().write(0, b"bytes").unwrap();
        vfs.rename("a", "b").unwrap();
        assert!(!vfs.exists("a").unwrap());
        assert_eq!(vfs.open("b").unwrap().len().unwrap(), 5);
        assert_eq!(vfs.bytes_stored(), 5);
        vfs.remove("b").unwrap();
        assert!(vfs.remove("b").is_err());
    }
}
