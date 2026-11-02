//! The text deltas of a spike: a scratch store, not merged.
//!
//! This module writes and reads the key spaces `text_deltas` and
//! `text_txn` (`format::text`) in a redb file of their own, with the frame
//! and the checksum of the core. It measures the bytes of a delta, the read
//! of a text at a snapshot, and the scan of a tid range. It does not go
//! through the staging, the commit batch or the write-ahead log.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::format::text::{
    decode_text, encode_text, split_text_key, text_key, text_prefix, text_txn_key,
};
use crate::format::{Compression, KeySpace, MAIN_BRANCH, decode_frame, encode_frame_with};
use crate::types::Tid;

const TEXT_DELTAS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("text_deltas");
const TEXT_TXN: TableDefinition<&[u8], ()> = TableDefinition::new("text_txn");

/// The error of the scratch store: a message.
pub type TextResult<T> = std::result::Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// One delta: the text of `rid` in `index` of `catalog`, or `None` for an
/// unindex. The text is the language, then the text.
#[derive(Clone, Debug)]
pub struct Delta {
    /// The oid of the catalog.
    pub catalog: u64,
    /// The id of the text index.
    pub index: String,
    /// The rid of the document.
    pub rid: i32,
    /// The language and the text, or `None` for an unindex.
    pub text: Option<(String, String)>,
}

/// The bytes that a commit writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteStats {
    /// The rows of `text_deltas`.
    pub rows: u64,
    /// The bytes of the keys and the frames of `text_deltas`.
    pub delta_bytes: u64,
    /// The bytes of the keys of `text_txn`.
    pub index_bytes: u64,
}

/// The scratch store of the text deltas.
pub struct TextDeltas {
    db: Database,
    compression: Compression,
}

impl TextDeltas {
    /// Create the store at `path`, or open it.
    pub fn create(path: &Path, compression: Compression) -> TextResult<Self> {
        let db = Database::create(path).map_err(err)?;
        Ok(Self { db, compression })
    }

    /// Write the deltas of the commits of `commits`, each at its tid, in
    /// one backend write. The tids increase.
    pub fn commit(&self, commits: &[(u64, Vec<Delta>)]) -> TextResult<WriteStats> {
        let mut stats = WriteStats::default();
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut deltas = txn.open_table(TEXT_DELTAS).map_err(err)?;
            let mut index = txn.open_table(TEXT_TXN).map_err(err)?;
            for (tid, list) in commits {
                for d in list {
                    let prefix = text_prefix(d.catalog, &d.index, d.rid);
                    let key = text_key(d.catalog, &d.index, d.rid, *tid);
                    // The previous serial: the tid of the previous version.
                    let prev = {
                        let mut range = deltas.range(prefix.as_slice()..key.as_slice()).map_err(err)?;
                        match range.next_back() {
                            Some(row) => {
                                let row = row.map_err(err)?;
                                split_text_key(row.0.value()).map_or(0, |k| k.3)
                            }
                            None => 0,
                        }
                    };
                    let payload = match &d.text {
                        Some((lang, text)) => encode_text(lang, text),
                        None => Vec::new(),
                    };
                    let frame = encode_frame_with(
                        KeySpace::TextDeltas,
                        &key,
                        MAIN_BRANCH,
                        Tid(prev),
                        &payload,
                        self.compression,
                    );
                    let tkey = text_txn_key(*tid, d.catalog, &d.index, d.rid);
                    stats.rows += 1;
                    stats.delta_bytes += (key.len() + frame.len()) as u64;
                    stats.index_bytes += tkey.len() as u64;
                    deltas.insert(key.as_slice(), frame.as_slice()).map_err(err)?;
                    index.insert(tkey.as_slice(), ()).map_err(err)?;
                }
            }
        }
        txn.commit().map_err(err)?;
        Ok(stats)
    }

    /// Return the language and the text of `rid` in `index` of `catalog` as
    /// of `snapshot`: the newest delta at or before it. `None` when the
    /// document has no text there, or an unindex.
    pub fn text_at(
        &self,
        catalog: u64,
        index: &str,
        rid: i32,
        snapshot: u64,
    ) -> TextResult<Option<(String, String)>> {
        let read = self.db.begin_read().map_err(err)?;
        let table = read.open_table(TEXT_DELTAS).map_err(err)?;
        let prefix = text_prefix(catalog, index, rid);
        let end = text_key(catalog, index, rid, snapshot);
        let mut range = table.range(prefix.as_slice()..=end.as_slice()).map_err(err)?;
        let Some(row) = range.next_back() else {
            return Ok(None);
        };
        let (key, value) = row.map_err(err)?;
        let frame = decode_frame(KeySpace::TextDeltas, key.value(), value.value())
            .map_err(|e| format!("corrupt text delta: {e:?}"))?;
        let text = decode_text(&frame.data).ok_or("bad text payload")?;
        Ok(text.map(|(l, t)| (l.to_owned(), t.to_owned())))
    }

    /// Return the documents of `catalog` whose text changed after `from`
    /// and at or before `to`, as (index, rid), each once, from `text_txn`.
    pub fn changed(&self, catalog: u64, from: u64, to: u64) -> TextResult<Vec<(String, i32)>> {
        let read = self.db.begin_read().map_err(err)?;
        let table = read.open_table(TEXT_TXN).map_err(err)?;
        let start = (from + 1).to_be_bytes();
        let end = (to.saturating_add(1)).to_be_bytes();
        let mut out = std::collections::BTreeSet::new();
        for row in table.range(&start[..]..&end[..]).map_err(err)? {
            let (key, _) = row.map_err(err)?;
            let key = key.value();
            let mut full = key[8..].to_vec();
            full.extend_from_slice(&key[..8]);
            let (cat, index, rid, _) = split_text_key(&full).ok_or("bad text_txn key")?;
            if cat == catalog {
                out.insert((index.to_owned(), rid));
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Read and verify every frame of `text_deltas`. Return the rows and
    /// the bytes of their keys and values.
    pub fn verify(&self) -> TextResult<(u64, u64)> {
        let read = self.db.begin_read().map_err(err)?;
        let table = read.open_table(TEXT_DELTAS).map_err(err)?;
        let (mut rows, mut bytes) = (0u64, 0u64);
        for row in table.iter().map_err(err)? {
            let (key, value) = row.map_err(err)?;
            decode_frame(KeySpace::TextDeltas, key.value(), value.value())
                .map_err(|e| format!("corrupt text delta: {e:?}"))?;
            rows += 1;
            bytes += (key.value().len() + value.value().len()) as u64;
        }
        Ok((rows, bytes))
    }
}

/// Return the payload of a delta, for a measure.
pub fn encode_payload_for_measure(language: &str, text: &str) -> Vec<u8> {
    encode_text(language, text)
}

/// Return the length of `data` as one zstd frame at level 3, the default
/// level of the store setting `zstd`.
pub fn zstd_len_for_measure(data: &[u8]) -> usize {
    crate::format::encode_payload(data, Compression::Zstd { level: 3 }).1.len()
}
