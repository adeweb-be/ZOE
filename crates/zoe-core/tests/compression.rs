//! Acceptance test of record compression: the payload of a record frame is
//! zstd-compressed when that makes it smaller, and the codec byte says so.
//! A store reads records of every codec, whatever its current setting.

use std::path::{Path, PathBuf};

use zoe_core::{Codec, Compression, Oid, Options, Store, Tid};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-zstd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(dir: &Path, compression: Compression) -> Store {
    let options = Options {
        compression,
        ..Options::default()
    };
    Store::open_with(dir, options).unwrap()
}

/// A pickle with the repetition of a Plone state: the same keys and
/// class names many times.
fn repetitive_pickle() -> Vec<u8> {
    let mut data = b"\x80\x03cplone.app.contenttypes.content\nDocument\nq\x00.\x80\x03}q\x01(".to_vec();
    for i in 0..1_000 {
        data.extend_from_slice(b"X\x0b\x00\x00\x00descriptionq\x02X\x05\x00\x00\x00title");
        data.extend_from_slice(&(i as u32).to_le_bytes());
    }
    data.extend_from_slice(b"u.");
    data
}

/// Bytes that zstd cannot shrink.
fn noise(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn commit(store: &Store, oid: Oid, data: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(oid, Tid::ZERO, data).unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap()
}

#[test]
fn zstd_is_the_default() {
    assert!(matches!(Options::default().compression, Compression::Zstd { .. }));
}

#[test]
fn a_compressible_pickle_is_stored_with_zstd() {
    let dir = fresh_dir("compressible");
    let data = repetitive_pickle();
    let (oid, tid) = {
        let store = open(&dir, Compression::Zstd { level: 3 });
        let oid = store.new_oid().unwrap();
        let tid = commit(&store, oid, &data);
        let info = store.record_info(oid, tid).unwrap();
        assert_eq!(info.codec, Codec::Zstd);
        assert_eq!(info.data_len, data.len() as u64);
        assert!(info.stored_len * 4 < info.data_len, "{info:?}");
        (oid, tid)
    };
    let store = open(&dir, Compression::Zstd { level: 3 });
    assert_eq!(store.load(oid).unwrap(), (data, tid));
}

#[test]
fn an_incompressible_pickle_is_stored_as_it_is() {
    let store = open(&fresh_dir("noise"), Compression::Zstd { level: 3 });
    let oid = store.new_oid().unwrap();
    let data = noise(4096);
    let tid = commit(&store, oid, &data);
    let info = store.record_info(oid, tid).unwrap();
    assert_eq!(info.codec, Codec::None);
    assert_eq!(info.stored_len, data.len() as u64);
    assert_eq!(store.load(oid).unwrap(), (data, tid));
}

#[test]
fn compression_off_writes_plain_records() {
    let store = open(&fresh_dir("off"), Compression::None);
    let oid = store.new_oid().unwrap();
    let tid = commit(&store, oid, &repetitive_pickle());
    assert_eq!(store.record_info(oid, tid).unwrap().codec, Codec::None);
}

#[test]
fn a_store_reads_records_of_every_codec() {
    let dir = fresh_dir("mixed");
    let data = repetitive_pickle();
    let (plain, plain_tid) = {
        let store = open(&dir, Compression::None);
        let oid = store.new_oid().unwrap();
        (oid, commit(&store, oid, &data))
    };
    let (packed, packed_tid) = {
        let store = open(&dir, Compression::Zstd { level: 3 });
        let oid = store.new_oid().unwrap();
        (oid, commit(&store, oid, &data))
    };
    for compression in [Compression::None, Compression::Zstd { level: 19 }] {
        let store = open(&dir, compression);
        assert_eq!(store.load(plain).unwrap(), (data.clone(), plain_tid));
        assert_eq!(store.load(packed).unwrap(), (data.clone(), packed_tid));
    }
}
