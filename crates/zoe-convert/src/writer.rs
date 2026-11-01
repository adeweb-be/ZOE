//! A writer of FileStorage files (`Data.fs`).
//!
//! The writer writes the format that `filestorage.rs` reads, as
//! FileStorage's `restore` writes it. Each transaction record has the
//! status ` `. Each data record holds its data. A deletion is a data record
//! with a data length of 0 and a back pointer of 0. The writer writes no
//! other back pointer.
//!
//! The previous-record offset of a data record is the offset of the newest
//! record of its oid before it, or 0. The writer keeps that offset for each
//! oid, as the index of FileStorage does. It holds one transaction at a
//! time.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Tid, TxnInfo};

use crate::Error;
use crate::filestorage::{Cut, DATA_HEADER_LEN, End, MAGIC, NUMBER_LEN, Reader, TXN_HEADER_LEN};
use crate::fsutil::io_error;

/// What a writer needs to append to an existing file.
#[derive(Debug)]
pub(crate) struct Tail {
    /// The offset after the last transaction record: the size of the file.
    pub(crate) end: u64,
    /// The tid of the last transaction. It is `Tid::ZERO` when the file
    /// has no transaction.
    pub(crate) last: Tid,
    /// The offset of the newest data record of each oid.
    newest: HashMap<Oid, u64>,
}

/// Read the FileStorage file at `path` to its end, and return its tail.
///
/// The file must end with a whole transaction. A file that ends inside a
/// transaction, with a commit in progress or with junk fails with
/// `Corrupt`, because ZODB cuts such a file when it opens it for writing.
/// The other failures are those of the reader.
pub(crate) fn tail(path: &Path) -> Result<Tail, Error> {
    let mut reader = Reader::open(path)?;
    let mut newest = HashMap::new();
    let mut last = Tid::ZERO;
    while let Some(txn) = reader.next_txn()? {
        let mut records = reader.records(&txn);
        loop {
            let pos = records.offset();
            let Some(record) = records.next() else {
                break;
            };
            newest.insert(record?.oid, pos);
        }
        last = txn.tid;
    }
    let (pos, what) = match reader.end() {
        Some(End::Clean) | None => {
            return Ok(Tail {
                end: reader.next_offset(),
                last,
                newest,
            });
        }
        Some(End::InProgress(pos)) => (pos, "a commit in progress starts here"),
        Some(End::Cut { pos, cut }) => match cut {
            Cut::Truncated => (pos, "the file ends inside this transaction"),
            Cut::Junk => (pos, "the file holds no valid transaction from here"),
            Cut::Commit => (pos, "this transaction has the status c"),
        },
    };
    Err(Error::Corrupt {
        path: path.to_owned(),
        pos,
        reason: format!("{what}, so the converter does not append to the file"),
    })
}

/// A writer that appends transaction records to a FileStorage file.
pub(crate) struct Writer {
    path: PathBuf,
    file: BufWriter<File>,
    /// The offset of the next transaction record.
    pos: u64,
    /// The offset of the newest data record of each oid.
    newest: HashMap<Oid, u64>,
}

impl Writer {
    /// Create the file at `path` with the magic and no transaction. An
    /// existing file is truncated.
    pub(crate) fn create(path: &Path) -> Result<Self, Error> {
        let file = File::create(path).map_err(|source| io_error(path, source))?;
        let mut writer = Self {
            path: path.to_owned(),
            file: BufWriter::with_capacity(1 << 16, file),
            pos: 0,
            newest: HashMap::new(),
        };
        writer.write(MAGIC)?;
        Ok(writer)
    }

    /// Open the file at `path` to append transactions after its `tail`.
    /// The file must hold exactly the bytes that `tail` describes.
    pub(crate) fn append(path: &Path, tail: Tail) -> Result<Self, Error> {
        let io = |source| io_error(path, source);
        let mut file = OpenOptions::new().write(true).open(path).map_err(io)?;
        let size = file.metadata().map_err(io)?.len();
        if size != tail.end {
            let message = format!("the file has {size} bytes, not {}", tail.end);
            return Err(io(io::Error::other(message)));
        }
        file.seek(SeekFrom::Start(tail.end)).map_err(io)?;
        Ok(Self {
            path: path.to_owned(),
            file: BufWriter::with_capacity(1 << 16, file),
            pos: tail.end,
            newest: tail.newest,
        })
    }

    /// Append the transaction `txn` with its records in the order of
    /// `txn.records`.
    ///
    /// A user, a description or an extension longer than 65535 bytes fails
    /// with `TooLong` before anything is written, because the header holds
    /// each length in 2 bytes.
    pub(crate) fn write_txn(&mut self, txn: &TxnInfo) -> Result<(), Error> {
        let mut lens = [0u16; 3];
        let fields = [
            ("user", &txn.user),
            ("description", &txn.description),
            ("extension", &txn.extension),
        ];
        for ((field, bytes), len) in fields.into_iter().zip(&mut lens) {
            *len = u16::try_from(bytes.len()).map_err(|_| Error::TooLong {
                tid: txn.tid,
                field,
                len: bytes.len(),
            })?;
        }
        let header_len = TXN_HEADER_LEN + lens.iter().map(|&len| u64::from(len)).sum::<u64>();
        let body: u64 = txn
            .records
            .iter()
            .map(|record| record_len(data(&record.data)))
            .sum();
        let len = header_len + body;
        let start = self.pos;

        self.write(&txn.tid.0.to_be_bytes())?;
        self.write(&len.to_be_bytes())?;
        self.write(b" ")?;
        for len in lens {
            self.write(&len.to_be_bytes())?;
        }
        self.write(&txn.user)?;
        self.write(&txn.description)?;
        self.write(&txn.extension)?;
        for record in &txn.records {
            let data = data(&record.data);
            let previous = self.newest.insert(record.oid, self.pos).unwrap_or(0);
            let data_len = data.map_or(0, |data| data.len() as u64);
            self.write(&record.oid.0.to_be_bytes())?;
            self.write(&txn.tid.0.to_be_bytes())?;
            self.write(&previous.to_be_bytes())?;
            self.write(&start.to_be_bytes())?;
            // The version length, always 0.
            self.write(&[0, 0])?;
            self.write(&data_len.to_be_bytes())?;
            match data {
                Some(data) => self.write(data)?,
                // A back pointer of 0: the record has no data.
                None => self.write(&0u64.to_be_bytes())?,
            }
        }
        self.write(&len.to_be_bytes())?;
        debug_assert_eq!(self.pos, start + len + NUMBER_LEN);
        Ok(())
    }

    /// Write the buffer to the file and sync the file.
    pub(crate) fn finish(mut self) -> Result<(), Error> {
        let io = |source| io_error(&self.path, source);
        self.file.flush().map_err(io)?;
        self.file.get_ref().sync_all().map_err(io)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(bytes)
            .map_err(|source| io_error(&self.path, source))?;
        self.pos += bytes.len() as u64;
        Ok(())
    }
}

/// Return the data of a record, or `None` for a deletion. An empty pickle
/// is a deletion too, because a data length of 0 means a back pointer.
fn data(data: &Option<Vec<u8>>) -> Option<&[u8]> {
    data.as_deref().filter(|data| !data.is_empty())
}

/// Return the length of a data record with `data`.
fn record_len(data: Option<&[u8]>) -> u64 {
    DATA_HEADER_LEN + data.map_or(NUMBER_LEN, |data| data.len() as u64)
}

#[cfg(test)]
mod tests {
    use zoe_core::RecordInfo;

    use super::*;
    use crate::filestorage::tests::test_dir;
    use crate::filestorage::{DataRecord, TxnHeader};

    fn txn(tid: u64, meta: [&[u8]; 3], records: &[(u64, Option<&[u8]>)]) -> TxnInfo {
        TxnInfo {
            tid: Tid(tid),
            user: meta[0].to_vec(),
            description: meta[1].to_vec(),
            extension: meta[2].to_vec(),
            records: records
                .iter()
                .map(|&(oid, data)| RecordInfo {
                    oid: Oid(oid),
                    tid: Tid(tid),
                    data: data.map(<[u8]>::to_vec),
                })
                .collect(),
        }
    }

    fn write(path: &Path, txns: &[TxnInfo]) {
        let mut writer = Writer::create(path).unwrap();
        for txn in txns {
            writer.write_txn(txn).unwrap();
        }
        writer.finish().unwrap();
    }

    fn read(path: &Path) -> Vec<(TxnHeader, Vec<DataRecord>)> {
        let mut reader = Reader::open(path).unwrap();
        let mut txns = Vec::new();
        while let Some(txn) = reader.next_txn().unwrap() {
            let records = reader.records(&txn).collect::<Result<_, _>>().unwrap();
            txns.push((txn, records));
        }
        assert_eq!(reader.end(), Some(End::Clean));
        txns
    }

    #[test]
    fn a_transaction_has_the_bytes_of_filestorage() {
        let path = test_dir("writer-bytes").join("Data.fs");
        write(
            &path,
            &[
                txn(0x10, [b"u", b"d", b"e"], &[(1, Some(b"ab"))]),
                txn(0x20, [b"", b"", b""], &[(1, None)]),
            ],
        );
        let mut want = b"FS30".to_vec();
        // The first transaction at offset 4: 23 + 3 bytes of header, then
        // one record of 42 + 2 bytes.
        want.extend_from_slice(&0x10u64.to_be_bytes());
        want.extend_from_slice(&70u64.to_be_bytes());
        want.extend_from_slice(b" \0\x01\0\x01\0\x01ude");
        want.extend_from_slice(&1u64.to_be_bytes());
        want.extend_from_slice(&0x10u64.to_be_bytes());
        want.extend_from_slice(&0u64.to_be_bytes());
        want.extend_from_slice(&4u64.to_be_bytes());
        want.extend_from_slice(b"\0\0");
        want.extend_from_slice(&2u64.to_be_bytes());
        want.extend_from_slice(b"ab");
        want.extend_from_slice(&70u64.to_be_bytes());
        // The second transaction at offset 82: a deletion of oid 1, whose
        // previous record is at offset 30.
        want.extend_from_slice(&0x20u64.to_be_bytes());
        want.extend_from_slice(&73u64.to_be_bytes());
        want.extend_from_slice(b" \0\0\0\0\0\0");
        want.extend_from_slice(&1u64.to_be_bytes());
        want.extend_from_slice(&0x20u64.to_be_bytes());
        want.extend_from_slice(&30u64.to_be_bytes());
        want.extend_from_slice(&82u64.to_be_bytes());
        want.extend_from_slice(b"\0\0");
        want.extend_from_slice(&0u64.to_be_bytes());
        want.extend_from_slice(&0u64.to_be_bytes());
        want.extend_from_slice(&73u64.to_be_bytes());
        assert_eq!(std::fs::read(&path).unwrap(), want);
    }

    #[test]
    fn written_transactions_read_back() {
        let path = test_dir("writer-read-back").join("Data.fs");
        let txns = [
            txn(
                10,
                [b"user", b"first", b"ext"],
                &[(1, Some(b"a")), (7, Some(b"b"))],
            ),
            txn(20, [b"", b"", b""], &[(1, Some(b"c")), (2, Some(b"d"))]),
            txn(30, [b"", b"undo", b""], &[(2, None)]),
            txn(40, [b"", b"", b""], &[]),
        ];
        write(&path, &txns);
        let read = read(&path);
        assert_eq!(read.len(), txns.len());
        for ((header, records), txn) in read.iter().zip(&txns) {
            assert_eq!((header.tid, header.status), (txn.tid, b' '));
            assert_eq!(header.user, txn.user);
            assert_eq!(header.description, txn.description);
            assert_eq!(header.extension, txn.extension);
            let records: Vec<_> = records.iter().map(|r| (r.oid, r.data.clone())).collect();
            let want: Vec<_> = txn
                .records
                .iter()
                .map(|r| (r.oid, r.data.clone()))
                .collect();
            assert_eq!(records, want);
        }
    }

    #[test]
    fn an_appended_file_is_the_file_written_in_one_run() {
        let dir = test_dir("writer-append");
        let txns = [
            txn(10, [b"", b"", b""], &[(1, Some(b"a")), (2, Some(b"b"))]),
            txn(20, [b"", b"", b""], &[(2, None)]),
            txn(30, [b"", b"", b""], &[(1, Some(b"c")), (2, Some(b"d"))]),
        ];
        let whole = dir.join("whole");
        write(&whole, &txns);

        let part = dir.join("part");
        write(&part, &txns[..2]);
        let tail = tail(&part).unwrap();
        assert_eq!(tail.last, Tid(20));
        let mut writer = Writer::append(&part, tail).unwrap();
        writer.write_txn(&txns[2]).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::read(&part).unwrap(),
            std::fs::read(&whole).unwrap()
        );
    }

    #[test]
    fn a_long_field_is_refused_before_it_is_written() {
        let path = test_dir("writer-long").join("Data.fs");
        let mut writer = Writer::create(&path).unwrap();
        let long = vec![b'x'; 65536];
        let error = writer
            .write_txn(&txn(10, [b"", &long, b""], &[(1, Some(b"a"))]))
            .unwrap_err();
        assert!(
            matches!(
                error,
                Error::TooLong {
                    field: "description",
                    len: 65536,
                    ..
                }
            ),
            "{error:?}"
        );
        // A field of 65535 bytes fits.
        writer
            .write_txn(&txn(10, [&long[1..], b"", b""], &[(1, Some(b"a"))]))
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(read(&path)[0].0.user.len(), 65535);
    }

    #[test]
    fn a_file_without_a_whole_last_transaction_has_no_tail() {
        let path = test_dir("writer-no-tail").join("Data.fs");
        write(&path, &[txn(10, [b"", b"", b""], &[(1, Some(b"a"))])]);
        let mut bytes = std::fs::read(&path).unwrap();
        let end = bytes.len() as u64;
        bytes.extend_from_slice(&[0; 30]);
        std::fs::write(&path, &bytes).unwrap();
        let error = tail(&path).unwrap_err();
        assert!(
            matches!(error, Error::Corrupt { pos, .. } if pos == end),
            "{error:?}"
        );
    }
}
