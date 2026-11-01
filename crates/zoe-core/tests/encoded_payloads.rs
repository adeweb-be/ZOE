//! Acceptance test: a payload encoded before the finish (N4).
//!
//! A skin encodes the staged pickles of a transaction on other threads
//! with `Encoded::encode`, and attaches each result with
//! `Txn::set_encoded`. Then:
//!
//! - the finish compresses no record whose payload is attached, and
//!   compresses inline each record without one;
//! - the stored frames are byte for byte the frames of an inline
//!   compression, for a pickle in memory and for a spilled pickle;
//! - a payload that does not encode the staged pickle with the compression
//!   of the store is not attached, and a later store of the oid drops the
//!   attached payload.
//!
//! `zoe_core::testing::compressions` counts the zstd compressions of the
//! thread of the test. With one transaction at a time, the finish runs on
//! that thread.

mod support;

use std::path::Path;

use support::fresh_dir;
use zoe_core::testing::{self, Key};
use zoe_core::{Compression, Encoded, Oid, Options, Store, Tid, Txn};

fn open(dir: &Path, options: Options) -> Store {
    Store::open_with(dir, options).unwrap()
}

/// The options of a store that compresses its records with zstd at
/// level 3.
fn zstd() -> Options {
    Options {
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    }
}

/// A pickle of `len` bytes that zstd compresses well.
fn compressible_item(oid: u64, len: usize) -> Vec<u8> {
    let mut data = b"\x80\x03cplone.app.content\nItem".to_vec();
    data.extend_from_slice(format!("{oid}\nq\x00.").as_bytes());
    while data.len() < len {
        data.extend_from_slice(b"X\x05\x00\x00\x00titleq\x02X\x04\x00\x00\x00textq\x03");
    }
    data.truncate(len);
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

/// The records of the tests: compressible pickles of 1 KB and 100 KB, a
/// pickle that zstd does not shrink, and a pickle of one byte.
fn records() -> Vec<(Oid, Vec<u8>)> {
    let mut records: Vec<(Oid, Vec<u8>)> = (1..=20)
        .map(|oid| (Oid(oid), compressible_item(oid, 1024)))
        .collect();
    records.push((Oid(21), compressible_item(21, 100 * 1024)));
    records.push((Oid(22), noise(5_000)));
    records.push((Oid(23), b"x".to_vec()));
    records
}

/// Encode `records` on another thread, as the compression pool of a skin
/// does.
fn encode_elsewhere(records: &[(Oid, Vec<u8>)], compression: Compression) -> Vec<(Oid, Encoded)> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                records
                    .iter()
                    .map(|(oid, data)| (*oid, Encoded::encode(data, compression)))
                    .collect()
            })
            .join()
            .unwrap()
    })
}

/// Vote and finish `txn`. Return its tid and the zstd compressions of the
/// finish.
fn finish(mut txn: Txn<&Store>) -> (Tid, u64) {
    txn.vote().unwrap();
    let before = testing::compressions();
    let tid = txn.finish().unwrap();
    (tid, testing::compressions() - before)
}

#[test]
fn the_finish_compresses_no_record_whose_payload_is_attached() {
    let dir = fresh_dir("attached");
    let store = open(&dir, zstd());
    let records = records();

    // Without an attached payload, the finish compresses each record.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for (oid, data) in &records {
        txn.store(*oid, Tid::ZERO, data).unwrap();
    }
    let (first, compressions) = finish(txn);
    assert_eq!(compressions, records.len() as u64);

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for (oid, data) in &records {
        txn.store(*oid, first, data).unwrap();
    }
    for (oid, encoded) in encode_elsewhere(&records, store.compression()) {
        assert!(txn.set_encoded(oid, encoded).unwrap(), "{oid:?}");
    }
    let (second, compressions) = finish(txn);
    assert_eq!(compressions, 0);
    for (oid, data) in &records {
        assert_eq!(store.load(*oid).unwrap(), (data.clone(), second));
    }
}

/// Restore `records` under `tid` in `store`, and attach their payloads
/// when `attach` is true. Return the zstd compressions of the finish.
fn restore(store: &Store, tid: Tid, records: &[(Oid, Vec<u8>)], attach: bool) -> u64 {
    let mut txn = store.tpc_begin_restore(tid, b"u", b"d", b"").unwrap();
    for (oid, data) in records {
        txn.restore(*oid, Some(data)).unwrap();
    }
    if attach {
        for (oid, encoded) in encode_elsewhere(records, store.compression()) {
            assert!(txn.set_encoded(oid, encoded).unwrap(), "{oid:?}");
        }
    }
    finish(txn).1
}

#[test]
fn an_attached_payload_gives_the_frames_of_an_inline_compression() {
    let records = records();
    // Each pickle of 1 KB and more goes to the spill file of the second
    // pair of stores.
    for (name, staging_memory) in [("memory", 16 << 20), ("spilled", 512)] {
        for compression in [
            Compression::default(),
            Compression::Zstd { level: 9 },
            Compression::None,
        ] {
            let options = Options {
                compression,
                staging_memory,
                ..Options::default()
            };
            let inline_dir = fresh_dir(&format!("inline-{name}"));
            let inline = open(&inline_dir, options.clone());
            let attached_dir = fresh_dir(&format!("attached-{name}"));
            let attached = open(&attached_dir, options);
            for tid in [Tid(0x0300_0000_0000_0001), Tid(0x0300_0000_0000_0002)] {
                restore(&inline, tid, &records, false);
                assert_eq!(restore(&attached, tid, &records, true), 0);
                for (oid, _) in &records {
                    let key = Key::Record(*oid, tid);
                    assert_eq!(
                        testing::read_frame(&inline, key).unwrap(),
                        testing::read_frame(&attached, key).unwrap(),
                        "{name} {compression:?} {oid:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_payload_of_other_bytes_or_another_compression_is_not_attached() {
    let dir = fresh_dir("refused");
    let store = open(&dir, zstd());
    let data = compressible_item(1, 4096);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, &data).unwrap();
    let mut other = data.clone();
    other[100] ^= 1;
    let refused = [
        Encoded::encode(&other, store.compression()),
        Encoded::encode(&data[..4000], store.compression()),
        Encoded::encode(&data, Compression::Zstd { level: 9 }),
        Encoded::encode(&data, Compression::None),
    ];
    for encoded in refused {
        assert!(!txn.set_encoded(Oid(1), encoded).unwrap());
    }
    // An oid that the transaction did not stage.
    let encoded = Encoded::encode(&data, store.compression());
    assert!(!txn.set_encoded(Oid(2), encoded).unwrap());
    let (tid, compressions) = finish(txn);
    assert_eq!(compressions, 1);
    assert_eq!(store.load(Oid(1)).unwrap(), (data, tid));
}

#[test]
fn a_later_store_of_the_oid_drops_its_attached_payload() {
    let dir = fresh_dir("restored");
    let store = open(&dir, zstd());
    let (first, second) = (compressible_item(1, 4096), compressible_item(2, 4096));
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, &first).unwrap();
    let encoded = Encoded::encode(&first, store.compression());
    assert!(txn.set_encoded(Oid(1), encoded).unwrap());
    txn.store(Oid(1), Tid::ZERO, &second).unwrap();
    let (tid, compressions) = finish(txn);
    assert_eq!(compressions, 1);
    assert_eq!(store.load(Oid(1)).unwrap(), (second, tid));
}

#[test]
fn set_encoded_from_another_thread_fails() {
    let dir = fresh_dir("thread");
    let store = open(&dir, Options::default());
    let data = compressible_item(1, 4096);
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store(Oid(1), Tid::ZERO, &data).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let encoded = Encoded::encode(&data, store.compression());
                assert!(matches!(
                    txn.set_encoded(Oid(1), encoded),
                    Err(zoe_core::ZoeError::WrongThread)
                ));
            })
            .join()
            .unwrap();
    });
}
