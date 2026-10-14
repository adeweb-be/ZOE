//! TLS to the server: the files of [`TlsOptions`] and the handshake of a
//! connection.
//!
//! The client speaks TLS with rustls and its ring provider, as `zoe serve`
//! does. It checks the certificate chain of the server against the CA
//! certificates of `authenticate`. With `check_hostname`, it also checks
//! that the certificate is valid for the name of the server. It sends its
//! own certificate to the server.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme,
};
use tokio_rustls::TlsConnector;

use crate::runtime::Stream;

/// The files and the checks of TLS to a server, as the `<ssl>` section of
/// a ZEO client gives them. The files are PEM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsOptions {
    certificate: PathBuf,
    key: Option<PathBuf>,
    authenticate: PathBuf,
    check_hostname: bool,
}

impl TlsOptions {
    /// Send the certificate chain in `certificate`, and check the server
    /// against the CA certificates in `authenticate`. The private key is in
    /// `certificate` too, unless [`TlsOptions::key`] names its file. The
    /// client checks the name of the server, unless
    /// [`TlsOptions::check_hostname`] turns the check off.
    pub fn new(certificate: impl AsRef<Path>, authenticate: impl AsRef<Path>) -> Self {
        Self {
            certificate: certificate.as_ref().to_path_buf(),
            key: None,
            authenticate: authenticate.as_ref().to_path_buf(),
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
    /// message of an error of the ring provider names the provider.
    pub fn load(options: &TlsOptions) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = certificates(&options.certificate)?;
        let key_file = options.key.as_deref().unwrap_or(&options.certificate);
        let key = PrivateKeyDer::from_pem_file(key_file)
            .map_err(|error| format!("{}: {error}", key_file.display()))?;
        let authenticate = &options.authenticate;
        let mut roots = RootCertStore::empty();
        for cert in certificates(authenticate)? {
            roots
                .add(cert)
                .map_err(|error| format!("{}: {error}", authenticate.display()))?;
        }
        let webpki =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .build()
                .map_err(|error| format!("{}: {error}", authenticate.display()))?;
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("the ring provider: {error}"))?;
        let builder = if options.check_hostname {
            builder.with_webpki_verifier(webpki)
        } else {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyName(webpki)))
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

/// A verifier that checks the chain of the server certificate, and not the
/// name of the server.
///
/// webpki checks the name after the chain and the validity period. So a
/// name error tells that every other check passed, and this verifier
/// accepts the certificate then.
#[derive(Debug)]
struct AnyName<V: ?Sized>(Arc<V>);

impl<V: ServerCertVerifier + ?Sized> ServerCertVerifier for AnyName<V> {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verified =
            self.0
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);
        match verified {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            verified => verified,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.0.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.0.root_hint_subjects()
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
        assert_eq!(options.authenticate, PathBuf::from("ca.pem"));
        assert!(options.check_hostname);
        let options = options.key("k.pem").check_hostname(false);
        assert_eq!(options.key, Some(PathBuf::from("k.pem")));
        assert!(!options.check_hostname);
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

    /// A verifier that fails each certificate with `error`, or accepts it.
    #[derive(Debug)]
    struct Fixed(Option<CertificateError>);

    impl ServerCertVerifier for Fixed {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            match &self.0 {
                Some(error) => Err(error.clone().into()),
                None => Ok(ServerCertVerified::assertion()),
            }
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

    fn verify(error: Option<CertificateError>) -> Result<(), rustls::Error> {
        let verifier = AnyName(Arc::new(Fixed(error)));
        let name = ServerName::try_from("example.org").unwrap();
        let cert = CertificateDer::from(vec![1, 2, 3]);
        verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .map(|_| ())
    }

    #[test]
    fn without_the_name_check_only_a_name_error_passes() {
        assert!(verify(None).is_ok());
        assert!(verify(Some(CertificateError::NotValidForName)).is_ok());
        let context = CertificateError::NotValidForNameContext {
            expected: ServerName::try_from("example.org").unwrap(),
            presented: vec!["localhost".to_owned()],
        };
        assert!(verify(Some(context)).is_ok());
        for error in [
            CertificateError::UnknownIssuer,
            CertificateError::Expired,
            CertificateError::NotValidYet,
            CertificateError::BadSignature,
            CertificateError::Revoked,
        ] {
            assert_eq!(
                verify(Some(error.clone())),
                Err(rustls::Error::InvalidCertificate(error))
            );
        }
    }

    #[test]
    fn the_verifier_without_the_name_check_keeps_the_schemes() {
        let verifier = AnyName(Arc::new(Fixed(None)));
        assert_eq!(
            verifier.supported_verify_schemes(),
            vec![SignatureScheme::ED25519]
        );
    }
}
