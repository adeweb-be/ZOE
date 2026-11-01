//! The helpers that the tests of `zoe-client` share: a new directory that
//! the test removes at its end, a `zoe serve` in the test process on a
//! store in such a directory, the records and the commits of another
//! client, the bytes of an oid or a tid as a stock server sends them, and a
//! pool with a client cache.

// Each test binary uses a part of the helpers.
#![allow(dead_code)]

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmpv::Value;
use zoe_client::runtime::BoxFuture;
use zoe_client::{Cache, Client, Connect, Error, Options, Pool, Tokio};
use zoe_core::{Oid, Tid};
use zoe_server::serve::{ServeOptions, Server};

/// A new empty directory of a test, in the temporary directory. The drop
/// removes it. After a failure of the test, the directory stays, so that
/// its files can be examined.
pub struct Fresh(PathBuf);

impl Deref for Fresh {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Fresh {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Return a new empty directory for the test `name` of this test binary.
pub fn fresh_dir(name: &str) -> Fresh {
    let binary = env!("CARGO_CRATE_NAME");
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("zoe-client-{binary}-{name}-{pid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Fresh(dir)
}

/// A `zoe serve` in the test process, on the store `store` of a new
/// directory. The drop stops the server, then removes the directory.
pub struct Started {
    server: Server,
    _dir: Fresh,
}

impl Deref for Started {
    type Target = Server;

    fn deref(&self) -> &Server {
        &self.server
    }
}

/// Start a server for the test `name`.
pub fn start(name: &str) -> Started {
    start_with(name, ServeOptions::new)
}

/// Start a server for the test `name`, with the options that `options`
/// gives for the path of the store.
pub fn start_with(name: &str, options: impl FnOnce(PathBuf) -> ServeOptions) -> Started {
    let dir = fresh_dir(name);
    let server = Server::start(options(dir.join("store"))).unwrap();
    Started { server, _dir: dir }
}

/// A ZODB record of a `PersistentMapping` that holds `value`.
pub fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// Commit the stores `stores` through `client` in one transaction, and
/// return its tid.
pub async fn commit(client: &Client, stores: &[(Oid, Tid, Vec<u8>)]) -> Result<Tid, Error> {
    let mut txn = client.begin(b"user", b"description", b"").await?;
    for (oid, serial, data) in stores {
        txn.store(*oid, *serial, data).await?;
    }
    txn.vote().await?;
    txn.finish().await
}

/// Commit `data` for `oid` through `client`, as another client.
pub async fn commit_other(client: &Client, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    commit(client, &[(oid, serial, data.to_vec())])
        .await
        .unwrap()
}

/// Return the bin of the 8 big-endian bytes of `n`, as a stock server
/// sends an oid or a tid. The tests build it without `zoe-wire`.
pub fn bin8(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

/// Open a pool of at most `bound` connections that commit on `server`,
/// with a client cache of 1 MB. With `options`, each connection opens as
/// `Client::open` does with them. Else it connects as `Client::connect`
/// does.
pub async fn open_cached(server: &Server, bound: usize, options: Option<Options>) -> Pool {
    let addr = server.addr();
    let open = move || -> BoxFuture<Result<Client, Error>> {
        match options.clone() {
            Some(options) => {
                Box::pin(async move { Client::open(Arc::new(Tokio), addr, "1", &options).await })
            }
            None => Box::pin(Client::connect(addr, "1")),
        }
    };
    let listener = open().await.unwrap();
    let connect: Connect = Arc::new(open);
    let cache = Cache::new(1 << 20).unwrap();
    Pool::with_cache(Arc::new(Tokio), listener, connect, bound, Some(cache))
}
