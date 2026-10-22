//! The open of a native client: its addresses, its TLS, the rounds of the
//! open with their retries, and the open of a new listener at a reconnect.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyException, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};
use zoe_client::{
    Address, Connect, Dropping, Endpoint, Error, Options, REFUSED_POLL, Tls, TlsOptions, Tokio, lock,
};

/// The time between two attempts to open a client, until `wait_timeout`.
const RETRY: Duration = Duration::from_millis(100);

/// The server name of TLS on a unix domain socket without
/// `server_hostname`. The client then does not check the name, so any
/// valid name does.
const UNIX_SERVER_NAME: &str = "localhost";

/// An error of the open of a client.
pub(super) enum Opening {
    /// An address or a server name is not valid.
    Address(String),
    /// The files of TLS do not read, or the settings of TLS do not fit an
    /// address.
    Tls(String),
    /// The runtime does not start.
    Runtime(std::io::Error),
    /// The client does not open.
    Client(Error),
}

impl From<Error> for Opening {
    fn from(error: Error) -> Self {
        Self::Client(error)
    }
}

impl Opening {
    /// Return the error of the client for an attempt to connect again.
    pub(super) fn into_error(self) -> Error {
        match self {
            Opening::Address(text) | Opening::Tls(text) => Error::Invalid(text),
            Opening::Runtime(error) => Error::Io(error),
            Opening::Client(error) => error,
        }
    }
}

/// What a client connects to: its addresses, its TLS, its storage, the
/// options of its `register` and the longest wait of one open. Each open
/// reads the addresses when it starts.
pub(super) struct Servers {
    pub(super) targets: Mutex<Vec<Target>>,
    pub(super) secure: Option<Arc<Secure>>,
    pub(super) storage: String,
    pub(super) options: Options,
    pub(super) wait: Duration,
}

impl Servers {
    /// Open a listener as `options` asks, and return it with the `Connect`
    /// of its server. With `retry`, try again after failed connections
    /// until the wait passes. Else make one round. A server of a protocol
    /// of `fall_back_on` ends the open at once.
    pub(super) async fn open(
        &self,
        options: &Options,
        retry: bool,
        fall_back_on: &[Vec<u8>],
    ) -> Result<(zoe_client::Client, Connect), Opening> {
        let targets = lock(&self.targets).clone();
        let (listener, endpoint) = open(
            &targets,
            self.secure.as_deref(),
            &self.storage,
            options,
            self.wait,
            retry,
            fall_back_on,
        )
        .await?;
        // The other connections register as the listener did, on its
        // server, without a fallback.
        let options = Options {
            read_only: listener.read_only(),
            read_only_fallback: false,
            ..options.clone()
        };
        let storage = self.storage.clone();
        let connect: Connect = Arc::new(move || {
            let (endpoint, storage, options) = (endpoint.clone(), storage.clone(), options.clone());
            Box::pin(async move {
                zoe_client::Client::open_endpoint(Arc::new(Tokio), &endpoint, &storage, &options)
                    .await
            })
        });
        Ok((listener, connect))
    }

    /// Open a new listener for the pool, in one round. With `writable`, open
    /// only a writable one.
    pub(super) async fn reopen(&self, writable: bool) -> Result<(zoe_client::Client, Connect), Error> {
        let options = match writable {
            true => Options {
                read_only: false,
                read_only_fallback: false,
                ..self.options.clone()
            },
            false => self.options.clone(),
        };
        self.open(&options, false, &[])
            .await
            .map_err(Opening::into_error)
    }
}

/// The files and the checks of TLS that `zoe.zeo` gives: the certificate
/// or `None`, the key or `None`, the CA file or directory of `authenticate`
/// or `None`, `check_hostname` and `server_hostname` or `None`.
pub(super) type TlsArgs = (
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
    bool,
    Option<String>,
);

/// The TLS of the endpoints: the loaded files and the name settings.
pub(super) struct Secure {
    tls: Tls,
    check_hostname: bool,
    server_hostname: Option<String>,
}

impl Secure {
    /// Read the files of `args`. An error names the file.
    pub(super) fn load(args: &TlsArgs) -> Result<Self, Opening> {
        let (certificate, key, authenticate, check_hostname, server_hostname) = args;
        let mut options = match (certificate, authenticate) {
            (Some(certificate), Some(authenticate)) => TlsOptions::new(certificate, authenticate),
            (Some(certificate), None) => TlsOptions::system(certificate),
            (None, authenticate) => TlsOptions::without_certificate(authenticate.as_deref()),
        };
        if let Some(key) = key {
            options = options.key(key);
        }
        let options = options.check_hostname(*check_hostname);
        Ok(Self {
            tls: Tls::load(&options).map_err(Opening::Tls)?,
            check_hostname: *check_hostname,
            server_hostname: server_hostname.clone(),
        })
    }
}

/// An address that `zoe.zeo` gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// A host and a port.
    Tcp(String, u16),
    /// The path of a unix domain socket.
    Unix(PathBuf),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Tcp(host, port) => write!(f, "{host}:{port}"),
            Target::Unix(path) => write!(f, "{}", path.display()),
        }
    }
}

impl Target {
    /// Read a `(host, port)` tuple, or the path of a unix domain socket as
    /// a `str`. Another value raises `TypeError`.
    pub(super) fn extract(address: &Bound<'_, PyAny>) -> PyResult<Self> {
        if let Ok(path) = address.cast::<PyString>() {
            return Ok(Target::Unix(PathBuf::from(path.to_str()?)));
        }
        if let Ok(pair) = address.cast::<PyTuple>()
            && let Ok((host, port)) = pair.extract::<(String, u16)>()
        {
            return Ok(Target::Tcp(host, port));
        }
        Err(PyTypeError::new_err(format!(
            "an address is (host, port) or the path of a unix socket, not {address}"
        )))
    }
}

/// The endpoints of one round of the open, and the error of each host that
/// did not resolve.
struct Round {
    endpoints: Vec<Endpoint>,
    unresolved: Vec<String>,
}

/// Return the endpoints of `targets`, each with TLS when `secure` is given.
/// `resolve` gives the IP address of a host and a port. A host that does
/// not resolve is left out of the round, and its error is kept. A setting
/// that does not fit an address fails with `Opening`.
pub(super) fn endpoints(
    targets: &[Target],
    secure: Option<&Secure>,
    resolve: impl Fn(&str, u16) -> Result<SocketAddr, String>,
) -> Result<Round, Opening> {
    let mut round = Round {
        endpoints: Vec::new(),
        unresolved: Vec::new(),
    };
    for target in targets {
        let (endpoint, host) = match target {
            Target::Tcp(host, port) => match resolve(host, *port) {
                Ok(addr) => (Endpoint::new(addr), Some(host.as_str())),
                Err(error) => {
                    round.unresolved.push(error);
                    continue;
                }
            },
            Target::Unix(path) => (Endpoint::new(Address::Unix(path.clone())), None),
        };
        let Some(secure) = secure else {
            round.endpoints.push(endpoint);
            continue;
        };
        let name = match (secure.server_hostname.as_deref(), host) {
            (Some(name), _) | (None, Some(name)) => name,
            (None, None) if secure.check_hostname => {
                return Err(Opening::Tls(format!(
                    "TLS on the unix socket {target} checks the name of the server: \
                     give server_hostname"
                )));
            }
            (None, None) => UNIX_SERVER_NAME,
        };
        let endpoint = endpoint
            .tls(secure.tls.clone(), name)
            .map_err(|error| Opening::Address(error.to_string()))?;
        round.endpoints.push(endpoint);
    }
    Ok(round)
}

/// Resolve the host of each TCP address of `targets`, as `resolve` does,
/// on the blocking threads of the runtime. A lookup of the system can wait
/// for a long time: on a worker thread it would stop the connections of
/// every client of the process. An IP address needs no lookup, so it does
/// not go to a blocking thread.
async fn resolve_hosts(targets: &[Target]) -> HashMap<(String, u16), Result<SocketAddr, String>> {
    let mut resolved = HashMap::new();
    for target in targets {
        let Target::Tcp(host, port) = target else {
            continue;
        };
        let key = (host.clone(), *port);
        if resolved.contains_key(&key) {
            continue;
        }
        let addr = match literal(host, *port) {
            Some(addr) => Ok(addr),
            None => {
                let (name, port) = key.clone();
                let lookup = tokio::task::spawn_blocking(move || resolve(&name, port)).await;
                lookup.unwrap_or_else(|e| Err(format!("the address {host}:{port}: {e}")))
            }
        };
        resolved.insert(key, addr);
    }
    resolved
}

/// Return the socket address of `host` and `port` when `host` is an IP
/// address, or `None` for a name.
fn literal(host: &str, port: u16) -> Option<SocketAddr> {
    host.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

/// Return the first IP address of `host` and `port`.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("the address {host}:{port}: {e}"))?;
    addrs
        .next()
        .ok_or_else(|| format!("the address {host}:{port} has no IP address"))
}

/// Open a client to the first server of `targets` that opens, on the
/// runtime of the caller, and return it with its endpoint. Each round
/// resolves the hosts again. With `retry`, try again after failed
/// connections until `wait` passes. A server of a protocol of
/// `fall_back_on` ends the open at once with `Error::OtherProtocol`.
async fn open(
    targets: &[Target],
    secure: Option<&Secure>,
    storage: &str,
    options: &Options,
    wait: Duration,
    retry: bool,
    fall_back_on: &[Vec<u8>],
) -> Result<(zoe_client::Client, Endpoint), Opening> {
    let deadline = Instant::now() + wait;
    loop {
        let resolved = resolve_hosts(targets).await;
        let round = endpoints(targets, secure, |host, port| {
            resolved
                .get(&(host.to_owned(), port))
                .cloned()
                .unwrap_or_else(|| Err(format!("the address {host}:{port} was not resolved")))
        })?;
        let left = deadline.saturating_duration_since(Instant::now());
        let opened = if round.endpoints.is_empty() {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                round.unresolved.join("; "),
            )))
        } else {
            let runtime = Arc::new(Tokio);
            zoe_client::Client::open_first(runtime, &round.endpoints, storage, options, left).await
        };
        match opened {
            Err(Error::OtherProtocol(theirs)) if fall_back_on.contains(&theirs) => {
                return Err(Opening::Client(Error::OtherProtocol(theirs)));
            }
            Err(Error::OtherProtocol(theirs)) => {
                // ZEO logs a server of another protocol as a failed
                // registration, and tries again after its
                // `register_failed_poll`, until the wait passes.
                log_other_protocol(&theirs);
                if !retry {
                    return Err(Opening::Client(Error::OtherProtocol(theirs)));
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left <= REFUSED_POLL {
                    tokio::time::sleep(left).await;
                    return Err(Opening::Client(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        timed_out(&theirs),
                    ))));
                }
                tokio::time::sleep(REFUSED_POLL).await;
            }
            Err(Error::Io(_) | Error::Disconnected)
                if retry && Instant::now() + RETRY < deadline =>
            {
                tokio::time::sleep(RETRY).await;
            }
            Err(Error::Io(error)) => {
                let addresses: Vec<String> = targets.iter().map(Target::to_string).collect();
                let mut text = format!(
                    "no connection to {} within {wait:?}: {error}",
                    addresses.join(", ")
                );
                if !round.endpoints.is_empty() && !round.unresolved.is_empty() {
                    text = format!("{text}; {}", round.unresolved.join("; "));
                }
                return Err(Opening::Client(Error::Io(std::io::Error::new(
                    error.kind(),
                    text,
                ))));
            }
            opened => {
                let client = opened?;
                let endpoint = round
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.address() == client.address())
                    .cloned()
                    .ok_or_else(|| Error::Protocol("the server has no endpoint".into()))?;
                return Ok((client, endpoint));
            }
        }
    }
}

/// Return the `Dropping` function of the pool for the Python callable
/// `dropping`: a thread of its own calls `dropping(reason)`, so the code of
/// the callable never runs on a thread of the runtime, and the pool waits
/// for the end of the call. An error of the callable is lost.
pub(super) fn dropping_of(dropping: Py<PyAny>) -> Dropping {
    let dropping = Arc::new(dropping);
    Arc::new(move |reason: String| {
        let dropping = dropping.clone();
        let (done, called) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            Python::try_attach(|py| {
                if let Err(error) = dropping.call1(py, (reason,)) {
                    error.print(py);
                }
            });
            let _ = done.send(());
        });
        Box::pin(async move {
            let _ = called.await;
        })
    })
}

/// The text of `ClientDisconnected` when the open saw only servers of
/// another protocol until the wait passed: the text of ZEO.
const TIMED_OUT: &str = "timed out waiting for connection";

/// The pickle protocol of a stock ZEO 6 server without `msgpack true`.
const PICKLE: &[u8] = b"Z5";

/// Return the text of `ClientDisconnected` when the last server of the
/// open spoke `theirs`: the text of ZEO. For `Z5`, the text also tells
/// how to reach that server.
fn timed_out(theirs: &[u8]) -> String {
    if theirs == PICKLE {
        format!(
            "{TIMED_OUT}: the server speaks the pickle protocol Z5, and the \
             native client speaks only M5: set msgpack true on the server, \
             or give native=False"
        )
    } else {
        TIMED_OUT.to_owned()
    }
}

pyo3::create_exception!(
    _zoe,
    OtherProtocol,
    PyException,
    "The server speaks a protocol of `fall_back_on`: its argument."
);

/// Log a server of another protocol as ZEO logs a failed registration:
/// at error level, under `ZEO.asyncio.client`, with the protocol of the
/// server as bytes. Without a Python interpreter, log nothing.
fn log_other_protocol(theirs: &[u8]) {
    let _ = Python::try_attach(|py| -> PyResult<()> {
        let logger = py
            .import("logging")?
            .call_method1("getLogger", ("ZEO.asyncio.client",))?;
        let protocol = PyBytes::new(py, theirs);
        logger.call_method1(
            "error",
            ("Registration or cache validation failed, %s", protocol),
        )?;
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unix_socket_has_no_resolution() {
        let targets = [
            Target::Unix(PathBuf::from("./zeo.sock")),
            Target::Tcp("127.0.0.1".into(), 8100),
        ];
        let round = endpoints(&targets, None, resolve).ok().unwrap();
        assert!(round.unresolved.is_empty());
        let endpoints = round.endpoints;
        let addresses: Vec<_> = endpoints.iter().map(Endpoint::address).collect();
        assert_eq!(
            addresses,
            [
                &Address::Unix(PathBuf::from("./zeo.sock")),
                &Address::Tcp("127.0.0.1:8100".parse().unwrap()),
            ]
        );
        assert!(endpoints.iter().all(|endpoint| !endpoint.is_tls()));
    }

    #[test]
    fn a_tls_file_that_does_not_read_fails_the_open_and_names_it() {
        let missing = std::env::temp_dir().join(format!("zoe-py-missing-{}", std::process::id()));
        let tls = (
            Some(missing.join("client.pem")),
            None,
            Some(missing.join("ca.pem")),
            true,
            None,
        );
        match Secure::load(&tls) {
            Err(Opening::Tls(text)) => assert!(text.contains("client.pem"), "{text}"),
            _ => panic!("the open did not fail on the missing file"),
        }
    }

    #[test]
    fn a_host_that_does_not_resolve_leaves_the_others_in_the_round() {
        let targets = [
            Target::Tcp("no-such-host".into(), 8100),
            Target::Tcp("127.0.0.1".into(), 8100),
        ];
        let fake = |host: &str, port: u16| match host {
            "no-such-host" => Err(format!("the address {host}:{port}: not found")),
            _ => resolve(host, port),
        };
        let round = endpoints(&targets, None, fake).ok().unwrap();
        assert_eq!(round.endpoints.len(), 1);
        assert_eq!(
            round.endpoints[0].address(),
            &Address::Tcp("127.0.0.1:8100".parse().unwrap())
        );
        assert_eq!(round.unresolved.len(), 1);
        assert!(round.unresolved[0].contains("no-such-host"));
    }

    #[test]
    fn an_address_shows_as_zeo_gives_it() {
        assert_eq!(Target::Tcp("db".into(), 8100).to_string(), "db:8100");
        assert_eq!(Target::Unix("./zeo.sock".into()).to_string(), "./zeo.sock");
    }

    #[test]
    fn an_ip_address_needs_no_lookup() {
        assert_eq!(
            literal("127.0.0.1", 8100),
            Some("127.0.0.1:8100".parse().unwrap())
        );
        assert_eq!(literal("::1", 8100), Some("[::1]:8100".parse().unwrap()));
        assert_eq!(literal("localhost", 8100), None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let targets = [Target::Tcp("127.0.0.1".into(), 8100)];
        let resolved = runtime.block_on(resolve_hosts(&targets));
        assert_eq!(
            resolved[&("127.0.0.1".to_owned(), 8100)],
            Ok("127.0.0.1:8100".parse().unwrap())
        );
    }

    #[test]
    fn a_host_name_resolves_on_a_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let targets = [
            Target::Tcp("localhost".into(), 8100),
            Target::Unix(PathBuf::from("./zeo.sock")),
        ];
        let resolved = runtime.block_on(resolve_hosts(&targets));
        assert_eq!(resolved.len(), 1);
        let addr = resolved[&("localhost".to_owned(), 8100)].clone().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 8100);
    }

    #[test]
    fn an_ip_address_resolves_to_itself() {
        assert_eq!(
            resolve("127.0.0.1", 8100).ok(),
            Some("127.0.0.1:8100".parse().unwrap())
        );
    }
}
