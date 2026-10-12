//! Acceptance test of the blobs of the space screen (F15): a blob record
//! whose file is missing.
//!
//! `Folder::space` counts the current blob files in `blobs`, with the size
//! of each file. A current blob record whose file is missing does not fail
//! the call: it counts in `missing_blobs`, with the size that its blob
//! record gives, and not in `blobs`. The table of the classes counts every
//! current record. The window shows the count of the missing files.

mod common;

use std::io::Write as _;

use zoe_core::{Oid, Store, Tid};
use zoe_loupe::{Folder, Totals};

use common::{files, fresh_dir};

/// The record of a `ZODB.blob.Blob`.
const BLOB_RECORD: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

#[test]
fn a_blob_record_without_its_file_counts_as_missing() {
    let dir = fresh_dir("space-missing-blob");
    // Two blobs of 100 and 250 bytes. The second one loses its file.
    let lost = {
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for (oid, size) in [(Oid(1), 100), (Oid(2), 250)] {
            let mut file = store.new_blob_file().unwrap();
            file.write_all(&vec![oid.0 as u8; size]).unwrap();
            txn.store_blob(oid, Tid(0), BLOB_RECORD, file).unwrap();
        }
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        store.blob_path(Oid(2), tid).unwrap()
    };
    std::fs::remove_file(&lost).unwrap();
    let before = files(&dir);

    let folder = Folder::open(&dir).unwrap();
    let space = folder.space(10).unwrap();
    assert_eq!(space.blobs, Totals { count: 1, bytes: 100 });
    assert_eq!(space.missing_blobs, Totals { count: 1, bytes: 250 });
    assert_eq!(space.by_class["ZODB.blob.Blob"].count, 2);

    // The IPC form gives both counts.
    let json = serde_json::to_value(&space).unwrap();
    assert_eq!(
        json["missing_blobs"],
        serde_json::json!({"count": 1, "bytes": 250})
    );
    assert_eq!(json["blobs"], serde_json::json!({"count": 1, "bytes": 100}));

    drop(folder);
    assert_eq!(files(&dir), before, "the space screen changed the store");
}
