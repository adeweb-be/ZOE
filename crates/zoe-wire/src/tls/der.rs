//! The parts of a DER certificate (RFC 5280) that a pinned certificate
//! needs. webpki does not give them for a certificate that it does not
//! verify.
//!
//! One TLV reader and one date arithmetic serve two readers. [`validity`]
//! reads the validity period, for the server. [`parts`] also reads the
//! common names of the subject and whether the certificate has a
//! subjectAltName extension, for the name check of the client. So each
//! side pins the certificates that it reads.

use std::time::Duration;

use rustls::pki_types::UnixTime;

use super::Validity;

/// The DER tags that the readers use.
const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const OID: u8 = 0x06;
const OCTET_STRING: u8 = 0x04;
const BOOLEAN: u8 = 0x01;
const VERSION: u8 = 0xa0;
const ISSUER_UID: u8 = 0x81;
const SUBJECT_UID: u8 = 0x82;
const EXTENSIONS: u8 = 0xa3;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
const UTF8_STRING: u8 = 0x0c;
const PRINTABLE_STRING: u8 = 0x13;
const IA5_STRING: u8 = 0x16;

/// The OID of the commonName attribute (2.5.4.3), without its tag.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// The OID of the subjectAltName extension (2.5.29.17), without its tag.
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// The parts of a certificate that a pinned certificate of the client
/// needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parts {
    /// The notBefore of the certificate.
    pub not_before: UnixTime,
    /// The notAfter of the certificate.
    pub not_after: UnixTime,
    /// The common names of the subject, as text. A common name that is not
    /// a UTF8String, a PrintableString or an IA5String is not in it.
    pub common_names: Vec<String>,
    /// The certificate has a subjectAltName extension.
    pub has_alt_names: bool,
}

impl Parts {
    /// Return the validity period of the certificate.
    pub fn validity(&self) -> Validity {
        Validity {
            not_before: self.not_before,
            not_after: self.not_after,
        }
    }
}

/// Read the validity period of the DER certificate `der` (RFC 5280,
/// 4.1.2.5): its notBefore and its notAfter. Return `None` when the
/// certificate does not read, when a time is before 1970, or when
/// notBefore is after notAfter.
pub fn validity(der: &[u8]) -> Option<Validity> {
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
    let (not_before, not_after) = period(validity)?;
    Some(Validity {
        not_before,
        not_after,
    })
}

/// Read the parts of the DER certificate `der`. Return `None` when the
/// certificate does not read, when a time is before 1970, or when
/// notBefore is after notAfter.
pub fn parts(der: &[u8]) -> Option<Parts> {
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
    let (validity, tbs) = element(tbs, SEQUENCE)?;
    let (subject, tbs) = element(tbs, SEQUENCE)?;
    let (_key, tbs) = element(tbs, SEQUENCE)?;
    let tbs = element(tbs, ISSUER_UID).map_or(tbs, |(_, rest)| rest);
    let tbs = element(tbs, SUBJECT_UID).map_or(tbs, |(_, rest)| rest);
    let has_alt_names = match element(tbs, EXTENSIONS) {
        Some((extensions, _)) => has_extension(extensions, SUBJECT_ALT_NAME)?,
        None => false,
    };
    let (not_before, not_after) = period(validity)?;
    Some(Parts {
        not_before,
        not_after,
        common_names: common_names(subject)?,
        has_alt_names,
    })
}

/// Read the contents of a Validity: two times. Return `None` when they do
/// not read, when bytes follow them, or when notBefore is after notAfter.
fn period(validity: &[u8]) -> Option<(UnixTime, UnixTime)> {
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

/// Return the common names of the Name `name`: the contents of its
/// SEQUENCE OF RelativeDistinguishedName.
fn common_names(mut name: &[u8]) -> Option<Vec<String>> {
    let mut names = Vec::new();
    while !name.is_empty() {
        let (mut set, rest) = element(name, SET)?;
        name = rest;
        while !set.is_empty() {
            let (attribute, rest) = element(set, SEQUENCE)?;
            set = rest;
            let (oid, value) = element(attribute, OID)?;
            if oid != COMMON_NAME {
                continue;
            }
            let (&tag, _) = value.split_first()?;
            let (text, _) = element(value, tag)?;
            if matches!(tag, UTF8_STRING | PRINTABLE_STRING | IA5_STRING)
                && let Ok(text) = std::str::from_utf8(text)
            {
                names.push(text.to_owned());
            }
        }
    }
    Some(names)
}

/// Tell if the contents of the `[3]` extensions of a certificate hold the
/// extension `oid`. Return `None` when they do not read.
fn has_extension(extensions: &[u8], oid: &[u8]) -> Option<bool> {
    let (mut list, _) = element(extensions, SEQUENCE)?;
    let mut found = false;
    while !list.is_empty() {
        let (extension, rest) = element(list, SEQUENCE)?;
        list = rest;
        let (id, value) = element(extension, OID)?;
        let value = element(value, BOOLEAN).map_or(value, |(_, rest)| rest);
        element(value, OCTET_STRING)?;
        found |= id == oid;
    }
    Some(found)
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
    /// The tests of the validity period, the reading of the server.
    mod validity_reader {
        use super::super::*;

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
        fn certificate(
            not_before: &[u8],
            not_after: &[u8],
            version: bool,
            issuer: usize,
        ) -> Vec<u8> {
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

        fn seconds(validity: Option<Validity>) -> Option<(u64, u64)> {
            validity.map(|validity| (validity.not_before.as_secs(), validity.not_after.as_secs()))
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

    /// The tests of the parts, the reading of the client.
    mod parts_reader {
        use super::super::*;

        /// Return the DER of a TLV with the tag `tag`. A length from 128 takes
        /// the long form, with two bytes.
        fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
            let len = contents.len();
            let mut out = match u8::try_from(len) {
                Ok(short) if short < 0x80 => vec![tag, short],
                _ => vec![tag, 0x82, (len >> 8) as u8, len as u8],
            };
            out.extend_from_slice(contents);
            out
        }

        /// Return the DER of a certificate with `validity`, `subject` and the
        /// `[3]` extensions `extensions`, if any. The other fields are empty.
        fn certificate(validity: &[u8], subject: &[u8], extensions: Option<&[u8]>) -> Vec<u8> {
            let mut tbs = tlv(INTEGER, &[1]);
            tbs.extend(tlv(SEQUENCE, &[]));
            tbs.extend(tlv(SEQUENCE, &[]));
            tbs.extend(tlv(SEQUENCE, validity));
            tbs.extend(tlv(SEQUENCE, subject));
            tbs.extend(tlv(SEQUENCE, &[]));
            if let Some(extensions) = extensions {
                tbs.extend(tlv(EXTENSIONS, &tlv(SEQUENCE, extensions)));
            }
            let mut body = tlv(SEQUENCE, &tbs);
            body.extend(tlv(SEQUENCE, &[]));
            tlv(SEQUENCE, &body)
        }

        fn validity(not_before: &[u8], not_after: &[u8]) -> Vec<u8> {
            let mut out = tlv(UTC_TIME, not_before);
            out.extend(tlv(GENERALIZED_TIME, not_after));
            out
        }

        fn common_name(tag: u8, text: &str) -> Vec<u8> {
            let mut attribute = tlv(OID, COMMON_NAME);
            attribute.extend(tlv(tag, text.as_bytes()));
            tlv(SET, &tlv(SEQUENCE, &attribute))
        }

        fn extension(oid: &[u8]) -> Vec<u8> {
            let mut out = tlv(OID, oid);
            out.extend(tlv(OCTET_STRING, &[]));
            tlv(SEQUENCE, &out)
        }

        fn seconds(time: UnixTime) -> u64 {
            time.as_secs()
        }

        #[test]
        fn the_parts_read_the_validity_the_common_names_and_the_alt_names() {
            let mut subject = common_name(PRINTABLE_STRING, "db.example");
            subject.extend(common_name(UTF8_STRING, "other.example"));
            // A common name in a BMPString is not text that the client reads.
            subject.extend(common_name(0x1e, "xx"));
            let time = validity(b"700102000000Z", b"20380119031407Z");
            let found = parts(&certificate(&time, &subject, None)).unwrap();
            assert_eq!(seconds(found.not_before), 86_400);
            assert_eq!(seconds(found.not_after), 2_147_483_647);
            assert_eq!(found.common_names, ["db.example", "other.example"]);
            assert!(!found.has_alt_names);
            let extensions = [extension(&[0x55, 0x1d, 0x13]), extension(SUBJECT_ALT_NAME)].concat();
            let found = parts(&certificate(&time, &subject, Some(&extensions))).unwrap();
            assert!(found.has_alt_names);
            let extensions = extension(&[0x55, 0x1d, 0x13]);
            let found = parts(&certificate(&time, &subject, Some(&extensions))).unwrap();
            assert!(!found.has_alt_names);
        }

        #[test]
        fn a_certificate_that_does_not_read_has_no_parts() {
            let time = validity(b"700102000000Z", b"20380119031407Z");
            let good = certificate(&time, &common_name(IA5_STRING, "a"), None);
            assert!(parts(&good).is_some());
            assert!(parts(&good[..good.len() - 1]).is_none());
            assert!(parts(&[good.as_slice(), &[0]].concat()).is_none());
            // notBefore after notAfter, a time before 1970, a bad month.
            for (before, after) in [
                (&b"300101000000Z"[..], &b"20290101000000Z"[..]),
                (b"690101000000Z", b"20290101000000Z"),
                (b"701301000000Z", b"20290101000000Z"),
            ] {
                let time = validity(before, after);
                let cert = certificate(&time, &common_name(IA5_STRING, "a"), None);
                assert!(parts(&cert).is_none());
            }
        }

        #[test]
        fn the_days_count_the_leap_years() {
            assert_eq!(days_since_epoch(1970, 1, 1), 0);
            assert_eq!(days_since_epoch(1972, 3, 1), 365 * 2 + 31 + 29);
            assert_eq!(days_since_epoch(2000, 3, 1), 11_017);
            assert_eq!(days_in_month(1900, 2), 28);
        }
    }
}
