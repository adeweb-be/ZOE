//! TLS to the server: the files of [`TlsOptions`] and the handshake of a
//! connection.
//!
//! The client speaks TLS with rustls and its ring provider, as `zoe serve`
//! does. It checks the certificate chain of the server against the CA
//! certificates of `authenticate`: a PEM file, or a directory of PEM files.
//! Without `authenticate`, it uses the CA certificates of the system, as
//! the default context of Python's `ssl` does. With `check_hostname`, it
//! also checks that the certificate is valid for the name of the server:
//! the webpki verifier of rustls does both. Without it, the chain check of
//! rustls runs alone, and no name check runs. It sends its own certificate
//! to the server.
//!
//! The certificate of the server must chain to a CA certificate of
//! `authenticate`. A self-signed server certificate in `authenticate` does
//! not match it by its bytes, as OpenSSL does. In a directory, the client
//! reads each file, while OpenSSL reads only the files whose names are
//! the hashes of `openssl rehash`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_rustls::TlsConnector;

use crate::runtime::Stream;

/// The files and the checks of TLS to a server, as the `<ssl>` section of
/// a ZEO client gives them. The files are PEM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsOptions {
    certificate: PathBuf,
    key: Option<PathBuf>,
    authenticate: Option<PathBuf>,
    check_hostname: bool,
}

impl TlsOptions {
    /// Send the certificate chain in `certificate`, and check the server
    /// against the CA certificates in `authenticate`. `authenticate` is a
    /// PEM file, or a directory: the client then reads each file in it.
    /// The private key is in `certificate` too, unless [`TlsOptions::key`]
    /// names its file. The client checks the name of the server, unless
    /// [`TlsOptions::check_hostname`] turns the check off.
    pub fn new(certificate: impl AsRef<Path>, authenticate: impl AsRef<Path>) -> Self {
        Self {
            authenticate: Some(authenticate.as_ref().to_path_buf()),
            ..Self::system(certificate)
        }
    }

    /// Send the certificate chain in `certificate`, and check the server
    /// against the CA certificates of the system. These are the files of
    /// `SSL_CERT_FILE` and `SSL_CERT_DIR` when one of the two is set, else
    /// the default files of OpenSSL.
    pub fn system(certificate: impl AsRef<Path>) -> Self {
        Self {
            certificate: certificate.as_ref().to_path_buf(),
            key: None,
            authenticate: None,
            check_hostname: true,
        }
    }

    /// Read the private key of the certificate from `key`.
    #[must_use]
    pub fn key(mut self, key: impl AsRef<Path>) -> Self {
        self.key = Some(key.as_ref().to_path_buf());
        self
    }

    /// Check the name of the server in its certificate when `check` is
    /// true. When it is false, the client still checks the chain of the
    /// certificate.
    #[must_use]
    pub fn check_hostname(mut self, check: bool) -> Self {
        self.check_hostname = check;
        self
    }
}

/// The TLS of a client: its certificate, its key and the checks of the
/// server certificate. A clone shares the configuration.
#[derive(Clone)]
pub struct Tls {
    connector: TlsConnector,
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tls").finish_non_exhaustive()
    }
}

impl Tls {
    /// Read the files of `options`. Fail with a message that names the
    /// file that does not read, holds no PEM item or does not fit. The
    /// message of an error of the ring provider names the provider. A
    /// directory or a system store without a CA certificate fails the load
    /// too.
    pub fn load(options: &TlsOptions) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = certificates(&options.certificate)?;
        let key_file = options.key.as_deref().unwrap_or(&options.certificate);
        let key = PrivateKeyDer::from_pem_file(key_file)
            .map_err(|error| format!("{}: {error}", key_file.display()))?;
        let (roots, source) = roots(options.authenticate.as_deref())?;
        let roots = Arc::new(roots);
        let algorithms = provider.signature_verification_algorithms;
        let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("the ring provider: {error}"))?;
        let builder = if options.check_hostname {
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|error| format!("{source}: {error}"))?;
            builder.with_webpki_verifier(webpki)
        } else {
            let verifier = ChainOnly { roots, algorithms };
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
        };
        let config = builder
            .with_client_auth_cert(certs, key)
            .map_err(|error| format!("{}: {error}", key_file.display()))?;
        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
        })
    }

    /// Run the handshake of the client on `stream`, to the server `name`.
    /// A failed check of the server certificate fails the handshake.
    pub(crate) async fn handshake(
        &self,
        stream: Box<dyn Stream>,
        name: ServerName<'static>,
    ) -> io::Result<Box<dyn Stream>> {
        let stream = self.connector.connect(name, stream).await?;
        Ok(Box::new(stream))
    }
}

/// The name of the system store in an error message.
const SYSTEM: &str = "the system CA certificates";

/// Return the CA certificates of `authenticate`, and the name of their
/// source for an error message.
///
/// - A file: each certificate in it must fit.
/// - A directory: the certificates of each file in it. A certificate that
///   does not fit is skipped, as is a file that does not read.
/// - `None`: the CA certificates of the system, with the same rules as a
///   directory.
///
/// A directory or a system store without a certificate that fits fails.
fn roots(authenticate: Option<&Path>) -> Result<(RootCertStore, String), String> {
    let mut roots = RootCertStore::empty();
    let (found, source) = match authenticate {
        Some(path) if !path.is_dir() => {
            for cert in certificates(path)? {
                roots
                    .add(cert)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
            }
            return Ok((roots, path.display().to_string()));
        }
        Some(dir) => (
            rustls_native_certs::load_certs_from_paths(None, Some(dir)),
            dir.display().to_string(),
        ),
        None => (rustls_native_certs::load_native_certs(), SYSTEM.to_owned()),
    };
    roots.add_parsable_certificates(found.certs);
    if roots.is_empty() {
        let reason = match found.errors.first() {
            Some(error) => format!(" ({error})"),
            None => String::new(),
        };
        return Err(format!("{source}: no CA certificate{reason}"));
    }
    Ok((roots, source))
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

/// A verifier that checks the chain of the server certificate against
/// `roots`, and not the name of the server.
///
/// It runs the chain check of rustls itself and never runs the name check.
/// So no name error can hide an error of the chain.
struct ChainOnly {
    roots: Arc<RootCertStore>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for ChainOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainOnly")
            .field("roots", &self.roots.len())
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zoe-client-tls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_options_keep_their_files_and_check_the_name_by_default() {
        let options = TlsOptions::new("c.pem", "ca.pem");
        assert_eq!(options.certificate, PathBuf::from("c.pem"));
        assert_eq!(options.key, None);
        assert_eq!(options.authenticate, Some(PathBuf::from("ca.pem")));
        assert!(options.check_hostname);
        let options = options.key("k.pem").check_hostname(false);
        assert_eq!(options.key, Some(PathBuf::from("k.pem")));
        assert!(!options.check_hostname);
        let options = TlsOptions::system("c.pem");
        assert_eq!(options.authenticate, None);
        assert!(options.check_hostname);
    }

    #[test]
    fn a_directory_without_a_certificate_fails_the_load_and_the_message_names_it() {
        let dir = fresh_dir("no-ca");
        std::fs::write(dir.join("readme.txt"), "no PEM here\n").unwrap();
        let error = roots(Some(&dir)).err().unwrap();
        assert_eq!(error, format!("{}: no CA certificate", dir.display()));
    }

    #[test]
    fn a_file_that_does_not_read_fails_the_load_and_the_message_names_it() {
        let dir = fresh_dir("missing");
        let options = TlsOptions::new(dir.join("client.pem"), dir.join("ca.pem"));
        let error = Tls::load(&options).unwrap_err();
        assert!(error.contains("client.pem"), "{error}");
    }

    #[test]
    fn a_file_without_a_certificate_fails_the_load() {
        let dir = fresh_dir("empty");
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "no PEM here\n").unwrap();
        let error = certificates(&empty).unwrap_err();
        assert!(error.ends_with("empty.pem: no certificate"), "{error}");
        let error = Tls::load(&TlsOptions::new(&empty, &empty)).unwrap_err();
        assert!(error.contains("empty.pem"), "{error}");
    }

    fn chain_only() -> ChainOnly {
        ChainOnly {
            roots: Arc::new(RootCertStore::empty()),
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }

    #[test]
    fn without_the_name_check_a_certificate_that_does_not_parse_fails() {
        let name = ServerName::try_from("example.org").unwrap();
        let cert = CertificateDer::from(vec![1, 2, 3]);
        let verified = chain_only().verify_server_cert(&cert, &[], &name, &[], UnixTime::now());
        assert!(verified.is_err());
    }

    #[test]
    fn the_verifier_without_the_name_check_keeps_the_schemes_of_the_provider() {
        let provider = rustls::crypto::ring::default_provider();
        assert_eq!(
            chain_only().supported_verify_schemes(),
            provider
                .signature_verification_algorithms
                .supported_schemes()
        );
    }
}
