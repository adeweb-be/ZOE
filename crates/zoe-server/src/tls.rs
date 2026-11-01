//! TLS on the ZEO port: the files of [`TlsOptions`] and the handshake of
//! each connection.
//!
//! The server speaks TLS with rustls and its ring provider. A client CA
//! makes a client certificate necessary: the handshake refuses a client
//! without a certificate that chains to it. A client whose certificate
//! chains only to the read-only client CA registers read-only only.
//!
//! A client certificate that is byte for byte one of the certificates of
//! the client CA file is also valid, within its validity period, also when
//! it is a CA certificate. OpenSSL accepts such a certificate, and ZEO's
//! tests use one. A certificate of the read-only client CA file never
//! matches by its bytes. `zoe_wire::tls` reads the PEM files and the
//! validity period of the pinned certificates.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use zoe_wire::tls::{Pinned, Validity, certificates, der};

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
    /// `ca`, or that is byte for byte a certificate in `ca`. Its holder
    /// registers as it asks.
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
    /// file that does not read, holds no PEM item or does not fit. The
    /// message of an error of the ring provider names the provider.
    pub(crate) fn load(options: &TlsOptions) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = certificates(&options.cert)?;
        let key = PrivateKeyDer::from_pem_file(&options.key)
            .map_err(|error| format!("{}: {error}", options.key.display()))?;
        let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("the ring provider: {error}"))?;
        let writers = options.client_ca.as_deref().map(ca).transpose()?;
        let readers = options.read_only_client_ca.as_deref().map(ca).transpose()?;
        // Only a certificate of the client CA file matches by its bytes.
        let pinned = writers.as_ref().map_or_else(
            || Arc::new(Pinned::default()),
            |writers| pinned(&writers.certs),
        );
        let (builder, access) = match (writers, readers) {
            (None, None) => (builder.with_no_client_auth(), Access::AsAsked),
            (Some(writers), None) => (
                builder.with_client_cert_verifier(verifier(writers, &provider, &pinned)?),
                Access::AsAsked,
            ),
            (writers, Some(mut all)) => {
                // The handshake accepts a certificate of either CA.
                let access = match writers {
                    None => Access::ReadOnly,
                    Some(writers) => {
                        all.roots.roots.extend(writers.roots.roots.iter().cloned());
                        Access::Writers(verifier(writers, &provider, &pinned)?)
                    }
                };
                (
                    builder.with_client_cert_verifier(verifier(all, &provider, &pinned)?),
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

/// A client CA file: its certificates, its trust anchors and its path.
struct Ca<'a> {
    certs: Vec<CertificateDer<'static>>,
    roots: RootCertStore,
    path: &'a Path,
}

/// Read the client CA file `path`. Its certificates are the trust anchors.
/// The file must hold one certificate at least.
fn ca(path: &Path) -> Result<Ca<'_>, String> {
    let certs = certificates(path)?;
    let mut roots = RootCertStore::empty();
    for cert in &certs {
        roots
            .add(cert.clone())
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(Ca { certs, roots, path })
}

/// Return the verifier that requires a client certificate that chains to
/// a trust anchor of `ca`, or that is one of `pinned`. An error names the
/// CA file.
fn verifier(
    ca: Ca<'_>,
    provider: &Arc<CryptoProvider>,
    pinned: &Arc<Pinned<Validity>>,
) -> Result<Arc<dyn ClientCertVerifier>, String> {
    let webpki =
        WebPkiClientVerifier::builder_with_provider(Arc::new(ca.roots), Arc::clone(provider))
            .build()
            .map_err(|error| format!("{}: {error}", ca.path.display()))?;
    Ok(Arc::new(ClientCa {
        webpki,
        pinned: Arc::clone(pinned),
    }))
}

/// Return the certificates of the client CA file `certs` that a client
/// certificate can match by its bytes, with their validity periods. A
/// certificate whose validity period does not read never matches.
fn pinned(certs: &[CertificateDer<'static>]) -> Arc<Pinned<Validity>> {
    Arc::new(Pinned::read(certs, der::validity))
}

/// The verifier of the client certificates of a server.
///
/// A client certificate that is byte for byte one of `pinned` is valid
/// when the time is within its validity period. The other checks of
/// `webpki` do not apply to it, so it can be a CA certificate. `webpki`
/// checks each other client certificate. `webpki` also checks the
/// signature of the handshake for each certificate, so the client must
/// hold the key of its certificate.
#[derive(Debug)]
struct ClientCa {
    webpki: Arc<dyn ClientCertVerifier>,
    pinned: Arc<Pinned<Validity>>,
}

impl ClientCertVerifier for ClientCa {
    fn offer_client_auth(&self) -> bool {
        self.webpki.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.webpki.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.webpki.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let Some(validity) = self.pinned.find(end_entity) else {
            return self
                .webpki
                .verify_client_cert(end_entity, intermediates, now);
        };
        validity.check(now)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.webpki.requires_raw_public_keys()
    }
}

#[cfg(test)]
mod tests {
    use rustls::CertificateError;

    use super::*;
    use crate::protocol::testing::{self, Fresh};

    /// Return a new empty directory for the test `name`.
    #[track_caller]
    fn fresh_dir(name: &str) -> Fresh {
        let dir = testing::fresh_dir(name);
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
        let error = ca(&empty).err().unwrap();
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

    /// A verifier that refuses each client certificate as of an unknown
    /// issuer.
    #[derive(Debug)]
    struct Refuse;

    impl ClientCertVerifier for Refuse {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _now: UnixTime,
        ) -> Result<ClientCertVerified, rustls::Error> {
            Err(CertificateError::UnknownIssuer.into())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(CertificateError::BadSignature.into())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(CertificateError::BadSignature.into())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ED25519]
        }
    }

    fn at(secs: u64) -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs))
    }

    /// A verifier that pins the bytes 1, 2, 3 from the time 100 to the
    /// time 200, and refuses each other certificate.
    fn pinned_123() -> ClientCa {
        let validity = Validity {
            not_before: at(100),
            not_after: at(200),
        };
        ClientCa {
            webpki: Arc::new(Refuse),
            pinned: Arc::new(Pinned::read(&[CertificateDer::from(vec![1, 2, 3])], |_| {
                Some(validity)
            })),
        }
    }

    #[test]
    fn a_pinned_certificate_is_valid_only_within_its_validity_period() {
        let verifier = pinned_123();
        let pinned = CertificateDer::from(vec![1, 2, 3]);
        for now in [100, 150, 200] {
            assert!(verifier.verify_client_cert(&pinned, &[], at(now)).is_ok());
        }
        assert_eq!(
            verifier.verify_client_cert(&pinned, &[], at(99)).err(),
            Some(
                CertificateError::NotValidYetContext {
                    time: at(99),
                    not_before: at(100),
                }
                .into()
            )
        );
        assert_eq!(
            verifier.verify_client_cert(&pinned, &[], at(201)).err(),
            Some(
                CertificateError::ExpiredContext {
                    time: at(201),
                    not_after: at(200),
                }
                .into()
            )
        );
    }

    #[test]
    fn a_certificate_that_is_not_byte_for_byte_pinned_goes_to_webpki() {
        let verifier = pinned_123();
        for other in [vec![], vec![1, 2], vec![1, 2, 4], vec![1, 2, 3, 4]] {
            assert_eq!(
                verifier
                    .verify_client_cert(&CertificateDer::from(other), &[], at(150))
                    .err(),
                Some(CertificateError::UnknownIssuer.into())
            );
        }
        // Without a client CA file, no certificate is pinned.
        let verifier = ClientCa {
            webpki: Arc::new(Refuse),
            pinned: Arc::new(Pinned::default()),
        };
        let cert = CertificateDer::from(vec![1, 2, 3]);
        assert!(verifier.verify_client_cert(&cert, &[], at(150)).is_err());
    }
}
