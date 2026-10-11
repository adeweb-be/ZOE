//! Acceptance test of the live tail of Loupe (F15).
//!
//! In server mode, `Remote::tail()` follows the commits of the store as the
//! invalidations of the server announce them, as a scrolling commit log.
//! Each `Commit` gives the tid, the user, the description, the oids with
//! the class of each record, and the size of the records. The tail reads
//! the transaction of each invalidation through the iterator of the
//! server, so it holds what the store holds. Commits come in tid order,
//! each once. `Tail::next(timeout)` waits at most `timeout` for the next
//! commit.
//!
//! Lock waits and the conflicts of other clients are not on the ZEO wire;
//! the tail shows them when the server gives them, after phase 2.

mod common;

use std::time::Duration;

use zoe_client::Client;
use zoe_core::{Oid, Tid};
use zoe_loupe::Remote;
use zoe_server::serve::{ServeOptions, Server};

use common::{fresh_dir, write_store};

const TIMEOUT: Duration = Duration::from_secs(30);

fn record(class: &str, value: &[u8]) -> Vec<u8> {
    let (module, name) = class.rsplit_once('.').unwrap();
    let mut pickle = b"\x80\x03c".to_vec();
    pickle.extend_from_slice(module.as_bytes());
    pickle.push(b'\n');
    pickle.extend_from_slice(name.as_bytes());
    pickle.extend_from_slice(b"\nq\x00.");
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

#[test]
fn the_tail_shows_each_commit_with_its_metadata() {
    let dir = fresh_dir("tail");
    write_store(&dir);
    let server = Server::start(ServeOptions::new(&dir)).unwrap();
    let remote = Remote::connect(server.addr(), "1").unwrap();
    let mut tail = remote.tail().unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (first, second, oids) = runtime.block_on(async {
        let client = Client::connect(server.addr(), "1").await.unwrap();
        let oids = client.new_oids(2).await.unwrap();
        let mut txn = client.begin(b"editor", b"first edit", b"").await.unwrap();
        txn.store(oids[0], Tid(0), &record("app.content.Document", b"a"))
            .await
            .unwrap();
        txn.store(oids[1], Tid(0), &record("app.content.Folder", b"bb"))
            .await
            .unwrap();
        txn.vote().await.unwrap();
        let first = txn.finish().await.unwrap();

        let mut txn = client.begin(b"admin", b"second edit", b"").await.unwrap();
        txn.store(oids[0], first, &record("app.content.Document", b"ccc"))
            .await
            .unwrap();
        txn.vote().await.unwrap();
        let second = txn.finish().await.unwrap();
        client.close().await;
        (first, second, oids)
    });

    let one = tail.next(TIMEOUT).unwrap().expect("no first commit");
    assert_eq!(one.tid, first);
    assert_eq!(one.user, "editor");
    assert_eq!(one.description, "first edit");
    let mut records: Vec<(Oid, String)> = one
        .records
        .iter()
        .map(|r| (r.oid, r.class.clone().unwrap()))
        .collect();
    records.sort();
    assert_eq!(
        records,
        vec![
            (oids[0], "app.content.Document".to_string()),
            (oids[1], "app.content.Folder".to_string())
        ]
    );
    assert_eq!(
        one.size,
        (record("app.content.Document", b"a").len() + record("app.content.Folder", b"bb").len())
            as u64
    );

    let two = tail.next(TIMEOUT).unwrap().expect("no second commit");
    assert_eq!(two.tid, second);
    assert_eq!(two.user, "admin");
    assert_eq!(two.records.len(), 1);

    // Nothing more.
    assert!(tail.next(Duration::from_millis(300)).unwrap().is_none());
    drop(tail);
    drop(remote);
    server.stop();
}
