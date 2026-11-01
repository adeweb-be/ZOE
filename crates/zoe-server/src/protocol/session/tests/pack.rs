//! The tests of `pack`.

use super::*;

/// The message `info` with the sizes of `storage`.
fn info_of(storage: &NamedStorage) -> Vec<u8> {
    let size = storage.size().unwrap();
    let length = storage.store.object_count().unwrap_or(0);
    message::notify(
        "info",
        vec![Value::Map(vec![
            (Value::from("length"), Value::from(length)),
            (Value::from("size"), Value::from(size)),
        ])],
    )
}

#[test]
fn pack_sends_info_through_the_hub_to_every_client_before_its_reply() {
    let storage = storage("pack-info", Queues::DEFAULT);
    let (mut other, mut other_frames) = connected(&storage);
    let (outbox, mut unregistered_frames, _closing) = Outbox::new(10, 10);
    let _unregistered = Client::new(Arc::clone(&storage), outbox);
    let root = commit(&mut other, 1, 0, 0, MAPPING).unwrap();
    let dropped = commit(&mut other, 2, 1, 0, MAPPING).unwrap();
    // The info and the reply of each commit.
    assert_eq!(messages(&mut other_frames).len(), 4);

    // The packing client runs as a connection does.
    let (calls, received) = tokio::sync::mpsc::channel(crate::protocol::QUEUE);
    let (outbox, mut frames, _closing) = Outbox::new(10, 10);
    let worker = std::thread::spawn({
        let storage = Arc::clone(&storage);
        move || run(serving(storage), received, outbox)
    });
    let request = |id: i64, method: &str, args: Vec<Value>| Call {
        id: Value::from(id),
        oneway: false,
        method: method.into(),
        args,
        context: None,
    };
    let register = vec![Value::from("1"), Value::Boolean(false)];
    calls
        .blocking_send(request(1, "register", register))
        .unwrap();
    // A time in 2099: the pack packs at the last published tid.
    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    calls.blocking_send(request(2, "pack", pack)).unwrap();
    drop(calls);
    worker.join().unwrap();

    let info = info_of(&storage);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| frames.try_recv())
        .map(|frame| frame.to_vec())
        .collect();
    assert_eq!(
        got,
        [
            message::reply(Value::from(1), p64(dropped)),
            info.clone(),
            message::reply(Value::from(2), Value::Nil),
        ]
    );
    assert_eq!(
        other_frames.try_recv().map(|frame| frame.to_vec()),
        Some(info)
    );
    assert!(other_frames.try_recv().is_none());
    assert!(unregistered_frames.try_recv().is_none());

    // The pack removed the object that the root does not reach.
    let fault = failed(call(&mut other, "loadSerial", vec![p64(1), p64(dropped)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(1) }));
    let load = done(call(&mut other, "loadSerial", vec![p64(0), p64(root)]));
    assert_eq!(load, Value::Binary(MAPPING.to_vec()));
}

#[test]
fn a_pack_keeps_the_records_that_a_read_of_the_published_commits_needs() {
    let mut s = registered("pack-published");
    let root = commit(&mut s, 1, 0, 0, MAPPING).unwrap();
    // A commit of a new revision of the root is durable, and the hub
    // does not publish it yet.
    let mut txn = Txn::begin(Arc::clone(&s.store), b"", b"", b"");
    txn.store(Oid(0), Tid(root), MAPPING).unwrap();
    txn.vote().unwrap();
    let finished = txn.finish_after().unwrap();
    let next = finished.tid;
    assert_eq!(s.store.last_transaction(), next);
    assert_eq!(s.storage.invalidations.published(), Tid(root));

    // A pack at a time after that commit packs at the last published
    // tid. So it keeps the revision that a current read gives.
    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);
    let current = vec![p64(0), p64(u64::MAX)];
    assert_eq!(
        done(call(&mut s, "loadBefore", current.clone())),
        Value::Array(vec![Value::Binary(MAPPING.to_vec()), p64(root), Value::Nil])
    );
    assert_eq!(
        done(call(&mut s, "loadSerial", vec![p64(0), p64(root)])),
        Value::Binary(MAPPING.to_vec())
    );

    // After the publication, a read gives the new revision.
    drop(s.publication(finished, None));
    assert_eq!(
        done(call(&mut s, "loadBefore", current)),
        Value::Array(vec![
            Value::Binary(MAPPING.to_vec()),
            p64(next.0),
            Value::Nil
        ])
    );
}

#[test]
fn pack_takes_a_time_and_a_read_only_store_refuses_it() {
    let mut s = registered("pack-args");
    for args in [vec![], vec![Value::from("now")]] {
        let fault = failed(call(&mut s, "pack", args));
        assert_eq!(fault.class, "builtins.TypeError");
    }
    let fault = failed(call(&mut s, "pack", vec![Value::F64(f64::NAN)]));
    assert_eq!(fault.class, "builtins.ValueError");
    // An int is a time too.
    let pack = vec![Value::from(1_000_000_000), Value::from(0)];
    assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);

    // A store that the server opened read-only refuses the pack, also
    // for a connection that is not read-only.
    let dir = s.storage.path.clone();
    drop(s);
    let store = Arc::new(Store::open_read_only(&dir).unwrap());
    let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
    for read_only in [false, true] {
        let mut s = Client::new(Arc::clone(&storage), outbox());
        done(call(
            &mut s,
            "register",
            vec![Value::from("1"), Value::Boolean(read_only)],
        ));
        let fault = failed(call(&mut s, "pack", vec![Value::F64(1e9)]));
        assert_eq!(fault, Fault::from(ZoeError::ReadOnly));
    }
}

#[test]
fn a_read_only_connection_packs_a_writable_store() {
    let storage = storage("pack-read-only-client", Queues::DEFAULT);
    let (mut writer, _frames) = connected(&storage);
    let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
    let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();

    // A read-only connection, as `zeopack` opens it.
    let (outbox, mut frames, _closing) = Outbox::new(10, 10);
    let mut reader = Client::new(Arc::clone(&storage), outbox);
    let register = vec![Value::from("1"), Value::Boolean(true)];
    assert_eq!(done(call(&mut reader, "register", register)), p64(new));
    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    assert_eq!(done(call(&mut reader, "pack", pack)), Value::Nil);
    assert_eq!(
        frames.try_recv().map(|frame| frame.to_vec()),
        Some(info_of(&storage))
    );

    // The pack removed the old revision of the root and kept the new one.
    let fault = failed(call(&mut reader, "loadSerial", vec![p64(0), p64(old)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(0) }));
    let load = done(call(&mut reader, "loadSerial", vec![p64(0), p64(new)]));
    assert_eq!(load, Value::Binary(MAPPING.to_vec()));
    // The connection stays read-only for a commit.
    assert_eq!(
        failed(call(&mut reader, "new_oids", vec![])),
        Fault::read_only()
    );
}

#[test]
fn a_read_only_client_does_not_pack() {
    let storage = storage("pack-read-only-certificate", Queues::DEFAULT);
    let (mut writer, _frames) = connected(&storage);
    let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
    let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();

    // A client with a certificate of the read-only client CA.
    let (outbox, mut frames, _closing) = Outbox::new(10, 10);
    let session = Session::new(Arc::clone(&storage), outbox, never_stops());
    let mut reader = Client::of(session.read_only_client(true));
    let register = vec![Value::from("1"), Value::Boolean(true)];
    assert_eq!(done(call(&mut reader, "register", register)), p64(new));
    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    assert_eq!(failed(call(&mut reader, "pack", pack)), Fault::read_only());
    assert!(frames.try_recv().is_none());

    // The store keeps the old revision of the root.
    let load = done(call(&mut reader, "loadSerial", vec![p64(0), p64(old)]));
    assert_eq!(load, Value::Binary(MAPPING.to_vec()));
}

#[test]
fn a_pack_whose_sizes_do_not_read_replies_nil_and_sends_no_info() {
    let dir = std::env::temp_dir().join(format!("zoe-session-pack-sizes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Store::open(&dir).unwrap());
    // The sizes come from the directory of the storage. This one does
    // not exist, so they do not read.
    let missing = dir.join("missing");
    let storage = Arc::new(NamedStorage::new("1", &missing, store, Queues::DEFAULT));
    assert!(storage.size().is_err());
    let (mut writer, _frames) = connected(&storage);
    let old = commit(&mut writer, 1, 0, 0, MAPPING).unwrap();
    let new = commit(&mut writer, 2, 0, old, MAPPING).unwrap();
    let (mut packer, mut frames) = connected(&storage);

    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    assert_eq!(done(call(&mut packer, "pack", pack)), Value::Nil);
    assert!(frames.try_recv().is_none());
    // The pack is done.
    let fault = failed(call(&mut packer, "loadSerial", vec![p64(0), p64(old)]));
    assert_eq!(fault, Fault::from(ZoeError::NotFound { oid: Oid(0) }));
    let load = done(call(&mut packer, "loadSerial", vec![p64(0), p64(new)]));
    assert_eq!(load, Value::Binary(MAPPING.to_vec()));
}

#[test]
fn the_stop_of_the_server_stops_a_pack_between_two_batches() {
    let storage = storage("pack-stop", Queues::DEFAULT);
    let (stop, stopping) = Stopping::new();
    let (outbox, _frames, _closing) = Outbox::new(10, 10);
    let mut s = Client::stopping(Arc::clone(&storage), outbox, stopping);
    let register = vec![Value::from("1"), Value::Boolean(false)];
    done(call(&mut s, "register", register));
    // The root refers to nothing. The 1,500 other objects make two
    // batches of the pack.
    let oids: Vec<(u64, u64)> = (0..=1_500).map(|oid| (oid, 0)).collect();
    let tid = commit_with(&mut s, 1, Value::Map(vec![]), &oids, MAPPING);

    // The stop comes before the pack. The pack still runs its first
    // batch, which removes the oldest 1,000 keys, then stops.
    stop.send_replace(true);
    let pack = vec![Value::F64(4_102_444_799.0), Value::from(1)];
    let fault = failed(call(&mut s, "pack", pack.clone()));
    assert_eq!(fault, Fault::storage("pack stopped: the server stops"));
    let load = |s: &mut Client, oid: u64| call(s, "loadSerial", vec![p64(oid), p64(tid)]);
    assert_eq!(
        failed(load(&mut s, 1_000)),
        Fault::from(ZoeError::NotFound { oid: Oid(1_000) })
    );
    assert_eq!(done(load(&mut s, 1_001)), Value::Binary(MAPPING.to_vec()));
    drop(s);

    // A session that no stop reaches removes what the pack left.
    let (mut s, _frames) = connected(&storage);
    assert_eq!(done(call(&mut s, "pack", pack)), Value::Nil);
    assert_eq!(
        failed(load(&mut s, 1_500)),
        Fault::from(ZoeError::NotFound { oid: Oid(1_500) })
    );
    assert_eq!(done(load(&mut s, 0)), Value::Binary(MAPPING.to_vec()));
}
