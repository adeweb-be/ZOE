//! A streaming reader of FileStorage files (`Data.fs`).
//!
//! The file starts with the magic `FS30`. Transaction records follow, in
//! tid order. All numbers are big-endian.
//!
//! A transaction record is a 23-byte header (tid, length, status, then the
//! lengths of the user, the description and the extension), the user, the
//! description, the extension, the data records, and the length again.
//! The length counts the bytes from the start of the header to the end of
//! the last data record.
//!
//! A data record is a 42-byte header (oid, tid, previous record, offset of
//! its transaction, a version length that is always 0, data length), then
//! the data. A record with a data length of 0 has an 8-byte back pointer
//! instead: the offset of an earlier record of the same oid that holds the
//! data. A back pointer of 0 means no data: the record undoes the creation
//! of its object.
//!
//! The reader reads the file in order through a buffer. It holds one
//! transaction header and one record at a time. It reads the record that a
//! back pointer leads to, and the redundant length, at their offsets. These
//! reads do not move the buffer.
//!
//! The reader stops where ZODB stops: before a commit in progress at the
//! end of the file, before a transaction that the end of the file cuts, and
//! before a tail that ZODB takes for the junk of a crash. ZODB cuts the
//! file at the same offset when it opens the file for writing.

use std::fs::{self, File};
use std::io::{self, BufReader, ErrorKind, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Tid};

use crate::Error;

/// The magic of a FileStorage file.
pub(crate) const MAGIC: &[u8; 4] = b"FS30";

/// The length of a transaction header.
const TXN_HEADER_LEN: u64 = 23;

/// The length of a data record header.
const DATA_HEADER_LEN: u64 = 42;

/// The length of a number: a length, an offset or a back pointer.
const NUMBER_LEN: u64 = 8;

/// Where the reader stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The last transaction ends at the end of the file.
    Clean,
    /// The last transaction of the file, at this offset, has the status
    /// `c`. It is a commit in progress, and the reader stops before it.
    InProgress(u64),
    /// The reader stops before the end of the file, at the offset `pos`.
    Cut {
        /// The offset where the reader stops.
        pos: u64,
        /// Why the reader stops there.
        cut: Cut,
    },
}

/// Why the reader stops before the end of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// The file ends inside the transaction.
    Truncated,
    /// The transaction header is not valid, and the last length of the file
    /// is not the length of a transaction after it. ZODB takes such a tail
    /// for the junk of a crash.
    Junk,
    /// The transaction has the status `c`, but it is not a whole last
    /// transaction: data follows it, or its length is not valid.
    Commit,
}

/// The header of a transaction record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TxnHeader {
    /// The tid of the transaction.
    pub(crate) tid: Tid,
    /// The status byte: a space, or `p` for a packed transaction.
    pub(crate) status: u8,
    /// The user name, as the file holds it.
    pub(crate) user: Vec<u8>,
    /// The description, as the file holds it.
    pub(crate) description: Vec<u8>,
    /// The pickled extension data, as the file holds it.
    pub(crate) extension: Vec<u8>,
    /// The offset of the transaction record.
    pos: u64,
    /// The offset of the first data record.
    records: u64,
    /// The offset after the last data record.
    end: u64,
}

/// One data record, with the data that its back pointers lead to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataRecord {
    /// The oid of the record.
    pub(crate) oid: Oid,
    /// The pickle. It is `None` when the record undoes the creation of its
    /// object.
    pub(crate) data: Option<Vec<u8>>,
}

/// A reader of the transactions of a FileStorage file.
pub(crate) struct Reader {
    path: PathBuf,
    file: BufReader<File>,
    /// The offset where the reader stops: the size of the file when it was
    /// opened, or a lower limit. The reader ignores the bytes after it.
    size: u64,
    /// The offset of the read position of `file`.
    at: u64,
    /// The offset of the next transaction record.
    next: u64,
    /// The tid of the last transaction read.
    last: Tid,
    /// Where the reader stopped. It is `None` until the reader stops.
    end: Option<End>,
}

impl Reader {
    /// Open the FileStorage file at `path`. A path that is not a regular
    /// file is refused with `Io`, before it is opened. A file that does not
    /// start with the magic `FS30` is refused with `NotDataFs`.
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let io = |source| Error::Io {
            path: path.to_owned(),
            source,
        };
        // The open of a FIFO waits for a writer, so the check comes first.
        // The second check covers a file that changes between the two.
        let not_a_file = || {
            io(io::Error::new(
                ErrorKind::InvalidInput,
                "not a regular file",
            ))
        };
        if !fs::metadata(path).map_err(io)?.is_file() {
            return Err(not_a_file());
        }
        let file = File::open(path).map_err(io)?;
        let meta = file.metadata().map_err(io)?;
        if !meta.is_file() {
            return Err(not_a_file());
        }
        let size = meta.len();
        let mut magic = Vec::with_capacity(MAGIC.len());
        (&file)
            .take(MAGIC.len() as u64)
            .read_to_end(&mut magic)
            .map_err(io)?;
        if magic != MAGIC {
            return Err(Error::NotDataFs {
                path: path.to_owned(),
                magic: magic.escape_ascii().to_string(),
            });
        }
        let start = MAGIC.len() as u64;
        Ok(Self {
            path: path.to_owned(),
            file: BufReader::with_capacity(1 << 16, file),
            size,
            at: start,
            next: start,
            last: Tid::ZERO,
            end: None,
        })
    }

    /// Open the FileStorage file at `path`, as `open` does, and read it
    /// only up to the offset `end`. The bytes after `end` are ignored.
    pub(crate) fn open_to(path: &Path, end: u64) -> Result<Self, Error> {
        let mut reader = Self::open(path)?;
        reader.size = reader.size.min(end);
        Ok(reader)
    }

    /// Return where the reader stopped, or `None` before it stops.
    pub(crate) fn end(&self) -> Option<End> {
        self.end
    }

    /// Return the offset of the next transaction record. After the reader
    /// stops, this is the offset where it stopped.
    pub(crate) fn next_offset(&self) -> u64 {
        self.next
    }

    /// Read the header of the next transaction. Return `None` at the end.
    ///
    /// The call checks the header and the redundant length before it
    /// returns. A transaction with the status `u` is skipped, as the index
    /// of ZODB skips it. The next call moves past the data records of this
    /// transaction, read or not.
    ///
    /// A header that is not valid is `Corrupt`, except in a tail that ZODB
    /// takes for junk. There the reader stops with `Cut::Junk`.
    pub(crate) fn next_txn(&mut self) -> Result<Option<TxnHeader>, Error> {
        loop {
            if self.end.is_some() {
                return Ok(None);
            }
            let pos = self.next;
            let Some(left) = self.size.checked_sub(pos) else {
                return Err(self.corrupt(pos, "offset is after the end of the file"));
            };
            if left == 0 {
                self.end = Some(End::Clean);
                return Ok(None);
            }
            if left < TXN_HEADER_LEN {
                return self.stop(pos, Cut::Truncated);
            }
            self.goto(pos)?;
            let mut header = [0; TXN_HEADER_LEN as usize];
            self.read(&mut header)?;
            let tid = Tid(number(&header[0..8]));
            let len = number(&header[8..16]);
            let status = header[16];
            let lens = [&header[17..19], &header[19..21], &header[21..23]]
                .map(|field| u64::from(u16::from_be_bytes([field[0], field[1]])));
            let header_len = TXN_HEADER_LEN + lens.iter().sum::<u64>();
            let total = len.checked_add(NUMBER_LEN);

            if status == b'c' {
                // ZODB writes the status `c` only on the last transaction.
                // A commit can still write the end of that transaction.
                let writing = total.is_none_or(|total| total > left);
                let whole = total == Some(left) && len >= header_len;
                if writing || whole {
                    self.end = Some(End::InProgress(pos));
                    return Ok(None);
                }
                return self.stop(pos, Cut::Commit);
            }
            if total.is_none_or(|total| total > left) {
                return self.stop(pos, Cut::Truncated);
            }
            let invalid = if !matches!(status, b' ' | b'p' | b'u') {
                let status = status.escape_ascii();
                Some(format!("unknown transaction status \"{status}\""))
            } else if len < header_len {
                Some(format!(
                    "transaction length {len} is shorter than its header"
                ))
            } else if tid <= self.last {
                Some(format!("tid {} is not after the tid before it", hex(tid.0)))
            } else {
                None
            };
            if let Some(reason) = invalid {
                if self.junk_from(pos)? {
                    return self.stop(pos, Cut::Junk);
                }
                return Err(self.corrupt(pos, reason));
            }
            let end = pos + len;
            let redundant = self.number_at(end)?;
            if redundant != len {
                return Err(self.corrupt(
                    end,
                    format!("redundant transaction length {redundant} is not {len}"),
                ));
            }
            self.last = tid;
            self.next = end + NUMBER_LEN;
            if status == b'u' {
                continue;
            }

            let [user, description, extension] = lens.map(|len| vec![0; len as usize]);
            let mut txn = TxnHeader {
                tid,
                status,
                user,
                description,
                extension,
                pos,
                records: pos + header_len,
                end,
            };
            self.read(&mut txn.user)?;
            self.read(&mut txn.description)?;
            self.read(&mut txn.extension)?;
            return Ok(Some(txn));
        }
    }

    /// Stop the reader at `pos` for the reason `cut`.
    fn stop(&mut self, pos: u64, cut: Cut) -> Result<Option<TxnHeader>, Error> {
        self.end = Some(End::Cut { pos, cut });
        Ok(None)
    }

    /// Tell if the bytes from `pos` to the end are junk, as ZODB decides
    /// for a header that is not valid. The last 8 bytes of the file are the
    /// length of its last transaction. The tail is junk when that length is
    /// shorter than a header, or when it leads to a transaction before
    /// `pos`. So a tail of zeros is junk.
    fn junk_from(&self, pos: u64) -> Result<bool, Error> {
        let last = self.number_at(self.size - NUMBER_LEN)?;
        Ok(last < TXN_HEADER_LEN || self.size.checked_sub(last).is_none_or(|at| at < pos))
    }

    /// Return an iterator over the data records of `txn`, which must be the
    /// transaction that `next_txn` returned last.
    pub(crate) fn records<'r>(&'r mut self, txn: &TxnHeader) -> Records<'r> {
        Records {
            reader: self,
            tid: txn.tid,
            txn: txn.pos,
            pos: txn.records,
            end: txn.end,
        }
    }

    /// Read the data record at `pos` of the transaction at `txn`, which
    /// ends at `end`. Return the record and its length.
    fn read_record(
        &mut self,
        tid: Tid,
        txn: u64,
        pos: u64,
        end: u64,
    ) -> Result<(DataRecord, u64), Error> {
        if end - pos < DATA_HEADER_LEN {
            return Err(self.corrupt(pos, "data record header crosses the transaction end"));
        }
        self.goto(pos)?;
        let mut header = [0; DATA_HEADER_LEN as usize];
        self.read(&mut header)?;
        let (oid, len) = self.check_data_header(&header, pos)?;
        if number(&header[8..16]) != tid.0 {
            return Err(self.corrupt(pos, "data record tid is not the transaction tid"));
        }
        if number(&header[24..32]) != txn {
            return Err(self.corrupt(pos, "data record does not point to its transaction"));
        }
        let room = end - pos - DATA_HEADER_LEN;
        if len > 0 {
            if len > room {
                return Err(self.corrupt(pos, "data record crosses the transaction end"));
            }
            let mut data = self.buffer(len, pos)?;
            (&mut self.file)
                .take(len)
                .read_to_end(&mut data)
                .map_err(|source| self.io(source))?;
            if data.len() as u64 != len {
                return Err(self.io(ErrorKind::UnexpectedEof.into()));
            }
            self.at += len;
            let record = DataRecord {
                oid,
                data: Some(data),
            };
            return Ok((record, DATA_HEADER_LEN + len));
        }
        if room < NUMBER_LEN {
            return Err(self.corrupt(pos, "back pointer crosses the transaction end"));
        }
        let mut back = [0; NUMBER_LEN as usize];
        self.read(&mut back)?;
        let data = self.load_back(oid, number(&back), pos)?;
        Ok((DataRecord { oid, data }, DATA_HEADER_LEN + NUMBER_LEN))
    }

    /// Follow the back pointer `back` of the record of `oid` at `record`
    /// to the data. A chain of back pointers is followed to its end. A
    /// back pointer of 0 gives `None`.
    fn load_back(&self, oid: Oid, back: u64, record: u64) -> Result<Option<Vec<u8>>, Error> {
        let Some((pos, len)) = self.chain_end(oid, back, record)? else {
            return Ok(None);
        };
        let mut data = self.buffer(len, pos)?;
        let mut source = ReadAt {
            file: self.file.get_ref(),
            pos: pos + DATA_HEADER_LEN,
        };
        source
            .by_ref()
            .take(len)
            .read_to_end(&mut data)
            .map_err(|source| self.io(source))?;
        if data.len() as u64 != len {
            return Err(self.io(ErrorKind::UnexpectedEof.into()));
        }
        Ok(Some(data))
    }

    /// Return the end of the chain of back pointers that starts with the
    /// back pointer `back` of the record of `oid` at `from`: the offset of
    /// the record with the data and its length, or `None` for no data.
    ///
    /// Each back pointer must point before the record that holds it, so
    /// the chain always ends.
    fn chain_end(
        &self,
        oid: Oid,
        mut back: u64,
        mut from: u64,
    ) -> Result<Option<(u64, u64)>, Error> {
        loop {
            if back == 0 {
                return Ok(None);
            }
            let start = MAGIC.len() as u64 + TXN_HEADER_LEN;
            if back < start || back >= from || from - back < DATA_HEADER_LEN {
                return Err(self.corrupt(from, format!("back pointer {back} is out of range")));
            }
            let mut header = [0; DATA_HEADER_LEN as usize];
            self.read_at(&mut header, back)?;
            let (target, len) = self.check_data_header(&header, back)?;
            if target != oid {
                return Err(self.corrupt(
                    from,
                    format!("back pointer {back} leads to oid {}", hex(target.0)),
                ));
            }
            let room = from - back - DATA_HEADER_LEN;
            if len > 0 {
                if len > room {
                    return Err(self.corrupt(back, "data record crosses the record after it"));
                }
                return Ok(Some((back, len)));
            }
            if room < NUMBER_LEN {
                return Err(self.corrupt(back, "back pointer crosses the record after it"));
            }
            let mut next = [0; NUMBER_LEN as usize];
            self.read_at(&mut next, back + DATA_HEADER_LEN)?;
            from = back;
            back = number(&next);
        }
    }

    /// Return an empty buffer with room for `len` bytes of the record at
    /// `pos`. A length that the memory cannot hold is `Io` with
    /// `OutOfMemory`, not an abort of the process.
    fn buffer(&self, len: u64, pos: u64) -> Result<Vec<u8>, Error> {
        buffer(len).ok_or_else(|| {
            let message = format!("the data record at offset {pos} needs {len} bytes of memory");
            self.io(io::Error::new(ErrorKind::OutOfMemory, message))
        })
    }

    /// Check the version length of the data record header `header` at
    /// `pos`, and return its oid and its data length.
    fn check_data_header(&self, header: &[u8], pos: u64) -> Result<(Oid, u64), Error> {
        if header[32..34] != [0, 0] {
            return Err(self.corrupt(pos, "data record has version data"));
        }
        Ok((Oid(number(&header[0..8])), number(&header[34..42])))
    }

    /// Move the read position of the buffer to `pos`.
    fn goto(&mut self, pos: u64) -> Result<(), Error> {
        if pos != self.at {
            // The difference of two offsets in the file fits in an i64.
            let offset = pos.wrapping_sub(self.at) as i64;
            self.file
                .seek_relative(offset)
                .map_err(|source| self.io(source))?;
            self.at = pos;
        }
        Ok(())
    }

    /// Fill `buf` from the read position of the buffer.
    fn read(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.file
            .read_exact(buf)
            .map_err(|source| self.io(source))?;
        self.at += buf.len() as u64;
        Ok(())
    }

    /// Fill `buf` from the offset `pos`. The read position of the buffer
    /// does not move.
    fn read_at(&self, buf: &mut [u8], pos: u64) -> Result<(), Error> {
        self.file
            .get_ref()
            .read_exact_at(buf, pos)
            .map_err(|source| self.io(source))
    }

    /// Read the number at the offset `pos`.
    fn number_at(&self, pos: u64) -> Result<u64, Error> {
        let mut buf = [0; NUMBER_LEN as usize];
        self.read_at(&mut buf, pos)?;
        Ok(number(&buf))
    }

    fn io(&self, source: std::io::Error) -> Error {
        Error::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn corrupt(&self, pos: u64, reason: impl Into<String>) -> Error {
        Error::Corrupt {
            path: self.path.clone(),
            pos,
            reason: reason.into(),
        }
    }
}

/// The data records of one transaction, in file order.
pub(crate) struct Records<'r> {
    reader: &'r mut Reader,
    tid: Tid,
    /// The offset of the transaction record.
    txn: u64,
    /// The offset of the next data record.
    pos: u64,
    /// The offset after the last data record.
    end: u64,
}

impl Iterator for Records<'_> {
    type Item = Result<DataRecord, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end {
            return None;
        }
        match self
            .reader
            .read_record(self.tid, self.txn, self.pos, self.end)
        {
            Ok((record, len)) => {
                self.pos += len;
                Some(Ok(record))
            }
            Err(error) => {
                // A record that does not read ends the iteration.
                self.pos = self.end;
                Some(Err(error))
            }
        }
    }
}

/// Return an empty buffer with room for `len` bytes, or `None` when the
/// memory cannot hold them.
fn buffer(len: u64) -> Option<Vec<u8>> {
    let len = usize::try_from(len).ok()?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).ok()?;
    Some(buf)
}

/// A reader of a file from an offset, with positioned reads.
struct ReadAt<'f> {
    file: &'f File,
    pos: u64,
}

impl Read for ReadAt<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Decode an 8-byte big-endian number.
fn number(bytes: &[u8]) -> u64 {
    let mut buf = [0; NUMBER_LEN as usize];
    buf.copy_from_slice(bytes);
    u64::from_be_bytes(buf)
}

/// Write an oid or a tid as 16 hex digits.
pub(crate) fn hex(n: u64) -> String {
    format!("0x{n:016x}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The data of one record that `FileBuilder::txn` writes.
    pub(crate) enum Data<'a> {
        /// The record holds these bytes.
        Bytes(&'a [u8]),
        /// The record holds a back pointer to this offset.
        Back(u64),
    }

    /// Writes the bytes of a FileStorage file.
    pub(crate) struct FileBuilder {
        pub(crate) bytes: Vec<u8>,
    }

    impl FileBuilder {
        pub(crate) fn new() -> Self {
            Self {
                bytes: MAGIC.to_vec(),
            }
        }

        /// Write a transaction with the status `status` and the records
        /// `records`. Return the offset of each record.
        pub(crate) fn txn(
            &mut self,
            tid: u64,
            status: u8,
            meta: [&[u8]; 3],
            records: &[(u64, Data<'_>)],
        ) -> Vec<u64> {
            let pos = self.bytes.len() as u64;
            let mut body = Vec::new();
            for field in meta {
                body.extend_from_slice(field);
            }
            let mut offsets = Vec::new();
            for (oid, data) in records {
                offsets.push(pos + TXN_HEADER_LEN + body.len() as u64);
                body.extend_from_slice(&oid.to_be_bytes());
                body.extend_from_slice(&tid.to_be_bytes());
                body.extend_from_slice(&0u64.to_be_bytes());
                body.extend_from_slice(&pos.to_be_bytes());
                body.extend_from_slice(&[0, 0]);
                match data {
                    Data::Bytes(bytes) => {
                        body.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                        body.extend_from_slice(bytes);
                    }
                    Data::Back(back) => {
                        body.extend_from_slice(&0u64.to_be_bytes());
                        body.extend_from_slice(&back.to_be_bytes());
                    }
                }
            }
            let len = TXN_HEADER_LEN + body.len() as u64;
            self.bytes.extend_from_slice(&tid.to_be_bytes());
            self.bytes.extend_from_slice(&len.to_be_bytes());
            self.bytes.push(status);
            for field in meta {
                self.bytes
                    .extend_from_slice(&(field.len() as u16).to_be_bytes());
            }
            self.bytes.extend_from_slice(&body);
            self.bytes.extend_from_slice(&len.to_be_bytes());
            offsets
        }

        /// Write the file to a new directory for the test `name`, and
        /// return its path.
        pub(crate) fn write(&self, name: &str) -> PathBuf {
            let dir = test_dir(name);
            let path = dir.join("Data.fs");
            std::fs::write(&path, &self.bytes).unwrap();
            path
        }
    }

    /// Return a new, empty directory for the test `name`.
    pub(crate) fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-convert-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The transactions of a file, each with its records.
    type Txns = Vec<(TxnHeader, Vec<DataRecord>)>;

    /// Read every transaction of the file at `path` with its records.
    fn read_all(path: &Path) -> Result<(Txns, End), Error> {
        let mut reader = Reader::open(path)?;
        let mut txns = Vec::new();
        while let Some(txn) = reader.next_txn()? {
            let records = reader.records(&txn).collect::<Result<_, _>>()?;
            txns.push((txn, records));
        }
        Ok((txns, reader.end().unwrap()))
    }

    fn record(oid: u64, data: Option<&[u8]>) -> DataRecord {
        DataRecord {
            oid: Oid(oid),
            data: data.map(<[u8]>::to_vec),
        }
    }

    fn corrupt_at(result: Result<impl std::fmt::Debug, Error>) -> u64 {
        match result {
            Err(Error::Corrupt { pos, .. }) => pos,
            other => panic!("not a corrupt file: {other:?}"),
        }
    }

    #[test]
    fn transactions_and_records_read_in_file_order() {
        let mut file = FileBuilder::new();
        let ext: &[u8] = b"\x80\x03}q\x00.";
        file.txn(
            10,
            b'p',
            [b"user", b"first", ext],
            &[(1, Data::Bytes(b"a")), (2, Data::Bytes(b"b"))],
        );
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"c"))]);
        let (txns, end) = read_all(&file.write("order")).unwrap();

        assert_eq!(end, End::Clean);
        let (first, records) = &txns[0];
        assert_eq!((first.tid, first.status), (Tid(10), b'p'));
        assert_eq!(first.user, b"user");
        assert_eq!(first.description, b"first");
        assert_eq!(first.extension, ext);
        assert_eq!(records, &[record(1, Some(b"a")), record(2, Some(b"b"))]);
        let (second, records) = &txns[1];
        assert_eq!(second.tid, Tid(20));
        assert!(second.user.is_empty() && second.description.is_empty());
        assert!(second.extension.is_empty());
        assert_eq!(records, &[record(1, Some(b"c"))]);
    }

    #[test]
    fn back_pointers_lead_to_the_data_or_to_none() {
        let mut file = FileBuilder::new();
        let first = file.txn(
            10,
            b' ',
            [b"", b"", b""],
            &[(1, Data::Bytes(b"one")), (2, Data::Bytes(b"two"))],
        );
        let second = file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Back(first[0]))]);
        file.txn(
            30,
            b' ',
            [b"", b"", b""],
            &[(1, Data::Back(second[0])), (2, Data::Back(0))],
        );
        let (txns, _) = read_all(&file.write("back")).unwrap();

        assert_eq!(txns[1].1, [record(1, Some(b"one"))]);
        // A chain of two back pointers, and a back pointer to no data.
        assert_eq!(txns[2].1, [record(1, Some(b"one")), record(2, None)]);
    }

    #[test]
    fn a_back_pointer_to_an_undone_creation_gives_none() {
        let mut file = FileBuilder::new();
        let undo = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Back(0))]);
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Back(undo[0]))]);
        let (txns, _) = read_all(&file.write("back-none")).unwrap();
        assert_eq!(txns[1].1, [record(1, None)]);
    }

    #[test]
    fn a_bad_back_pointer_is_refused() {
        for (name, back) in [("forward", 1 << 20), ("short", 3)] {
            let mut file = FileBuilder::new();
            let at = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Back(back))]);
            let path = file.write(&format!("bad-back-{name}"));
            assert_eq!(corrupt_at(read_all(&path)), at[0], "{name}");
        }

        // A back pointer to a record of another oid.
        let mut file = FileBuilder::new();
        let first = file.txn(10, b' ', [b"", b"", b""], &[(2, Data::Bytes(b"two"))]);
        let at = file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Back(first[0]))]);
        assert_eq!(corrupt_at(read_all(&file.write("bad-back-oid"))), at[0]);
    }

    #[test]
    fn a_file_with_another_magic_is_refused() {
        let dir = test_dir("magic");
        for (name, bytes) in [
            ("old", &b"FS21"[..]),
            ("short", &b"FS"[..]),
            ("empty", &b""[..]),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let error = Reader::open(&path).err().unwrap();
            assert!(matches!(error, Error::NotDataFs { .. }), "{error:?}");
        }
    }

    #[test]
    fn a_file_with_only_the_magic_has_no_transactions() {
        let (txns, end) = read_all(&FileBuilder::new().write("only-magic")).unwrap();
        assert!(txns.is_empty());
        assert_eq!(end, End::Clean);
    }

    #[test]
    fn a_wrong_redundant_length_is_refused() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let last = file.bytes.len() - 1;
        file.bytes[last] ^= 1;
        let end = last as u64 + 1 - NUMBER_LEN;
        assert_eq!(corrupt_at(read_all(&file.write("redundant"))), end);
    }

    #[test]
    fn the_reader_stops_before_a_truncated_or_unfinished_transaction() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);

        for cut in [1, 10, 30] {
            let mut cut_file = FileBuilder {
                bytes: file.bytes.clone(),
            };
            cut_file.bytes.truncate(file.bytes.len() - cut);
            let (txns, end) = read_all(&cut_file.write("truncated")).unwrap();
            assert_eq!(txns.len(), 1, "{cut}");
            assert_eq!(
                end,
                End::Cut {
                    pos,
                    cut: Cut::Truncated
                },
                "{cut}"
            );
        }

        file.bytes[pos as usize + 16] = b'c';
        let (txns, end) = read_all(&file.write("in-progress")).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(end, End::InProgress(pos));
    }

    #[test]
    fn a_transaction_with_the_status_u_is_skipped() {
        let mut file = FileBuilder::new();
        file.txn(10, b'u', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        let (txns, _) = read_all(&file.write("status-u")).unwrap();
        let tids: Vec<_> = txns.iter().map(|(txn, _)| txn.tid).collect();
        assert_eq!(tids, [Tid(20)]);
    }

    #[test]
    fn an_unknown_status_is_refused() {
        let mut file = FileBuilder::new();
        file.txn(10, b'x', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        assert_eq!(corrupt_at(read_all(&file.write("status-x"))), 4);
    }

    #[test]
    fn tids_must_increase() {
        let mut file = FileBuilder::new();
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        assert_eq!(corrupt_at(read_all(&file.write("tid-order"))), pos);
    }

    #[test]
    fn a_record_that_breaks_its_transaction_is_refused() {
        // A data length that crosses the end of the transaction.
        let mut file = FileBuilder::new();
        let at = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"abc"))]);
        let len = at[0] as usize + 34;
        file.bytes[len + 7] = 200;
        assert_eq!(corrupt_at(read_all(&file.write("record-len"))), at[0]);

        // Version data.
        let mut file = FileBuilder::new();
        let at = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"abc"))]);
        file.bytes[at[0] as usize + 33] = 1;
        assert_eq!(corrupt_at(read_all(&file.write("version"))), at[0]);

        // A record that points to another transaction.
        let mut file = FileBuilder::new();
        let at = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"abc"))]);
        file.bytes[at[0] as usize + 31] ^= 1;
        assert_eq!(corrupt_at(read_all(&file.write("tloc"))), at[0]);

        // A record with the tid of another transaction.
        let mut file = FileBuilder::new();
        let at = file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"abc"))]);
        file.bytes[at[0] as usize + 15] ^= 1;
        assert_eq!(corrupt_at(read_all(&file.write("record-tid"))), at[0]);
    }

    #[test]
    fn a_transaction_shorter_than_its_header_is_refused() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"user", b"", b""], &[]);
        // The user length grows past the transaction length.
        file.bytes[4 + 18] = 200;
        assert_eq!(corrupt_at(read_all(&file.write("short-txn"))), 4);
    }

    #[test]
    fn a_commit_status_before_the_last_transaction_stops_the_reader() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        file.txn(30, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"c"))]);
        file.bytes[pos as usize + 16] = b'c';
        let (txns, end) = read_all(&file.write("commit-before-last")).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(
            end,
            End::Cut {
                pos,
                cut: Cut::Commit
            }
        );
    }

    #[test]
    fn a_commit_status_with_a_wrong_length_stops_the_reader() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b'c', [b"user", b"", b""], &[(1, Data::Bytes(b"b"))]);
        // The user length grows past the transaction length.
        file.bytes[pos as usize + 18] = 200;
        let (txns, end) = read_all(&file.write("commit-wrong-length")).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(
            end,
            End::Cut {
                pos,
                cut: Cut::Commit
            }
        );
    }

    #[test]
    fn a_commit_that_is_still_written_is_in_progress() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b'c', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        file.bytes.truncate(file.bytes.len() - 5);
        let (txns, end) = read_all(&file.write("commit-written")).unwrap();
        assert_eq!(txns.len(), 1);
        assert_eq!(end, End::InProgress(pos));
    }

    #[test]
    fn a_junk_tail_stops_the_reader() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        let pos = file.bytes.len() as u64;

        // A tail of zeros.
        let mut zeros = FileBuilder {
            bytes: file.bytes.clone(),
        };
        zeros.bytes.resize(zeros.bytes.len() + 64, 0);
        // A header with a short length, and a last length that is too
        // short for a transaction.
        let mut short = FileBuilder {
            bytes: file.bytes.clone(),
        };
        short.bytes.extend_from_slice(&30u64.to_be_bytes());
        short.bytes.extend_from_slice(&5u64.to_be_bytes());
        short.bytes.extend_from_slice(b" \0\0\0\0\0\0");
        short.bytes.extend_from_slice(&9u64.to_be_bytes());
        // A header with an unknown status, and a last length that leads
        // before the start of the file.
        let mut long = FileBuilder {
            bytes: file.bytes.clone(),
        };
        long.bytes.extend_from_slice(&40u64.to_be_bytes());
        long.bytes.extend_from_slice(&TXN_HEADER_LEN.to_be_bytes());
        long.bytes.extend_from_slice(&[0xff, 0, 0, 0, 0, 0, 0]);
        long.bytes.extend_from_slice(&u64::MAX.to_be_bytes());

        for (name, junk) in [("zeros", zeros), ("short", short), ("long", long)] {
            let (txns, end) = read_all(&junk.write(&format!("junk-{name}"))).unwrap();
            assert_eq!(txns.len(), 2, "{name}");
            assert_eq!(
                end,
                End::Cut {
                    pos,
                    cut: Cut::Junk
                },
                "{name}"
            );
        }
    }

    #[test]
    fn a_bad_header_before_a_valid_last_transaction_is_corrupt() {
        let mut file = FileBuilder::new();
        file.txn(10, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"a"))]);
        let pos = file.bytes.len() as u64;
        file.txn(20, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"b"))]);
        file.txn(30, b' ', [b"", b"", b""], &[(1, Data::Bytes(b"c"))]);
        file.bytes[pos as usize + 16] = 0;
        let error = read_all(&file.write("bad-header-middle")).unwrap_err();
        let Error::Corrupt {
            pos: at, reason, ..
        } = error
        else {
            panic!("not a corrupt file: {error:?}");
        };
        assert_eq!(at, pos);
        assert_eq!(reason, r#"unknown transaction status "\x00""#);
    }

    #[test]
    fn a_path_that_is_not_a_regular_file_is_refused() {
        let dir = test_dir("not-regular");
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        // The open of a FIFO without a writer waits forever, so a hang
        // here means that the check is missing.
        for path in [fifo, dir, PathBuf::from("/dev/null")] {
            let error = Reader::open(&path).err().unwrap();
            let Error::Io { source, .. } = &error else {
                panic!("not an I/O error: {error:?}");
            };
            assert_eq!(source.kind(), ErrorKind::InvalidInput, "{error:?}");
        }
    }

    #[test]
    fn a_length_that_the_memory_cannot_hold_gives_no_buffer() {
        assert!(buffer(u64::MAX).is_none());
        assert!(buffer(isize::MAX as u64 + 1).is_none());
        assert!(buffer(16).unwrap().capacity() >= 16);
    }

    /// Write a sparse file whose one data record claims 1 TiB, and return
    /// its path. Return `None` when the system cannot make such a file.
    pub(crate) fn sparse_data_fs(name: &str) -> Option<PathBuf> {
        let len: u64 = 1 << 40;
        let txn_len = TXN_HEADER_LEN + DATA_HEADER_LEN + len;
        let mut head = MAGIC.to_vec();
        head.extend_from_slice(&10u64.to_be_bytes());
        head.extend_from_slice(&txn_len.to_be_bytes());
        head.extend_from_slice(&[b' ', 0, 0, 0, 0, 0, 0]);
        head.extend_from_slice(&1u64.to_be_bytes());
        head.extend_from_slice(&10u64.to_be_bytes());
        head.extend_from_slice(&0u64.to_be_bytes());
        head.extend_from_slice(&4u64.to_be_bytes());
        head.extend_from_slice(&[0, 0]);
        head.extend_from_slice(&len.to_be_bytes());

        let path = test_dir(name).join("Data.fs");
        std::fs::write(&path, &head).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let size = MAGIC.len() as u64 + txn_len + NUMBER_LEN;
        file.set_len(size).ok()?;
        file.write_all_at(&txn_len.to_be_bytes(), size - NUMBER_LEN)
            .ok()?;
        Some(path)
    }

    /// Tell if the system refuses an allocation that is larger than its
    /// memory. With the overcommit mode 1, it does not, and a test that
    /// reads a sparse record would fill the memory.
    pub(crate) fn refuses_huge_allocations() -> bool {
        std::fs::read_to_string("/proc/sys/vm/overcommit_memory")
            .is_ok_and(|mode| mode.trim() != "1")
    }

    #[test]
    fn a_record_longer_than_the_memory_is_an_error() {
        if !refuses_huge_allocations() {
            return;
        }
        let Some(path) = sparse_data_fs("sparse-reader") else {
            return;
        };
        let mut reader = Reader::open(&path).unwrap();
        let txn = reader.next_txn().unwrap().unwrap();
        let error = reader.records(&txn).next().unwrap().unwrap_err();
        std::fs::remove_file(&path).unwrap();
        let Error::Io { source, .. } = &error else {
            panic!("not an I/O error: {error:?}");
        };
        assert_eq!(source.kind(), ErrorKind::OutOfMemory, "{error:?}");
    }

    #[test]
    fn the_fixture_reads_to_its_end() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data-fs/Data.fs");
        let (txns, end) = read_all(&path).unwrap();
        assert_eq!(end, End::Clean);
        assert!(txns.iter().any(|(txn, _)| txn.status == b'p'));
        let undone = txns.last().unwrap().1.iter().any(|r| r.data.is_none());
        assert!(undone);
    }
}
