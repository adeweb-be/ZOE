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
//! The body is plain text, one line for each storage. A reply to `HEAD`
//! has no body, whatever its status. The server reads one request on each
//! connection, sends one reply and closes the connection.
//! It never speaks ZEO on this port. The admin port is not a task of the
//! stop: it answers 503 to `/readyz` from the stop signal on, and closes
//! when the runtime stops, before the stores close.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

/// The longest request head, in bytes, with the empty line at its end. A
/// longer head gets 431.
const MAX_HEAD: usize = 8 * 1024;

/// The longest time that a client takes to send its request head. Then the
/// server closes the connection without a reply. A probe sends its head at
/// once, so this time is short: silent clients hold a place of
/// `MAX_CONNECTIONS` at most this long.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

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
            Some(stores) => Reply::text(Status::Ok, lines(stores, |_| "open")),
            None => Reply::text(Status::Unavailable, "starting\n".to_owned()),
        }
    }

    /// The reply to `/readyz`.
    fn ready(&self) -> Reply {
        let Some(stores) = self.stores.get() else {
            return Reply::text(Status::Unavailable, "starting\n".to_owned());
        };
        if *self.stopping.borrow() {
            return Reply::text(Status::Unavailable, "stopping\n".to_owned());
        }
        let body = lines(stores, |store| {
            if store.no_stale_locks {
                "ready"
            } else {
                "not ready: the lock table held entries at the open"
            }
        });
        if stores.iter().all(|store| store.no_stale_locks) {
            Reply::text(Status::Ok, body)
        } else {
            Reply::text(Status::Unavailable, body)
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

/// The status of a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    HeadTooLarge,
    Unavailable,
}

impl Status {
    /// Return the code and the reason phrase.
    fn line(self) -> (u16, &'static str) {
        match self {
            Self::Ok => (200, "OK"),
            Self::BadRequest => (400, "Bad Request"),
            Self::NotFound => (404, "Not Found"),
            Self::MethodNotAllowed => (405, "Method Not Allowed"),
            Self::HeadTooLarge => (431, "Request Header Fields Too Large"),
            Self::Unavailable => (503, "Service Unavailable"),
        }
    }
}

/// The content type of a plain text reply.
const TEXT: &str = "text/plain; charset=utf-8";

/// An HTTP reply.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    status: Status,
    content_type: &'static str,
    /// The header lines after the standard ones, each with its `\r\n`.
    headers: &'static str,
    body: String,
}

impl Reply {
    /// Return a plain text reply.
    fn text(status: Status, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: TEXT,
            headers: "",
            body: body.into(),
        }
    }

    /// Return the bytes of the reply. A reply to `HEAD` has no body, but
    /// keeps the length of the body.
    fn bytes(&self, head_only: bool) -> Vec<u8> {
        let (code, reason) = self.status.line();
        let mut out = format!(
            "HTTP/1.1 {code} {reason}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\
             {}\r\n",
            self.content_type,
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

/// Return the reply to the request head `head`.
fn respond(health: &Health, head: &[u8]) -> Reply {
    let bad = || Reply::text(Status::BadRequest, "bad request\n");
    let line = head.split(|&byte| byte == b'\n').next().unwrap_or_default();
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
        _ => return Reply::text(Status::NotFound, "not found\n"),
    };
    match method {
        "GET" | "HEAD" => reply,
        _ => Reply {
            headers: "Allow: GET, HEAD\r\n",
            ..Reply::text(Status::MethodNotAllowed, "method not allowed\n")
        },
    }
}

/// Tell if the request head `head` asks for `HEAD`. Then the reply has no
/// body, whatever its status.
fn head_only(head: &[u8]) -> bool {
    head.starts_with(b"HEAD ")
}

/// What the bytes of a request head show.
#[derive(Debug, PartialEq, Eq)]
enum Scan {
    /// The head ends at this length, with its empty line.
    End(usize),
    /// The head is longer than `MAX_HEAD`.
    TooLarge,
    /// The head needs more bytes.
    More,
}

/// Look for the end of the head in `head`, from `from` on. The bytes
/// before `from` were searched already. A head ends with an empty line:
/// `\r\n\r\n`, or `\n\n`, which some clients send. Only the first
/// `MAX_HEAD` bytes count.
fn scan(head: &[u8], from: usize) -> Scan {
    let limit = head.len().min(MAX_HEAD);
    // A line end of 3 bytes can start in the bytes searched already.
    let start = from.saturating_sub(3).min(limit);
    let searched = &head[..limit];
    for n in start..limit {
        if searched[n] != b'\n' {
            continue;
        }
        match searched.get(n + 1..) {
            Some([b'\n', ..]) => return Scan::End(n + 2),
            Some([b'\r', b'\n', ..]) => return Scan::End(n + 3),
            _ => {}
        }
    }
    if head.len() >= MAX_HEAD {
        Scan::TooLarge
    } else {
        Scan::More
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
    let reply = loop {
        let from = head.len();
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buffer[..n]),
        }
        match scan(&head, from) {
            Scan::End(end) => break respond(health, &head[..end]),
            Scan::TooLarge => break Reply::text(Status::HeadTooLarge, "request head too large\n"),
            Scan::More => {}
        }
    };
    if stream
        .write_all(&reply.bytes(head_only(&head)))
        .await
        .is_ok()
    {
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
        respond(health, head.as_bytes())
    }

    fn code(reply: &Reply) -> u16 {
        reply.status.line().0
    }

    #[test]
    fn a_server_whose_stores_are_not_open_is_neither_healthy_nor_ready() {
        let (_stop, stopping) = watch::channel(false);
        let health = Health::new(stopping);
        assert_eq!(code(&get(&health, "/healthz")), 503);
        assert_eq!(code(&get(&health, "/readyz")), 503);
        assert_eq!(get(&health, "/readyz").body, "starting\n");
    }

    #[test]
    fn open_stores_are_healthy_and_ready_and_each_has_a_line() {
        let (_stop, health) = health(&[true, true]);
        let live = get(&health, "/healthz");
        assert_eq!(
            (code(&live), live.body.as_str()),
            (200, "s0: open\ns1: open\n")
        );
        let ready = get(&health, "/readyz?verbose");
        assert_eq!(
            (code(&ready), ready.body.as_str()),
            (200, "s0: ready\ns1: ready\n")
        );
    }

    #[test]
    fn a_store_with_lock_entries_at_the_open_is_not_ready() {
        let (_stop, health) = health(&[true, false]);
        assert_eq!(code(&get(&health, "/healthz")), 200);
        let ready = get(&health, "/readyz");
        assert_eq!(code(&ready), 503);
        assert!(ready.body.contains("s1: not ready"), "{}", ready.body);
    }

    #[test]
    fn a_stop_makes_the_server_not_ready_and_keeps_it_healthy() {
        let (stop, health) = health(&[true]);
        stop.send_replace(true);
        assert_eq!(code(&get(&health, "/healthz")), 200);
        let ready = get(&health, "/readyz");
        assert_eq!((code(&ready), ready.body.as_str()), (503, "stopping\n"));
    }

    #[test]
    fn other_paths_methods_and_bad_requests() {
        let (_stop, health) = health(&[true]);
        assert_eq!(code(&get(&health, "/")), 404);
        assert_eq!(code(&get(&health, "/metrics")), 404);
        assert_eq!(code(&get(&health, "/healthz/")), 404);
        let reply = respond(&health, b"POST /readyz HTTP/1.1\r\n\r\n");
        assert_eq!(code(&reply), 405);
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
            assert_eq!(code(&respond(&health, head)), 400, "{head:?}");
        }
    }

    #[test]
    fn a_head_reply_has_the_length_of_the_body_and_no_body_whatever_its_status() {
        let (stop, health) = health(&[true]);
        let cases: [(&[u8], u16); 4] = [
            (b"HEAD /readyz HTTP/1.1\r\n\r\n", 200),
            (b"HEAD /nothing HTTP/1.1\r\n\r\n", 404),
            (b"HEAD /readyz HTTP/2.0\r\n\r\n", 400),
            (b"HEAD /readyz HTTP/1.1\r\n\r\n", 503),
        ];
        for (n, (head, status)) in cases.into_iter().enumerate() {
            if n == 3 {
                stop.send_replace(true);
            }
            assert!(head_only(head));
            let reply = respond(&health, head);
            assert_eq!(code(&reply), status);
            let text = String::from_utf8(reply.bytes(head_only(head))).unwrap();
            let length = format!("\r\nContent-Length: {}\r\n", reply.body.len());
            assert!(!reply.body.is_empty() && text.contains(&length), "{text}");
            assert!(text.ends_with("\r\n\r\n"), "{text}");
        }
        assert!(!head_only(b"GET /readyz HTTP/1.1\r\n\r\n"));
        let large = Reply::text(Status::HeadTooLarge, "request head too large\n");
        assert!(
            String::from_utf8(large.bytes(true))
                .unwrap()
                .ends_with("\r\n\r\n")
        );
    }

    #[test]
    fn a_head_ends_with_an_empty_line_of_either_form() {
        assert_eq!(
            scan(b"GET / HTTP/1.1\r\nHost: zoe\r\n\r\n", 0),
            Scan::End(29)
        );
        assert_eq!(scan(b"GET / HTTP/1.1\nHost: zoe\n\n", 0), Scan::End(26));
        assert_eq!(scan(b"GET / HTTP/1.1\r\nHost: zoe\r\n", 0), Scan::More);
        // The end arrives in two reads: the search looks back 3 bytes.
        assert_eq!(scan(b"GET / HTTP/1.1\r\n\r\n", 15), Scan::End(18));
        assert_eq!(scan(b"GET / HTTP/1.1\n\n", 15), Scan::End(16));
        let reply = respond(&health(&[true]).1, b"GET /readyz HTTP/1.1\nHost: zoe\n\n");
        assert_eq!(code(&reply), 200);
    }

    #[test]
    fn the_head_limit_is_exact_also_in_one_read() {
        let head = |len: usize| {
            let mut head = b"GET /readyz HTTP/1.1\r\nX: ".to_vec();
            head.resize(len - 4, b'a');
            head.extend_from_slice(b"\r\n\r\n");
            head
        };
        assert_eq!(scan(&head(MAX_HEAD), 0), Scan::End(MAX_HEAD));
        assert_eq!(scan(&head(MAX_HEAD + 1), 0), Scan::TooLarge);
        assert_eq!(scan(&head(MAX_HEAD + 100), 0), Scan::TooLarge);
        assert_eq!(scan(&[b'a'; MAX_HEAD - 1], 0), Scan::More);
        assert_eq!(scan(&[b'a'; MAX_HEAD], 0), Scan::TooLarge);
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

            // A head of the bound without its end gets 431.
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(&[b'a'; MAX_HEAD]).await.unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 431 "), "{response}");
        });
    }
}
