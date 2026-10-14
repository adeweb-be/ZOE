//! Acceptance test of `zoe-client`, the Rust client of the ZEO 6 msgpack
//! protocol.
//!
//! The client speaks the protocol of `ZEO.ClientStorage`: it answers the
//! protocol of the server, registers with a storage, and calls the methods
//! of the server with its own message ids. Its methods are `async fn` on
//! tokio. Many calls of one client can be in flight at once, and each
//! reply goes to its call. The calls from the server
//! (`invalidateTransaction`, `info`) go to a channel of invalidations.
//!
//! The test runs `zoe serve` in its own process with
//! `zoe_server::serve::Server`.

use std::time::Duration;

use zoe_client::{Client, Error};
use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server};

const TIMEOUT: Duration = Duration::from_secs(30);

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zoe-client-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A ZODB record of a `PersistentMapping` that holds `value`.
fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

async fn commit(client: &Client, stores: &[(Oid, Tid, Vec<u8>)]) -> Result<Tid, Error> {
    let mut txn = client.begin(b"user", b"description", b"").await?;
    for (oid, serial, data) in stores {
        txn.store(*oid, *serial, data).await?;
    }
    txn.vote().await?;
    txn.finish().await
}

#[tokio::test]
async fn a_client_registers_commits_and_loads() {
    let dir = fresh_dir("commit");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let client = Client::connect(server.addr(), "1").await.unwrap();
    assert_eq!(client.last_transaction(), Tid(0));

    let oids = client.new_oids(2).await.unwrap();
    assert_eq!(oids.len(), 2);
    let tid = commit(&client, &[(oids[0], Tid(0), record(b"one"))])
        .await
        .unwrap();
    assert_eq!(client.last_transaction(), tid);
    assert_eq!(client.load(oids[0]).await.unwrap(), (record(b"one"), tid));

    let second = commit(&client, &[(oids[0], tid, record(b"two"))])
        .await
        .unwrap();
    let before = client.load_before(oids[0], second).await.unwrap().unwrap();
    assert_eq!(before.data, record(b"one"));
    assert_eq!(before.tid, tid);
    assert_eq!(before.end, Some(second));

    let history = client.history(oids[0], 10).await.unwrap();
    assert_eq!(
        history.iter().map(|entry| entry.tid).collect::<Vec<_>>(),
        vec![second, tid]
    );

    // An oid without a record is an error of the server.
    match client.load(oids[1]).await {
        Err(Error::Server { class, .. }) => {
            assert_eq!(class, "ZODB.POSException.POSKeyError")
        }
        other => panic!("not a POSKeyError: {other:?}"),
    }
    client.close().await;
    server.stop();
}

#[tokio::test]
async fn many_loads_run_at_once_on_one_client() {
    let dir = fresh_dir("concurrent");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let client = Client::connect(server.addr(), "1").await.unwrap();
    let oids = client.new_oids(100).await.unwrap();
    let stores: Vec<_> = oids
        .iter()
        .map(|oid| (*oid, Tid(0), record(&oid.0.to_be_bytes())))
        .collect();
    let tid = commit(&client, &stores).await.unwrap();

    let loads = oids.iter().map(|oid| client.load(*oid));
    let results = futures::future::join_all(loads).await;
    for (oid, result) in oids.iter().zip(results) {
        assert_eq!(result.unwrap(), (record(&oid.0.to_be_bytes()), tid));
    }
    client.close().await;
    server.stop();
}

#[tokio::test]
async fn a_stale_serial_is_a_conflict_with_its_serials() {
    let dir = fresh_dir("conflict");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let a = Client::connect(server.addr(), "1").await.unwrap();
    let b = Client::connect(server.addr(), "1").await.unwrap();
    let oid = a.new_oids(1).await.unwrap()[0];
    let first = commit(&a, &[(oid, Tid(0), record(b"1"))]).await.unwrap();
    let second = commit(&a, &[(oid, first, record(b"2"))]).await.unwrap();

    match commit(&b, &[(oid, first, record(b"3"))]).await {
        Err(Error::Conflict { oid: at, serials }) => {
            assert_eq!(at, oid);
            assert_eq!(serials, Some((second, first)));
        }
        other => panic!("not a conflict: {other:?}"),
    }
    a.close().await;
    b.close().await;
    server.stop();
}

#[tokio::test]
async fn a_client_gets_the_invalidations_of_the_others() {
    let dir = fresh_dir("invalidations");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let writer = Client::connect(server.addr(), "1").await.unwrap();
    let reader = Client::connect(server.addr(), "1").await.unwrap();
    let mut invalidations = reader.invalidations();

    let oid = writer.new_oids(1).await.unwrap()[0];
    let tid = commit(&writer, &[(oid, Tid(0), record(b"x"))])
        .await
        .unwrap();

    let invalidation = tokio::time::timeout(TIMEOUT, invalidations.recv())
        .await
        .expect("no invalidation came")
        .unwrap();
    assert_eq!(invalidation.tid, tid);
    assert_eq!(invalidation.oids, vec![oid]);
    // The reader's view of the last transaction follows.
    assert_eq!(reader.last_transaction(), tid);
    writer.close().await;
    reader.close().await;
    server.stop();
}

#[tokio::test]
async fn a_dead_server_gives_disconnected() {
    let dir = fresh_dir("disconnected");
    let server = Server::start(ServeOptions::new(dir.join("store"))).unwrap();
    let client = Client::connect(server.addr(), "1").await.unwrap();
    server.stop();
    match tokio::time::timeout(TIMEOUT, client.new_oids(1)).await {
        Ok(Err(Error::Disconnected)) => {}
        other => panic!("not disconnected: {other:?}"),
    }
}
