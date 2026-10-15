//! Where a client connects: a TCP address or the path of a unix domain
//! socket, with TLS or without.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;

use rustls::pki_types::ServerName;

use crate::Error;
use crate::runtime::{Runtime, Stream};
use crate::tls::Tls;

/// The address of a server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    /// A TCP address.
    Tcp(SocketAddr),
    /// The path of a unix domain socket.
    Unix(PathBuf),
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Tcp(addr) => write!(f, "{addr}"),
            Address::Unix(path) => write!(f, "{}", path.display()),
        }
    }
}

impl From<SocketAddr> for Address {
    fn from(addr: SocketAddr) -> Self {
        Address::Tcp(addr)
    }
}

/// A server to connect to: its address, and the TLS of the connection.
///
/// An endpoint with TLS never sends a frame of the protocol outside TLS.
#[derive(Clone)]
pub struct Endpoint {
    address: Address,
    tls: Option<(Tls, ServerName<'static>)>,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address)
            .field("tls", &self.tls.as_ref().map(|(_, name)| name))
            .finish()
    }
}

impl Endpoint {
    /// Connect to `address` without TLS.
    pub fn new(address: impl Into<Address>) -> Self {
        Self {
            address: address.into(),
            tls: None,
        }
    }

    /// Speak TLS with `tls` on the connection. `server_name` is the name
    /// that the client sends to the server and, when `tls` checks the name,
    /// the name that the server certificate must be valid for: a DNS name
    /// or an IP address. A name that is neither fails with
    /// `Error::Invalid`.
    pub fn tls(mut self, tls: Tls, server_name: &str) -> Result<Self, Error> {
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|error| Error::Invalid(format!("the server name {server_name:?}: {error}")))?;
        self.tls = Some((tls, name));
        Ok(self)
    }

    /// Return the address of the server.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Tell if the connection speaks TLS.
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Open the stream to the server through `runtime`. With TLS, run the
    /// handshake: a failed handshake fails the connection, and the client
    /// never falls back to a stream without TLS.
    pub(crate) async fn connect(&self, runtime: &dyn Runtime) -> io::Result<Box<dyn Stream>> {
        let stream = match &self.address {
            Address::Tcp(addr) => runtime.connect(*addr).await?,
            Address::Unix(path) => runtime.connect_unix(path.clone()).await?,
        };
        match &self.tls {
            Some((tls, name)) => tls.handshake(stream, name.clone()).await,
            None => Ok(stream),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_shows_as_its_socket_address_or_its_path() {
        let tcp = Address::from("127.0.0.1:8100".parse::<SocketAddr>().unwrap());
        assert_eq!(tcp.to_string(), "127.0.0.1:8100");
        let unix = Address::Unix(PathBuf::from("./zeo.sock"));
        assert_eq!(unix.to_string(), "./zeo.sock");
    }

    #[test]
    fn an_endpoint_without_tls_keeps_its_address() {
        let endpoint = Endpoint::new(Address::Unix(PathBuf::from("/run/zeo.sock")));
        assert_eq!(
            endpoint.address(),
            &Address::Unix(PathBuf::from("/run/zeo.sock"))
        );
        assert!(!endpoint.is_tls());
    }
}
