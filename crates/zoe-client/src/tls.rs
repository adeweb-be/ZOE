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
//! to the server when it has one. Without a certificate, it sends none,
//! as ZEO's client does without `certificate`.
//!
//! The certificate of the server must chain to a CA certificate of
//! `authenticate`, or be byte for byte one of the certificates of
//! `authenticate` (a pinned certificate), within its validity period, also
//! when it is a CA certificate. OpenSSL accepts such a certificate as its
//! own trust anchor, and ZEO's tests give the self-signed certificate of
//! the server as `authenticate`. `zoe serve` matches the client
//! certificates of its client CA file the same way. The certificates of
//! the system store are never pinned. The handshake still proves that the
//! server holds the key of a pinned certificate. With `check_hostname`, a
//! pinned certificate must be valid for the name of the server: by its
//! subjectAltName, as webpki checks it, or, when it has no subjectAltName,
//! by a common name of its subject that is the DNS name, as OpenSSL checks
//! it for Python's `ssl`. In a directory, the client reads each file,
//! while OpenSSL reads only the files whose names are the hashes of
//! `openssl rehash`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{
    WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor, verify_server_name,
};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_rustls::TlsConnector;

use crate::runtime::Stream;

mod der;

/// The files and the checks of TLS to a server, as the `<ssl>` section of
/// a ZEO client gives them. The files are PEM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsOptions {
    certificate: Option<PathBuf>,
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
            certificate: Some(certificate.as_ref().to_path_buf()),
            ..Self::without_certificate(None)
        }
    }

    /// Send no certificate, and check the server against the CA
    /// certificates in `authenticate`, or against those of the system when
    /// `authenticate` is `None`. The client ignores [`TlsOptions::key`].
    pub fn without_certificate(authenticate: Option<&Path>) -> Self {
        Self {
            certificate: None,
            key: None,
            authenticate: authenticate.map(Path::to_path_buf),
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

/// The TLS of a client: its certificate and its key, if any, and the
/// checks of the server certificate. A clone shares the configuration.
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
        let client = match &options.certificate {
            Some(certificate) => {
                let certs = certificates(certificate)?;
                let key_file = options.key.as_deref().unwrap_or(certificate);
                let key = PrivateKeyDer::from_pem_file(key_file)
                    .map_err(|error| format!("{}: {error}", key_file.display()))?;
                Some((certs, key, key_file))
            }
            None => None,
        };
        let (roots, source, pinned) = roots(options.authenticate.as_deref())?;
        let roots = Arc::new(roots);
        let algorithms = provider.signature_verification_algorithms;
        let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("the ring provider: {error}"))?;
        let chain: Arc<dyn ServerCertVerifier> = if options.check_hostname {
            WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|error| format!("{source}: {error}"))?
        } else {
            Arc::new(ChainOnly { roots, algorithms })
        };
        let verifier = ServerCa {
            chain,
            pinned: pinned_of(&pinned),
            check_hostname: options.check_hostname,
        };
        let builder = builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier));
        let config = match client {
            Some((certs, key, key_file)) => builder
                .with_client_auth_cert(certs, key)
                .map_err(|error| format!("{}: {error}", key_file.display()))?,
            None => builder.with_no_client_auth(),
        };
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

/// The CA certificates of `authenticate`, the name of their source for an
/// error message, and the certificates that a server certificate can match
/// by its bytes.
type Roots = (RootCertStore, String, Vec<CertificateDer<'static>>);

/// Return the CA certificates of `authenticate`, the name of their source
/// for an error message, and the certificates to pin.
///
/// - A file: each certificate in it must fit. Each is pinned.
/// - A directory: the certificates of each file in it. A certificate that
///   does not fit is skipped, as is a file that does not read. Each
///   certificate that reads is pinned.
/// - `None`: the CA certificates of the system, with the same rules as a
///   directory. None is pinned.
///
/// A directory or a system store without a certificate that fits fails.
fn roots(authenticate: Option<&Path>) -> Result<Roots, String> {
    let mut roots = RootCertStore::empty();
    let (found, source, pin) = match authenticate {
        Some(path) if !path.is_dir() => {
            let certs = certificates(path)?;
            for cert in &certs {
                roots
                    .add(cert.clone())
                    .map_err(|error| format!("{}: {error}", path.display()))?;
            }
            return Ok((roots, path.display().to_string(), certs));
        }
        Some(dir) => (
            rustls_native_certs::load_certs_from_paths(None, Some(dir)),
            dir.display().to_string(),
            true,
        ),
        None => (
            rustls_native_certs::load_native_certs(),
            SYSTEM.to_owned(),
            false,
        ),
    };
    let pinned = if pin { found.certs.clone() } else { Vec::new() };
    roots.add_parsable_certificates(found.certs);
    if roots.is_empty() {
        let reason = match found.errors.first() {
            Some(error) => format!(" ({error})"),
            None => String::new(),
        };
        return Err(format!("{source}: no CA certificate{reason}"));
    }
    Ok((roots, source, pinned))
}

/// A certificate of `authenticate` that a server certificate can match by
/// its bytes, with its validity period and its names.
#[derive(Debug)]
struct Pinned {
    der: CertificateDer<'static>,
    parts: der::Parts,
}

/// Return the certificates of `certs` that a server certificate can match
/// by its bytes. A certificate whose parts do not read never matches.
fn pinned_of(certs: &[CertificateDer<'static>]) -> Arc<[Pinned]> {
    certs
        .iter()
        .filter_map(|der| {
            Some(Pinned {
                parts: der::parts(der)?,
                der: der.clone(),
            })
        })
        .collect()
}

/// The verifier of the server certificate.
///
/// A server certificate that is byte for byte one of `pinned` is valid
/// within its validity period, and, with `check_hostname`, for the name
/// of the server (see the module). The checks of the chain do not apply to
/// it, so it can be a CA certificate. `chain` checks each other server
/// certificate: the webpki verifier of rustls with the name check, else
/// `ChainOnly`. `chain` also checks the signature of the handshake for each
/// certificate, so the server must hold the key of its certificate.
#[derive(Debug)]
struct ServerCa {
    chain: Arc<dyn ServerCertVerifier>,
    pinned: Arc<[Pinned]>,
    check_hostname: bool,
}

impl ServerCertVerifier for ServerCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let pinned = self
            .pinned
            .iter()
            .find(|pinned| pinned.der.as_ref() == end_entity.as_ref());
        let Some(pinned) = pinned else {
            return self.chain.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            );
        };
        let parts = &pinned.parts;
        if now < parts.not_before {
            return Err(CertificateError::NotValidYetContext {
                time: now,
                not_before: parts.not_before,
            }
            .into());
        }
        if now > parts.not_after {
            return Err(CertificateError::ExpiredContext {
                time: now,
                not_after: parts.not_after,
            }
            .into());
        }
        if self.check_hostname {
            check_name(end_entity, parts, server_name)?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.chain.supported_verify_schemes()
    }
}

/// Check that the pinned certificate `cert` is valid for `name`: by its
/// subjectAltName, as webpki checks it. A certificate without a
/// subjectAltName is valid for a DNS name that is one of its common names,
/// without regard to case, as OpenSSL checks it.
fn check_name(
    cert: &CertificateDer<'_>,
    parts: &der::Parts,
    name: &ServerName<'_>,
) -> Result<(), rustls::Error> {
    let by_alt_names = verify_server_name(&ParsedCertificate::try_from(cert)?, name);
    if by_alt_names.is_ok() || parts.has_alt_names {
        return by_alt_names;
    }
    let ServerName::DnsName(dns) = name else {
        return by_alt_names;
    };
    let dns = dns.as_ref();
    if parts
        .common_names
        .iter()
        .any(|common| common.eq_ignore_ascii_case(dns))
    {
        return Ok(());
    }
    by_alt_names
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
        assert_eq!(options.certificate, Some(PathBuf::from("c.pem")));
        assert_eq!(options.key, None);
        assert_eq!(options.authenticate, Some(PathBuf::from("ca.pem")));
        assert!(options.check_hostname);
        let options = options.key("k.pem").check_hostname(false);
        assert_eq!(options.key, Some(PathBuf::from("k.pem")));
        assert!(!options.check_hostname);
        let options = TlsOptions::system("c.pem");
        assert_eq!(options.authenticate, None);
        assert!(options.check_hostname);
        let options = TlsOptions::without_certificate(Some(Path::new("ca.pem")));
        assert_eq!(options.certificate, None);
        assert_eq!(options.key, None);
        assert_eq!(options.authenticate, Some(PathBuf::from("ca.pem")));
        assert!(options.check_hostname);
        let options = TlsOptions::without_certificate(None);
        assert_eq!(options.authenticate, None);
    }

    #[test]
    fn without_a_certificate_the_load_reads_no_key() {
        let dir = fresh_dir("anonymous");
        let ca = dir.join("ca.pem");
        let output = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "ec", "-pkeyopt"])
            .args(["ec_paramgen_curve:prime256v1", "-nodes", "-days", "2"])
            .args(["-subj", "/CN=ca", "-keyout", "ca.key", "-out", "ca.pem"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        let options = TlsOptions::without_certificate(Some(&ca)).key(dir.join("missing.key"));
        Tls::load(&options).unwrap();
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

    /// Make a self-signed certificate `NAME.pem` in `dir` with the
    /// subject `subject` and the openssl arguments `extra`, and return it.
    fn self_signed(
        dir: &Path,
        name: &str,
        subject: &str,
        extra: &[&str],
    ) -> CertificateDer<'static> {
        let output = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "ec", "-pkeyopt"])
            .args(["ec_paramgen_curve:prime256v1", "-nodes", "-days", "2"])
            .args(["-subj", subject, "-keyout", &format!("{name}.key")])
            .args(["-out", &format!("{name}.pem")])
            .args(extra)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        certificates(&dir.join(format!("{name}.pem")))
            .unwrap()
            .remove(0)
    }

    #[test]
    fn a_pinned_certificate_is_valid_only_within_its_validity_period_and_for_its_names() {
        let dir = fresh_dir("pinned");
        let cert = self_signed(&dir, "cn", "/CN=Db.Example", &[]);
        let other = self_signed(&dir, "other", "/CN=db.example", &[]);
        let verifier = |check_hostname| ServerCa {
            chain: Arc::new(chain_only()),
            pinned: pinned_of(std::slice::from_ref(&cert)),
            check_hostname,
        };
        let name = ServerName::try_from("db.example").unwrap();
        fn verify(
            verifier: &ServerCa,
            cert: &CertificateDer<'_>,
            name: &ServerName<'_>,
            now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            verifier.verify_server_cert(cert, &[], name, &[], now)
        }
        let now = UnixTime::now();
        verify(&verifier(true), &cert, &name, now).unwrap();
        let before = UnixTime::since_unix_epoch(std::time::Duration::from_secs(86_400));
        let after = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1 << 40));
        for time in [before, after] {
            let refused = verify(&verifier(false), &cert, &name, time);
            assert!(refused.is_err(), "{refused:?}");
        }
        // The name check reads the common name, but only for a DNS name.
        for wrong in ["example.org", "sub.db.example", "127.0.0.1"] {
            let wrong = ServerName::try_from(wrong).unwrap();
            assert!(verify(&verifier(true), &cert, &wrong, now).is_err());
            verify(&verifier(false), &cert, &wrong, now).unwrap();
        }
        // A certificate that is not byte for byte pinned goes to the chain
        // check, with the same subject.
        assert!(verify(&verifier(false), &other, &name, now).is_err());
    }

    #[test]
    fn a_pinned_certificate_with_a_san_never_matches_by_its_common_name() {
        let dir = fresh_dir("pinned-san");
        let extra = ["-addext", "subjectAltName=DNS:alt.example"];
        let cert = self_signed(&dir, "san", "/CN=db.example", &extra);
        let verifier = ServerCa {
            chain: Arc::new(chain_only()),
            pinned: pinned_of(std::slice::from_ref(&cert)),
            check_hostname: true,
        };
        let now = UnixTime::now();
        let alt = ServerName::try_from("alt.example").unwrap();
        verifier
            .verify_server_cert(&cert, &[], &alt, &[], now)
            .unwrap();
        let common = ServerName::try_from("db.example").unwrap();
        let refused = verifier.verify_server_cert(&cert, &[], &common, &[], now);
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn a_file_and_a_directory_of_authenticate_pin_their_certificates() {
        let dir = fresh_dir("pinned-dir");
        self_signed(&dir, "a", "/CN=a", &[]);
        let (_, _, pinned) = roots(Some(&dir.join("a.pem"))).unwrap();
        assert_eq!(pinned.len(), 1);
        let (_, _, pinned) = roots(Some(&dir)).unwrap();
        assert_eq!(pinned.len(), 1);
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
