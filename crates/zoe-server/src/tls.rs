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
//! matches by its bytes.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme,
};
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
        let pinned = writers
            .as_ref()
            .map_or_else(|| Arc::from([]), |writers| pinned(&writers.certs));
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
    pinned: &Arc<[Pinned]>,
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

/// A certificate of the client CA file, with its validity period.
#[derive(Debug)]
struct Pinned {
    der: CertificateDer<'static>,
    not_before: UnixTime,
    not_after: UnixTime,
}

/// Return the certificates of the client CA file `certs` that a client
/// certificate can match by its bytes. A certificate whose validity period
/// does not read never matches.
fn pinned(certs: &[CertificateDer<'static>]) -> Arc<[Pinned]> {
    certs
        .iter()
        .filter_map(|der| {
            let (not_before, not_after) = validity(der)?;
            Some(Pinned {
                der: der.clone(),
                not_before,
                not_after,
            })
        })
        .collect()
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
    pinned: Arc<[Pinned]>,
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
        let pinned = self
            .pinned
            .iter()
            .find(|pinned| pinned.der.as_ref() == end_entity.as_ref());
        let Some(pinned) = pinned else {
            return self
                .webpki
                .verify_client_cert(end_entity, intermediates, now);
        };
        if now < pinned.not_before {
            return Err(CertificateError::NotValidYetContext {
                time: now,
                not_before: pinned.not_before,
            }
            .into());
        }
        if now > pinned.not_after {
            return Err(CertificateError::ExpiredContext {
                time: now,
                not_after: pinned.not_after,
            }
            .into());
        }
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

/// The DER tags that [`validity`] reads.
const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const VERSION: u8 = 0xa0;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// Read the validity period of the DER certificate `der` (RFC 5280,
/// 4.1.2.5): its notBefore and its notAfter. Return `None` when the
/// certificate does not read, when a time is before 1970, or when
/// notBefore is after notAfter.
fn validity(der: &[u8]) -> Option<(UnixTime, UnixTime)> {
    let (certificate, rest) = element(der, SEQUENCE)?;
    if !rest.is_empty() {
        return None;
    }
    let (tbs, _) = element(certificate, SEQUENCE)?;
    // The version is optional.
    let tbs = element(tbs, VERSION).map_or(tbs, |(_, rest)| rest);
    let (_serial, tbs) = element(tbs, INTEGER)?;
    let (_signature, tbs) = element(tbs, SEQUENCE)?;
    let (_issuer, tbs) = element(tbs, SEQUENCE)?;
    let (validity, _) = element(tbs, SEQUENCE)?;
    let (not_before, rest) = time(validity)?;
    let (not_after, rest) = time(rest)?;
    if !rest.is_empty() || not_before > not_after {
        return None;
    }
    Some((
        UnixTime::since_unix_epoch(Duration::from_secs(not_before)),
        UnixTime::since_unix_epoch(Duration::from_secs(not_after)),
    ))
}

/// Read the DER element with the tag `tag` at the start of `input`.
/// Return its contents and the bytes after it.
fn element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&first, input) = input.split_first()?;
    if first != tag {
        return None;
    }
    let (&length, input) = input.split_first()?;
    let (length, input) = if length < 0x80 {
        (usize::from(length), input)
    } else {
        // The long form: the low bits give the count of the length bytes.
        let count = usize::from(length & 0x7f);
        if count == 0 || count > 4 {
            return None;
        }
        let (bytes, input) = input.split_at_checked(count)?;
        let length = bytes
            .iter()
            .fold(0, |length, &byte| length << 8 | usize::from(byte));
        (length, input)
    };
    input.split_at_checked(length)
}

/// Read the UTCTime or the GeneralizedTime at the start of `input`, in the
/// form of RFC 5280: seconds, no fraction, and `Z`. Return it in seconds
/// since the Unix epoch, and the bytes after it. A time before 1970 does
/// not read.
fn time(input: &[u8]) -> Option<(u64, &[u8])> {
    let (year, digits, rest) = if let Some((value, rest)) = element(input, UTC_TIME) {
        let (year, digits) = value.split_at_checked(2)?;
        let year = number(year)?;
        // A two-digit year from 50 is in the 1900s, else in the 2000s.
        let year = if year >= 50 { 1900 + year } else { 2000 + year };
        (year, digits, rest)
    } else {
        let (value, rest) = element(input, GENERALIZED_TIME)?;
        let (year, digits) = value.split_at_checked(4)?;
        (number(year)?, digits, rest)
    };
    let digits = digits.strip_suffix(b"Z")?;
    if digits.len() != 10 {
        return None;
    }
    let field = |at: usize| number(&digits[at..at + 2]);
    let (month, day) = (field(0)?, field(2)?);
    let (hour, minute, second) = (field(4)?, field(6)?, field(8)?);
    if year < 1970
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_since_epoch(year, month, day);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second, rest))
}

/// Read the decimal number in the ASCII digits `digits`.
fn number(digits: &[u8]) -> Option<u64> {
    digits.iter().try_fold(0, |number, &digit| {
        digit
            .is_ascii_digit()
            .then(|| number * 10 + u64::from(digit - b'0'))
    })
}

/// Return the count of the days of the month `month` of the year `year`.
fn days_in_month(year: u64, month: u64) -> u64 {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Return the count of the days from 1970-01-01 to the date
/// `year`-`month`-`day`. The year is 1970 or later.
fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    // The days from 0001-01-01 to the first day of a year.
    let days_before = |year: u64| {
        let past = year - 1;
        past * 365 + past / 4 - past / 100 + past / 400
    };
    let days_before_month: u64 = (1..month).map(|month| days_in_month(year, month)).sum();
    days_before(year) - days_before(1970) + days_before_month + day - 1
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
        UnixTime::since_unix_epoch(Duration::from_secs(secs))
    }

    /// A verifier that pins the bytes 1, 2, 3 from the time 100 to the
    /// time 200, and refuses each other certificate.
    fn pinned_123() -> ClientCa {
        ClientCa {
            webpki: Arc::new(Refuse),
            pinned: Arc::from([Pinned {
                der: CertificateDer::from(vec![1, 2, 3]),
                not_before: at(100),
                not_after: at(200),
            }]),
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
            pinned: Arc::from([]),
        };
        let cert = CertificateDer::from(vec![1, 2, 3]);
        assert!(verifier.verify_client_cert(&cert, &[], at(150)).is_err());
    }

    /// Encode the DER element with the tag `tag` and the contents
    /// `contents`.
    fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let length = contents.len();
        if length < 0x80 {
            out.push(u8::try_from(length).unwrap());
        } else {
            let bytes = length.to_be_bytes();
            let first = bytes.iter().position(|&byte| byte != 0).unwrap();
            out.push(0x80 | u8::try_from(bytes.len() - first).unwrap());
            out.extend_from_slice(&bytes[first..]);
        }
        out.extend_from_slice(contents);
        out
    }

    fn utc(text: &str) -> Vec<u8> {
        der(UTC_TIME, text.as_bytes())
    }

    fn generalized(text: &str) -> Vec<u8> {
        der(GENERALIZED_TIME, text.as_bytes())
    }

    /// Encode a certificate with the times `not_before` and `not_after`,
    /// a version when `version` is true, and an issuer of `issuer` bytes.
    fn certificate(not_before: &[u8], not_after: &[u8], version: bool, issuer: usize) -> Vec<u8> {
        let mut tbs = Vec::new();
        if version {
            tbs.extend(der(VERSION, &der(INTEGER, &[2])));
        }
        tbs.extend(der(INTEGER, &[1]));
        tbs.extend(der(SEQUENCE, &[]));
        tbs.extend(der(SEQUENCE, &vec![0; issuer]));
        tbs.extend(der(SEQUENCE, &[not_before, not_after].concat()));
        tbs.extend(der(SEQUENCE, &[]));
        let mut cert = der(SEQUENCE, &tbs);
        cert.extend(der(SEQUENCE, &[]));
        cert.extend(der(0x03, &[0]));
        der(SEQUENCE, &cert)
    }

    fn seconds(validity: Option<(UnixTime, UnixTime)>) -> Option<(u64, u64)> {
        validity.map(|(not_before, not_after)| (not_before.as_secs(), not_after.as_secs()))
    }

    fn time_of(input: &[u8]) -> Option<u64> {
        time(input).map(|(time, _)| time)
    }

    #[test]
    fn the_validity_reads_from_a_utc_time_and_a_generalized_time() {
        // The validity period of the client certificate of ZEO's tests.
        let cert = certificate(
            &utc("190426085809Z"),
            &generalized("30041218085809Z"),
            true,
            8,
        );
        assert_eq!(
            seconds(validity(&cert)),
            Some((1_556_269_089, 32_660_269_089))
        );
    }

    #[test]
    fn the_validity_reads_without_a_version_and_with_long_lengths() {
        let cert = certificate(&utc("700101000000Z"), &utc("491231235959Z"), false, 300);
        assert_eq!(seconds(validity(&cert)), Some((0, 2_524_607_999)));
    }

    #[test]
    fn a_certificate_that_does_not_read_has_no_validity() {
        let good = certificate(&utc("190426085809Z"), &utc("200426085809Z"), true, 8);
        assert!(validity(&good).is_some());
        for end in 0..good.len() {
            assert_eq!(validity(&good[..end]), None, "{end}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(validity(&trailing), None);
        // The indefinite length is not DER.
        let mut indefinite = good.clone();
        indefinite[1] = 0x80;
        assert_eq!(validity(&indefinite), None);
        let reversed = certificate(&utc("200426085809Z"), &utc("190426085809Z"), true, 8);
        assert_eq!(validity(&reversed), None);
        let before_1970 = certificate(&utc("690101000000Z"), &utc("200426085809Z"), true, 8);
        assert_eq!(validity(&before_1970), None);
        assert_eq!(validity(b"no"), None);
    }

    #[test]
    fn a_two_digit_year_from_50_is_in_the_1900s() {
        assert_eq!(time_of(&utc("491231235959Z")), Some(2_524_607_999));
        assert_eq!(
            time_of(&generalized("20500101000000Z")),
            Some(2_524_608_000)
        );
        assert_eq!(time_of(&utc("700101000000Z")), Some(0));
        // 1950 is before 1970.
        assert_eq!(time_of(&utc("500101000000Z")), None);
    }

    #[test]
    fn the_days_count_the_leap_years() {
        assert_eq!(days_since_epoch(1970, 1, 1), 0);
        assert_eq!(days_since_epoch(2000, 3, 1) * 86_400, 951_868_800);
        assert_eq!(days_since_epoch(2024, 2, 29) * 86_400, 1_709_164_800);
        assert_eq!(days_since_epoch(2100, 3, 1) * 86_400, 4_107_542_400);
        assert_eq!(time_of(&generalized("20000229000000Z")), Some(951_782_400));
        assert_eq!(
            time_of(&generalized("20240229000000Z")),
            Some(1_709_164_800)
        );
        assert_eq!(time_of(&generalized("20230229000000Z")), None);
        assert_eq!(time_of(&generalized("21000229000000Z")), None);
    }

    #[test]
    fn a_time_out_of_the_form_of_rfc_5280_does_not_read() {
        for text in [
            "190426085809",
            "1904260858Z",
            "19042608580Z",
            "190426085809.5Z",
            "190426085809+0100",
            "1904260858a9Z",
            "191326085809Z",
            "190400085809Z",
            "190431085809Z",
            "190426245809Z",
            "190426086009Z",
            "190426085860Z",
        ] {
            assert_eq!(time_of(&utc(text)), None, "{text}");
        }
        assert_eq!(time_of(&generalized("20190426085809.5Z")), None);
        assert_eq!(time_of(&generalized("190426085809Z")), None);
        assert_eq!(time_of(&der(INTEGER, b"190426085809Z")), None);
    }
}
