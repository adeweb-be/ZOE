//! TLS on the ZEO port: the files of [`TlsOptions`] and the handshake of
//! each connection.
//!
//! The server speaks TLS with rustls and its ring provider. A client CA
//! makes a client certificate necessary: the handshake refuses a client
//! without a certificate that chains to it. A client whose certificate
//! chains only to the read-only client CA registers read-only only.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// The files of TLS on the ZEO port, as PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    cert: PathBuf,
    key: PathBuf,
    client_ca: Option<PathBuf>,
    read_only_client_ca: Option<PathBuf>,
}

impl TlsOptions {
    /// Speak TLS with the certificate chain in `cert` and its private key
    /// in `key`. Without a client CA, the server asks for no client
    /// certificate.
    pub fn new(cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Self {
        Self {
            cert: cert.as_ref().to_path_buf(),
            key: key.as_ref().to_path_buf(),
            client_ca: None,
            read_only_client_ca: None,
        }
    }

    /// Require a client certificate that chains to a CA certificate in
    /// `ca`. Its holder registers as it asks.
    #[must_use]
    pub fn client_ca(mut self, ca: impl AsRef<Path>) -> Self {
        self.client_ca = Some(ca.as_ref().to_path_buf());
        self
    }

    /// Also accept a client certificate that chains to a CA certificate in
    /// `ca`. Its holder registers read-only only. A client certificate is
    /// then necessary, also without [`TlsOptions::client_ca`].
    #[must_use]
    pub fn read_only_client_ca(mut self, ca: impl AsRef<Path>) -> Self {
        self.read_only_client_ca = Some(ca.as_ref().to_path_buf());
        self
    }
}

/// The TLS of a server: the acceptor of the handshakes, and how it tells
/// a read-write client from a read-only one.
pub(crate) struct Tls {
    acceptor: TlsAcceptor,
    access: Access,
}

/// What a client certificate lets its holder register.
enum Access {
    /// No read-only client CA: each client registers as it asks.
    AsAsked,
    /// A read-only client CA and no client CA: each client registers
    /// read-only only.
    ReadOnly,
    /// Both CAs: a certificate that this verifier of the client CA accepts
    /// is read-write. Another one is read-only.
    Writers(Arc<dyn ClientCertVerifier>),
}

impl Tls {
    /// Read the files of `options`. Fail with a message that names the
    /// file that does not read, holds no PEM item or does not fit.
    pub(crate) fn load(options: &TlsOptions) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = certificates(&options.cert)?;
        let key = PrivateKeyDer::from_pem_file(&options.key)
            .map_err(|error| format!("{}: {error}", options.key.display()))?;
        let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?;
        let writers = options.client_ca.as_deref().map(roots).transpose()?;
        let readers = options
            .read_only_client_ca
            .as_deref()
            .map(roots)
            .transpose()?;
        let (builder, access) = match (writers, readers) {
            (None, None) => (builder.with_no_client_auth(), Access::AsAsked),
            (Some(writers), None) => (
                builder.with_client_cert_verifier(verifier(writers, &provider)?),
                Access::AsAsked,
            ),
            (writers, Some(mut all)) => {
                // The handshake accepts a certificate of either CA.
                let access = match writers {
                    None => Access::ReadOnly,
                    Some(writers) => {
                        all.roots.extend(writers.roots.iter().cloned());
                        Access::Writers(verifier(writers, &provider)?)
                    }
                };
                (
                    builder.with_client_cert_verifier(verifier(all, &provider)?),
                    access,
                )
            }
        };
        let config = builder
            .with_single_cert(certs, key)
            .map_err(|error| format!("{}: {error}", options.cert.display()))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            access,
        })
    }

    /// Run the handshake of the server on `stream`. Return the TLS stream,
    /// and true when the client may register read-only only.
    pub(crate) async fn accept(
        &self,
        stream: TcpStream,
    ) -> io::Result<(TlsStream<TcpStream>, bool)> {
        let stream = self.acceptor.accept(stream).await?;
        let read_only = match &self.access {
            Access::AsAsked => false,
            Access::ReadOnly => true,
            Access::Writers(verifier) => {
                let chain = stream.get_ref().1.peer_certificates().unwrap_or_default();
                !chain.split_first().is_some_and(|(end, intermediates)| {
                    verifier
                        .verify_client_cert(end, intermediates, UnixTime::now())
                        .is_ok()
                })
            }
        };
        Ok((stream, read_only))
    }
}

/// Read the certificates in the PEM file `path`. One at least is
/// necessary.
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let failed = |error: rustls::pki_types::pem::Error| format!("{}: {error}", path.display());
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    if certs.is_empty() {
        return Err(format!("{}: no certificate", path.display()));
    }
    Ok(certs)
}

/// Read the CA certificates in the PEM file `path` as trust anchors. The
/// file must hold one certificate at least.
fn roots(path: &Path) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(path)? {
        roots
            .add(cert)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(roots)
}

/// Return the verifier that requires a client certificate that chains to
/// one of `roots`.
fn verifier(
    roots: RootCertStore,
    provider: &Arc<CryptoProvider>,
) -> Result<Arc<dyn ClientCertVerifier>, String> {
    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(provider))
        .build()
        .map_err(|error| format!("a client CA: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-tls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_that_does_not_read_fails_the_load_and_the_message_names_it() {
        let dir = fresh_dir("missing");
        let options = TlsOptions::new(dir.join("server.pem"), dir.join("server.key"));
        let error = Tls::load(&options).err().unwrap();
        assert!(error.contains("server.pem"), "{error}");
    }

    #[test]
    fn a_file_without_a_certificate_fails_the_load() {
        let dir = fresh_dir("empty");
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "no PEM here\n").unwrap();
        let error = certificates(&empty).unwrap_err();
        assert!(error.ends_with("empty.pem: no certificate"), "{error}");
        let error = roots(&empty).unwrap_err();
        assert!(error.contains("empty.pem"), "{error}");
        let options = TlsOptions::new(&empty, &empty);
        let error = Tls::load(&options).err().unwrap();
        assert!(error.contains("empty.pem"), "{error}");
    }

    #[test]
    fn the_options_keep_their_files() {
        let options = TlsOptions::new("c.pem", "k.pem")
            .client_ca("rw.pem")
            .read_only_client_ca("ro.pem");
        assert_eq!(options.cert, PathBuf::from("c.pem"));
        assert_eq!(options.key, PathBuf::from("k.pem"));
        assert_eq!(options.client_ca, Some(PathBuf::from("rw.pem")));
        assert_eq!(options.read_only_client_ca, Some(PathBuf::from("ro.pem")));
    }
}
