//! The ids at the IPC boundary. Oids and tids cross it as ZODB-form
//! strings, as `ZODB.utils.oid_repr` writes them: `0x00`, `0x0100`.

use serde::Serializer;
use zoe_core::{Oid, Tid};

use crate::{Error, Result};

/// Return `oid` in ZODB form, as `ZODB.utils.oid_repr` writes it, for
/// example `0x00` or `0x0100`.
pub fn oid_repr(oid: Oid) -> String {
    oid.repr()
}

/// Return `tid` in ZODB form, for example `0x033f2eeca6666666`.
pub fn tid_repr(tid: Tid) -> String {
    tid.repr()
}

/// Read an oid in ZODB form: `0x` and 1 to 16 hex digits.
pub fn parse_oid(text: &str) -> Result<Oid> {
    Oid::parse(text).ok_or_else(|| Error::BadId(text.to_owned()))
}

/// Read a tid in ZODB form: `0x` and 1 to 16 hex digits.
pub fn parse_tid(text: &str) -> Result<Tid> {
    Tid::parse(text).ok_or_else(|| Error::BadId(text.to_owned()))
}

/// Serialize `oid` in ZODB form.
pub(crate) fn serialize_oid<S: Serializer>(
    oid: &Oid,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&oid_repr(*oid))
}

/// Serialize `tid` in ZODB form.
pub(crate) fn serialize_tid<S: Serializer>(
    tid: &Tid,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&tid_repr(*tid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_zodb_form() {
        assert_eq!(oid_repr(Oid(0)), "0x00");
        assert_eq!(oid_repr(Oid(1)), "0x01");
        assert_eq!(oid_repr(Oid(0x76)), "0x76");
        assert_eq!(oid_repr(Oid(0x100)), "0x0100");
        assert_eq!(tid_repr(Tid(0x033f2eeca6666666)), "0x033f2eeca6666666");
        assert_eq!(oid_repr(Oid(u64::MAX)), "0xffffffffffffffff");
    }

    #[test]
    fn an_oid_in_zodb_form_parses() {
        for oid in [0, 1, 0x76, 0x100, u64::MAX] {
            assert_eq!(parse_oid(&oid_repr(Oid(oid))).unwrap(), Oid(oid));
        }
        assert_eq!(parse_oid("0x1").unwrap(), Oid(1));
        for bad in [
            "",
            "0x",
            "12",
            "0xg1",
            "0x00000000000000001",
            "0x-1",
            "0x+1",
        ] {
            assert!(parse_oid(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_tid_in_zodb_form_parses() {
        let tid = Tid(0x033f2eeca6666666);
        assert_eq!(parse_tid(&tid_repr(tid)).unwrap(), tid);
        assert!(matches!(parse_tid("latest"), Err(Error::BadId(_))));
    }
}
