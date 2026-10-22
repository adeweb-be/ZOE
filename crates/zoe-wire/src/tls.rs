//! The reading of the TLS certificates that the server and the client
//! share: the PEM loader, the pinned certificates and the DER reader.
//!
//! A pinned certificate is a certificate of a CA file that a peer
//! certificate matches byte for byte. OpenSSL accepts such a certificate,
//! also when it is a CA certificate, and ZEO's tests use one. The server
//! pins the certificates of its client CA file, and the client those of
//! `authenticate`. Each side keeps its own verifier of rustls.

use std::path::Path;

use rustls::CertificateError;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, UnixTime};

pub mod der;

pub use der::Parts;

/// Read the certificates in the PEM file `path`. One at least is
/// necessary.
pub fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
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

/// The validity period of a certificate: its notBefore and its notAfter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    /// The notBefore of the certificate.
    pub not_before: UnixTime,
    /// The notAfter of the certificate.
    pub not_after: UnixTime,
}

impl Validity {
    /// Check that `now` is within the period, its ends included. Return
    /// the error of rustls for a certificate that is not valid yet, or
    /// that expired.
    pub fn check(&self, now: UnixTime) -> Result<(), CertificateError> {
        if now < self.not_before {
            return Err(CertificateError::NotValidYetContext {
                time: now,
                not_before: self.not_before,
            });
        }
        if now > self.not_after {
            return Err(CertificateError::ExpiredContext {
                time: now,
                not_after: self.not_after,
            });
        }
        Ok(())
    }
}

/// The certificates of a CA file that a peer certificate can match byte
/// for byte, each with what a reader of its DER gave (`P`). A certificate
/// that the reader does not read never matches.
#[derive(Debug)]
pub struct Pinned<P> {
    certs: Box<[(CertificateDer<'static>, P)]>,
}

impl<P> Default for Pinned<P> {
    /// No pinned certificate.
    fn default() -> Self {
        Self {
            certs: Box::new([]),
        }
    }
}

impl<P> Pinned<P> {
    /// Read each certificate of `certs` with `reader`, and keep those that
    /// it reads.
    pub fn read(certs: &[CertificateDer<'static>], reader: impl Fn(&[u8]) -> Option<P>) -> Self {
        Self {
            certs: certs
                .iter()
                .filter_map(|der| Some((der.clone(), reader(der)?)))
                .collect(),
        }
    }

    /// Return what the reader gave for the pinned certificate whose bytes
    /// are the bytes of `end_entity`, if one is.
    pub fn find(&self, end_entity: &[u8]) -> Option<&P> {
        self.certs
            .iter()
            .find(|(der, _)| der.as_ref() == end_entity)
            .map(|(_, read)| read)
    }

    /// Return the number of pinned certificates.
    pub fn len(&self) -> usize {
        self.certs.len()
    }

    /// Tell if no certificate is pinned.
    pub fn is_empty(&self) -> bool {
        self.certs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64) -> UnixTime {
        UnixTime::since_unix_epoch(Duration::from_secs(secs))
    }

    #[test]
    fn a_validity_holds_from_its_start_to_its_end() {
        let validity = Validity {
            not_before: at(100),
            not_after: at(200),
        };
        for now in [100, 150, 200] {
            assert_eq!(validity.check(at(now)), Ok(()));
        }
        assert_eq!(
            validity.check(at(99)),
            Err(CertificateError::NotValidYetContext {
                time: at(99),
                not_before: at(100),
            })
        );
        assert_eq!(
            validity.check(at(201)),
            Err(CertificateError::ExpiredContext {
                time: at(201),
                not_after: at(200),
            })
        );
    }

    #[test]
    fn a_pinned_certificate_matches_by_its_bytes_once_it_reads() {
        let certs = [
            CertificateDer::from(vec![1, 2, 3]),
            CertificateDer::from(vec![9]),
        ];
        // The reader reads only the certificates of 3 bytes.
        let pinned = Pinned::read(&certs, |der| (der.len() == 3).then_some(der[0]));
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned.find(&[1, 2, 3]), Some(&1));
        for other in [&[9][..], &[1, 2], &[1, 2, 4], &[]] {
            assert_eq!(pinned.find(other), None, "{other:?}");
        }
        assert!(Pinned::<u8>::default().is_empty());
    }

    #[test]
    fn a_file_without_a_certificate_names_the_file() {
        let dir = std::env::temp_dir().join(format!("zoe-wire-pem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "no PEM here\n").unwrap();
        let error = certificates(&empty).unwrap_err();
        assert!(error.ends_with("empty.pem: no certificate"), "{error}");
        let error = certificates(&dir.join("missing.pem")).unwrap_err();
        assert!(error.contains("missing.pem"), "{error}");
    }
}
