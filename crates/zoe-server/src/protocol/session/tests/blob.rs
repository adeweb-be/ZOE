//! The tests of the uploads and the downloads of blobs.

use super::*;

/// Return the files under the blob directory of `storage`, but the
/// layout file. A blob directory that does not exist holds no file.
fn blob_tree(storage: &NamedStorage) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.file_name().unwrap() != ".layout" {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&storage.path.join("blobs"), &mut found);
    found
}

/// Begin the transaction `id`, store a pickle for `oid` and open an
/// upload with one chunk, as a stock client does.
fn upload(s: &mut Client, id: i64, oid: u64, chunk: &[u8]) {
    begin(s, id, None);
    store(s, id, oid, 0, b"pickle");
    done(call(s, "storeBlobStart", vec![]));
    done(call(
        s,
        "storeBlobChunk",
        vec![Value::Binary(chunk.to_vec())],
    ));
}

fn store_blob_end(s: &mut Client, id: i64, oid: u64, data: &[u8]) -> Outcome {
    let data = Value::Binary(data.to_vec());
    call(
        s,
        "storeBlobEnd",
        vec![p64(oid), p64(0), data, Value::from(id)],
    )
}

#[test]
fn an_uploaded_blob_commits_with_the_pickle_of_storea() {
    let mut s = registered("blob-commit");
    upload(&mut s, 1, 1, b"the blob");
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    let tid = Tid(vote_and_finish(&mut s, 1).unwrap());
    assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
    let path = s.store.blob_path(Oid(1), tid).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"the blob");
    assert_eq!(blob_tree(&s.storage), [path]);
    // A stock client counts the storea, not the blob.
    assert_eq!(count(&s.storage.status().to_value(), "stores"), 1);
}

#[test]
fn a_commit_with_a_blob_publishes_its_invalidation() {
    let storage = storage("blob-invalidation", Queues::DEFAULT);
    let (mut writer, mut own) = connected(&storage);
    let (_reader, mut others) = connected(&storage);
    upload(&mut writer, 1, 7, b"the blob");
    done(store_blob_end(&mut writer, 1, 7, b"pickle"));
    let tid = vote_and_finish(&mut writer, 1).unwrap();
    assert_eq!(messages(&mut others), [invalidation(tid, &[7])]);
    let methods: Vec<String> = messages(&mut own).into_iter().map(|(m, _)| m).collect();
    assert_eq!(methods, ["info", ".reply"]);
}

#[test]
fn an_aborted_or_dropped_upload_leaves_no_file() {
    let none = Vec::<std::path::PathBuf>::new();
    // An abort after storeBlobEnd: the core took the file.
    let mut s = registered("blob-abort");
    upload(&mut s, 1, 1, b"the blob");
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    assert_eq!(
        blob_tree(&s.storage).len(),
        1,
        "the transaction holds the file"
    );
    done(call(&mut s, "tpc_abort", vec![Value::from(1)]));
    assert_eq!(blob_tree(&s.storage), none);

    // An abort before storeBlobEnd: the upload file is still open.
    upload(&mut s, 2, 1, b"the blob");
    assert_eq!(blob_tree(&s.storage).len(), 1, "the upload file");
    done(call(&mut s, "tpc_abort", vec![Value::from(2)]));
    assert_eq!(blob_tree(&s.storage), none);

    // The end of the connection, during the upload and after it.
    upload(&mut s, 3, 1, b"the blob");
    let storage = Arc::clone(&s.storage);
    drop(s);
    assert_eq!(blob_tree(&storage), none);
    let (mut s, _frames) = connected(&storage);
    upload(&mut s, 4, 1, b"the blob");
    done(store_blob_end(&mut s, 4, 1, b"pickle"));
    drop(s);
    assert_eq!(blob_tree(&storage), none);

    // A finish with an open upload commits the pickle alone.
    let (mut s, _frames) = connected(&storage);
    upload(&mut s, 5, 1, b"the blob");
    let tid = Tid(vote_and_finish(&mut s, 5).unwrap());
    assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
    assert_eq!(blob_tree(&storage), none);
}

#[test]
fn a_blob_that_the_core_refuses_fails_the_vote_and_leaves_no_file() {
    let none = Vec::<std::path::PathBuf>::new();
    let mut s = registered("blob-refused");
    upload(&mut s, 1, 1, b"the blob");
    // The core refuses an empty pickle, and the drop of the file removes it.
    done(store_blob_end(&mut s, 1, 1, b""));
    assert_eq!(blob_tree(&s.storage), none);
    let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
    assert_eq!(fault.class, TRANSACTION_ERROR);
    assert!(s.txn.open.is_none());

    // An oid that is not 8 bytes fails the vote, as in storea.
    upload(&mut s, 2, 1, b"the blob");
    let data = Value::Binary(b"pickle".to_vec());
    let end = vec![Value::from(1), p64(0), data, Value::from(2)];
    done(call(&mut s, "storeBlobEnd", end));
    assert_eq!(blob_tree(&s.storage), none);
    let fault = failed(call(&mut s, "vote", vec![Value::from(2)]));
    assert_eq!(fault.class, "builtins.TypeError");
}

#[test]
fn after_an_error_of_the_transaction_the_blob_calls_write_no_file() {
    let none = Vec::<std::path::PathBuf>::new();
    // A read-only connection: tpc_begin fails the transaction.
    let mut s = session("blob-read-only");
    let register = vec![Value::from("1"), Value::Boolean(true)];
    done(call(&mut s, "register", register));
    upload(&mut s, 1, 1, b"the blob");
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    assert_eq!(blob_tree(&s.storage), none);
    let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
    assert_eq!(fault, Fault::read_only());

    // A store that fails during an upload: the end removes the file.
    let mut s = registered("blob-failed-store");
    let chunk = || vec![Value::Binary(b"part".to_vec())];
    begin(&mut s, 1, None);
    done(call(&mut s, "storeBlobStart", vec![]));
    done(call(&mut s, "storeBlobChunk", chunk()));
    assert_eq!(blob_tree(&s.storage).len(), 1, "the upload file");
    // The core refuses an empty pickle.
    store(&mut s, 1, 1, 0, b"");
    done(call(&mut s, "storeBlobChunk", chunk()));
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    assert_eq!(blob_tree(&s.storage), none);
    let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
    assert_eq!(fault.class, TRANSACTION_ERROR);
}

#[test]
fn blob_calls_out_of_order_fail() {
    let mut s = registered("blob-order");
    let chunk = || vec![Value::Binary(b"more".to_vec())];
    let fault = failed(call(&mut s, "storeBlobStart", vec![]));
    assert_eq!(fault.class, TRANSACTION_ERROR);
    let fault = failed(call(&mut s, "storeBlobChunk", chunk()));
    assert_eq!(fault.class, "builtins.ValueError");
    upload(&mut s, 1, 1, b"the blob");
    let fault = failed(call(&mut s, "storeBlobStart", vec![]));
    assert_eq!(fault.class, "builtins.ValueError");
    // A storeBlobEnd of another transaction leaves the upload open.
    let fault = failed(store_blob_end(&mut s, 2, 1, b"pickle"));
    assert_eq!(fault.class, TRANSACTION_ERROR);
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    let fault = failed(store_blob_end(&mut s, 1, 1, b"pickle"));
    assert_eq!(fault.class, "builtins.ValueError");
    let fault = failed(call(&mut s, "storeBlobChunk", chunk()));
    assert_eq!(fault.class, "builtins.ValueError");
    let fault = failed(call(&mut s, "storeBlobChunk", vec![Value::from("text")]));
    assert_eq!(fault.class, "builtins.TypeError");
}

/// Store `chunk` as the blob of `oid` over `serial` in the open
/// transaction `id`, as a stock client does: `storea`, then the upload.
fn store_blob(s: &mut Client, id: i64, oid: u64, serial: u64, chunk: &[u8]) {
    store(s, id, oid, serial, b"pickle");
    done(call(s, "storeBlobStart", vec![]));
    let chunk = vec![Value::Binary(chunk.to_vec())];
    done(call(s, "storeBlobChunk", chunk));
    let data = Value::Binary(b"pickle".to_vec());
    let end = vec![p64(oid), p64(serial), data, Value::from(id)];
    done(call(s, "storeBlobEnd", end));
}

#[test]
fn a_stale_blob_is_listed_and_its_transaction_leaves_no_file() {
    let mut s = registered("blob-conflict");
    begin(&mut s, 1, None);
    store_blob(&mut s, 1, 1, 0, b"first");
    let first = vote_and_finish(&mut s, 1).unwrap();
    begin(&mut s, 2, None);
    store_blob(&mut s, 2, 1, first, b"second");
    let second = vote_and_finish(&mut s, 2).unwrap();
    let committed = blob_tree(&s.storage);
    assert_eq!(committed.len(), 2);

    // A blob over the stale serial. The first vote lists the conflict,
    // as for a pickle. A stock client cannot resolve a blob, so it
    // aborts.
    begin(&mut s, 3, None);
    store_blob(&mut s, 3, 1, first, b"stale");
    let listed = done(call(&mut s, "vote", vec![Value::from(3)]));
    let conflicts = listed.as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        get(&conflicts[0], "serials"),
        &Value::Array(vec![p64(second), p64(first)])
    );
    done(call(&mut s, "tpc_abort", vec![Value::from(3)]));
    assert_eq!(blob_tree(&s.storage), committed);

    // A second vote without a resolved pickle fails with ConflictError.
    // It ends the transaction, and that removes the staged blob and an
    // upload that is still open.
    begin(&mut s, 4, None);
    store_blob(&mut s, 4, 1, first, b"stale");
    done(call(&mut s, "storeBlobStart", vec![]));
    done(call(
        &mut s,
        "storeBlobChunk",
        vec![Value::Binary(b"open".to_vec())],
    ));
    assert_eq!(blob_tree(&s.storage).len(), 4);
    done(call(&mut s, "vote", vec![Value::from(4)]));
    let fault = failed(call(&mut s, "vote", vec![Value::from(4)]));
    assert_eq!(fault.class, CONFLICT_ERROR);
    assert!(s.txn.open.is_none());
    assert_eq!(blob_tree(&s.storage), committed);
}

#[test]
fn send_blob_checks_the_blob_before_the_first_chunk() {
    let storage = storage("send-blob", Queues::DEFAULT);
    let (mut s, mut frames) = connected(&storage);
    upload(&mut s, 1, 1, b"the blob");
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    let tid = vote_and_finish(&mut s, 1).unwrap();
    messages(&mut frames);
    let send_blob =
        |s: &mut Client, oid: u64, tid: u64| call(s, "sendBlob", vec![p64(oid), p64(tid)]);
    assert_eq!(done(send_blob(&mut s, 1, tid)), Value::Nil);
    let methods: Vec<String> = messages(&mut frames).into_iter().map(|(m, _)| m).collect();
    assert_eq!(
        methods,
        ["receiveBlobStart", "receiveBlobChunk", "receiveBlobStop"]
    );

    // No blob record: a POSKeyError, as in ZEO.
    let fault = failed(send_blob(&mut s, 1, tid + 1));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
    let fault = failed(send_blob(&mut s, 2, tid));
    assert_eq!(fault.class, "ZODB.POSException.POSKeyError");

    // A file that does not match its record, then a missing file.
    let path = s.store.blob_path(Oid(1), Tid(tid)).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
    std::fs::write(&path, b"the blub").unwrap();
    let corrupt = Fault::from(ZoeError::CorruptBlob {
        oid: Oid(1),
        tid: Tid(tid),
    });
    assert_eq!(failed(send_blob(&mut s, 1, tid)), corrupt);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(failed(send_blob(&mut s, 1, tid)), corrupt);
    assert_eq!(corrupt.class, "ZODB.POSException.StorageSystemError");
    // A blob that fails sends no chunk.
    assert!(messages(&mut frames).is_empty());
}

#[test]
fn a_download_to_a_client_that_is_gone_ends_the_session() {
    // The frames of this session go nowhere.
    let mut s = registered("send-blob-gone");
    upload(&mut s, 1, 1, b"the blob");
    done(store_blob_end(&mut s, 1, 1, b"pickle"));
    let tid = vote_and_finish(&mut s, 1).unwrap();
    let outcome = call(&mut s, "sendBlob", vec![p64(1), p64(tid)]);
    assert!(matches!(outcome, Outcome::Gone), "{outcome:?}");
}

#[test]
fn a_blob_with_a_nil_serial_is_a_new_object() {
    let mut s = registered("blob-nil-serial");
    // A stock `storeBlob(oid, None, ...)`: `storea`, the upload and
    // `storeBlobEnd`, each with the nil serial.
    let send = |s: &mut Client, id: i64, data: &[u8]| {
        begin(s, id, None);
        store_with(s, id, 1, Value::Nil, b"pickle");
        done(call(s, "storeBlobStart", vec![]));
        done(call(
            s,
            "storeBlobChunk",
            vec![Value::Binary(data.to_vec())],
        ));
        let end = vec![
            p64(1),
            Value::Nil,
            Value::Binary(b"pickle".to_vec()),
            Value::from(id),
        ];
        done(call(s, "storeBlobEnd", end));
    };
    send(&mut s, 1, b"the blob");
    let tid = Tid(vote_and_finish(&mut s, 1).unwrap());
    assert_eq!(s.store.load_serial(Oid(1), tid).unwrap(), b"pickle");
    let path = s.store.blob_path(Oid(1), tid).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"the blob");

    // The oid has a record now: the nil serial fails the vote.
    send(&mut s, 2, b"another blob");
    let fault = vote_and_finish(&mut s, 2).unwrap_err();
    assert_eq!(fault.class, CONFLICT_ERROR);
    assert_eq!(
        item(&fault, "serials"),
        Value::Array(vec![p64(tid.0), p64(0)])
    );
    assert_eq!(
        done(call(&mut s, "tpc_abort", vec![Value::from(2)])),
        Value::Nil
    );
    assert_eq!(blob_tree(&s.storage), [path]);
}
