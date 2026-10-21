//! Identifiers of the data model.

/// An object id. ZODB writes it as 8 big-endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub u64);

/// A transaction id. It is the 8-byte ZODB `TimeStamp` and it always
/// increases. The serial of a record is the tid that wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid(pub u64);

impl Oid {
    /// Return the oid in ZODB form, as `ZODB.utils.oid_repr` writes it:
    /// `0x`, then the hex digits without leading zeros, padded to an even
    /// count. For example `0x00` or `0x0100`.
    pub fn repr(self) -> String {
        repr(self.0)
    }

    /// Return the oid as `0x` and 16 hex digits, for example
    /// `0x0000000000000100`.
    pub fn hex(self) -> String {
        hex(self.0)
    }

    /// Read an oid in ZODB form: `0x` and 1 to 16 hex digits. Return
    /// `None` for another text.
    pub fn parse(text: &str) -> Option<Self> {
        parse(text).map(Self)
    }
}

impl Tid {
    /// The tid before every transaction. A client gives it as the serial
    /// when it stores a new object.
    pub const ZERO: Tid = Tid(0);

    /// The tid after every transaction. `loadBefore` with it loads the
    /// current record.
    pub const MAX: Tid = Tid(u64::MAX);

    /// Return the tid in ZODB form, as `ZODB.utils.tid_repr` writes it:
    /// `0x`, then the hex digits without leading zeros, padded to an even
    /// count. For example `0x033f2eeca6666666`.
    pub fn repr(self) -> String {
        repr(self.0)
    }

    /// Return the tid as `0x` and 16 hex digits, for example
    /// `0x0000000000000010`.
    pub fn hex(self) -> String {
        hex(self.0)
    }

    /// Read a tid in ZODB form: `0x` and 1 to 16 hex digits. Return
    /// `None` for another text.
    pub fn parse(text: &str) -> Option<Self> {
        parse(text).map(Self)
    }
}

/// Return an 8-byte id in ZODB form. Zero is `0x00`.
fn repr(id: u64) -> String {
    let digits = format!("{id:x}");
    let pad = if digits.len() % 2 == 1 { "0" } else { "" };
    format!("0x{pad}{digits}")
}

/// Return an 8-byte id as `0x` and 16 hex digits.
fn hex(id: u64) -> String {
    format!("{id:#018x}")
}

/// Read an 8-byte id in ZODB form: `0x` and 1 to 16 hex digits.
fn parse(text: &str) -> Option<u64> {
    let digits = text
        .strip_prefix("0x")
        .filter(|digits| (1..=16).contains(&digits.len()))
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))?;
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_has_its_hex_form_and_reads_from_its_zodb_form() {
        assert_eq!(Oid(0x100).hex(), "0x0000000000000100");
        assert_eq!(Tid(u64::MAX).hex(), "0xffffffffffffffff");
        for id in [0, 1, 0x100, u64::MAX] {
            assert_eq!(Oid::parse(&Oid(id).repr()), Some(Oid(id)));
            assert_eq!(Tid::parse(&Tid(id).hex()), Some(Tid(id)));
        }
        for bad in ["", "0x", "12", "0xg1", "0x00000000000000001", "0x+1"] {
            assert_eq!(Oid::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn repr_strips_leading_zeros_to_an_even_count() {
        assert_eq!(repr(0), "0x00");
        assert_eq!(repr(0x0b), "0x0b");
        assert_eq!(repr(0x1ab), "0x01ab");
        assert_eq!(repr(u64::MAX), "0xffffffffffffffff");
    }
}
