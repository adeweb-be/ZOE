//! The admin port of `zoe serve`: a small HTTP/1.1 server for the probes
//! of a supervisor, such as the liveness and readiness probes of
//! Kubernetes.
//!
//! - `GET /healthz` gives 200 while every store is open.
//! - `GET /readyz` gives 200 when every store loaded its last tid and its
//!   lock table held no entry at the open, and the server does not stop.
//!   Otherwise it gives 503.
//! - Each other path gives 404. A known path with a method other than
//!   `GET` or `HEAD` gives 405.
//!
//! The body is plain text, one line for each storage. The server reads one
//! request on each connection, sends one reply and closes the connection.
//! It never speaks ZEO on this port. The admin port is not a task of the
//! stop: it answers 503 to `/readyz` from the stop signal on, and closes
//! when the runtime stops, before the stores close.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

/// The longest request head, in bytes. A longer head gets 431.
const MAX_HEAD: usize = 8 * 1024;

/// The longest time that a client takes to send its request head. Then the
/// server closes the connection without a reply.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The most admin connections that the server serves at the same time.
/// The next connection waits in the backlog of the port.
const MAX_CONNECTIONS: usize = 64;

/// The state that the probes read. It holds no store, so it never keeps a
/// store open.
pub(crate) struct Health {
    /// The stores, in the order of the configuration. It is empty until
    /// every store is open.
    stores: OnceLock<Vec<StoreHealth>>,
    /// The stop signal of the server. `true` from the stop on.
    stopping: watch::Receiver<bool>,
}

/// What the server found when it opened one store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreHealth {
    /// The storage name.
    pub(crate) name: String,
    /// True when the lock table of the store held no entry after the open.
    pub(crate) no_stale_locks: bool,
}

impl Health {
    /// Return the state of a server whose stores are not open yet.
    /// `stopping` is the stop signal of the server.
    pub(crate) fn new(stopping: watch::Receiver<bool>) -> Arc<Self> {
        Arc::new(Self {
            stores: OnceLock::new(),
            stopping,
        })
    }

    /// Record that every store is open. The open of a store loads its last
    /// tid.
    pub(crate) fn opened(&self, stores: Vec<StoreHealth>) {
        let _ = self.stores.set(stores);
    }

    /// The reply to `/healthz`.
    fn live(&self) -> Reply {
        match self.stores.get() {
            Some(stores) => Reply::ok(lines(stores, |_| "open")),
            None => Reply::unavailable("starting\n".to_owned()),
        }
    }

    /// The reply to `/readyz`.
    fn ready(&self) -> Reply {
        let Some(stores) = self.stores.get() else {
            return Reply::unavailable("starting\n".to_owned());
        };
        if *self.stopping.borrow() {
            return Reply::unavailable("stopping\n".to_owned());
        }
        let body = lines(stores, |store| {
            if store.no_stale_locks {
                "ready"
            } else {
                "not ready: the lock table held entries at the open"
            }
        });
        if stores.iter().all(|store| store.no_stale_locks) {
            Reply::ok(body)
        } else {
            Reply::unavailable(body)
        }
    }
}

/// Return one line for each store: its name, then `state`.
fn lines(stores: &[StoreHealth], state: impl Fn(&StoreHealth) -> &'static str) -> String {
    stores
        .iter()
        .map(|store| format!("{}: {}\n", store.name, state(store)))
        .collect()
}

/// An HTTP reply.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    status: u16,
    /// The header lines after the standard ones, each with its `\r\n`.
    headers: &'static str,
    body: String,
}

impl Reply {
    fn ok(body: String) -> Self {
        Self::new(200, body)
    }

    fn unavailable(body: String) -> Self {
        Self::new(503, body)
    }

    fn new(status: u16, body: String) -> Self {
        Self {
            status,
            headers: "",
            body,
        }
    }

    /// Return the bytes of the reply. A reply to `HEAD` has no body.
    fn bytes(&self, head_only: bool) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            431 => "Request Header Fields Too Large",
            _ => "Service Unavailable",
        };
        let mut out = format!(
            "HTTP/1.1 {} {reason}\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\
             {}\r\n",
            self.status,
            self.body.len(),
            self.headers,
        )
        .into_bytes();
        if !head_only {
            out.extend_from_slice(self.body.as_bytes());
        }
        out
    }
}

/// Return the reply to the request head `head`, and tell if the request is
/// `HEAD`.
fn respond(health: &Health, head: &[u8]) -> (Reply, bool) {
    let bad = || (Reply::new(400, "bad request\n".to_owned()), false);
    let Some(line) = head.split(|&byte| byte == b'\n').next() else {
        return bad();
    };
    let Ok(line) = std::str::from_utf8(line) else {
        return bad();
    };
    let mut parts = line.trim_end_matches('\r').split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return bad();
    };
    if !version.starts_with("HTTP/1.") {
        return bad();
    }
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    let reply = match path {
        "/healthz" => health.live(),
        "/readyz" => health.ready(),
        // Each new path of the admin port, `/metrics` too, goes here.
        _ => return (Reply::new(404, "not found\n".to_owned()), false),
    };
    match method {
        "GET" => (reply, false),
        "HEAD" => (reply, true),
        _ => (
            Reply {
                status: 405,
                headers: "Allow: GET, HEAD\r\n",
                body: "method not allowed\n".to_owned(),
            },
            false,
        ),
    }
}

/// Serve the admin port on `listener` until the runtime stops.
pub(crate) async fn serve(listener: TcpListener, health: Arc<Health>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            return;
        };
        match listener.accept().await {
            Ok((stream, _)) => {
                let health = Arc::clone(&health);
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(READ_TIMEOUT, answer(stream, &health)).await;
                    drop(permit);
                });
            }
            Err(error) => {
                eprintln!("zoe serve: admin accept failed: {error}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Read one request head from `stream`, send the reply and close.
async fn answer(mut stream: TcpStream, health: &Health) {
    let mut head = Vec::with_capacity(1024);
    let mut buffer = [0; 1024];
    let (reply, head_only) = loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
        if head.windows(4).any(|window| window == b"\r\n\r\n") {
            break respond(health, &head);
        }
        if head.len() > MAX_HEAD {
            break (
                Reply::new(431, "request head too large\n".to_owned()),
                false,
            );
        }
    };
    if stream.write_all(&reply.bytes(head_only)).await.is_ok() {
        let _ = stream.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(locks: &[bool]) -> (watch::Sender<bool>, Arc<Health>) {
        let (stop, stopping) = watch::channel(false);
        let health = Health::new(stopping);
        health.opened(
            locks
                .iter()
                .enumerate()
                .map(|(n, &no_stale_locks)| StoreHealth {
                    name: format!("s{n}"),
                    no_stale_locks,
                })
                .collect(),
        );
        (stop, health)
    }

    fn get(health: &Health, path: &str) -> Reply {
        let head = format!("GET {path} HTTP/1.1\r\nHost: zoe\r\n\r\n");
        respond(health, head.as_bytes()).0
    }

    #[test]
    fn a_server_whose_stores_are_not_open_is_neither_healthy_nor_ready() {
        let (_stop, stopping) = watch::channel(false);
        let health = Health::new(stopping);
        assert_eq!(get(&health, "/healthz").status, 503);
        assert_eq!(get(&health, "/readyz").status, 503);
        assert_eq!(get(&health, "/readyz").body, "starting\n");
    }

    #[test]
    fn open_stores_are_healthy_and_ready_and_each_has_a_line() {
        let (_stop, health) = health(&[true, true]);
        let live = get(&health, "/healthz");
        assert_eq!(
            (live.status, live.body.as_str()),
            (200, "s0: open\ns1: open\n")
        );
        let ready = get(&health, "/readyz?verbose");
        assert_eq!(
            (ready.status, ready.body.as_str()),
            (200, "s0: ready\ns1: ready\n")
        );
    }

    #[test]
    fn a_store_with_lock_entries_at_the_open_is_not_ready() {
        let (_stop, health) = health(&[true, false]);
        assert_eq!(get(&health, "/healthz").status, 200);
        let ready = get(&health, "/readyz");
        assert_eq!(ready.status, 503);
        assert!(ready.body.contains("s1: not ready"), "{}", ready.body);
    }

    #[test]
    fn a_stop_makes_the_server_not_ready_and_keeps_it_healthy() {
        let (stop, health) = health(&[true]);
        stop.send_replace(true);
        assert_eq!(get(&health, "/healthz").status, 200);
        let ready = get(&health, "/readyz");
        assert_eq!((ready.status, ready.body.as_str()), (503, "stopping\n"));
    }

    #[test]
    fn other_paths_methods_and_bad_requests() {
        let (_stop, health) = health(&[true]);
        assert_eq!(get(&health, "/").status, 404);
        assert_eq!(get(&health, "/metrics").status, 404);
        assert_eq!(get(&health, "/healthz/").status, 404);
        let (reply, head_only) = respond(&health, b"POST /readyz HTTP/1.1\r\n\r\n");
        assert_eq!((reply.status, head_only), (405, false));
        assert!(
            String::from_utf8(reply.bytes(false))
                .unwrap()
                .contains("\r\nAllow: GET, HEAD\r\n")
        );
        for head in [
            &b"\r\n\r\n"[..],
            b"GET /readyz\r\n\r\n",
            b"GET /readyz HTTP/2.0\r\n\r\n",
            b"M5\r\n\r\n",
            b"\xff\xfe /readyz HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(respond(&health, head).0.status, 400, "{head:?}");
        }
    }

    #[test]
    fn a_head_reply_has_the_length_of_the_body_and_no_body() {
        let (_stop, health) = health(&[true]);
        let (reply, head_only) = respond(&health, b"HEAD /readyz HTTP/1.1\r\n\r\n");
        assert!(head_only);
        let text = String::from_utf8(reply.bytes(true)).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("\r\nContent-Length: 10\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text}");
    }

    #[test]
    fn the_port_answers_one_request_and_closes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_stop, health) = health(&[true]);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(serve(listener, health));
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /readyz HTTP/1.1\r\nHost: zoe\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
            assert!(response.ends_with("\r\n\r\ns0: ready\n"), "{response}");

            // A head longer than the bound gets 431.
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(&[b'a'; MAX_HEAD + 1]).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 431 "), "{response}");
        });
    }
}
