//! The text forms of pickle values: str, Python 2 str, bytes and ints.

use zoe_pickle::{ByteString, Int, Node, Resolver, Unicode};

/// Return the bytes of a Python 2 `str`.
pub(crate) fn byte_string(string: &ByteString) -> Vec<u8> {
    byte_string_start(string, usize::MAX)
}

/// Return the first `limit` bytes of a Python 2 `str`. Decode no more of
/// it than that.
pub(crate) fn byte_string_start(string: &ByteString, limit: usize) -> Vec<u8> {
    match string {
        ByteString::Binary(bytes) => bytes.data[..limit.min(bytes.data.len())].to_vec(),
        ByteString::Text(quoted) => unquote(quoted, limit),
    }
}

/// Return the size of a Python 2 `str` in the record.
pub(crate) fn byte_string_len(string: &ByteString) -> usize {
    match string {
        ByteString::Binary(bytes) => bytes.data.len(),
        ByteString::Text(quoted) => quoted.len(),
    }
}

/// Return the text of a `str`.
pub(crate) fn unicode(text: &Unicode) -> String {
    unicode_start(text, usize::MAX)
}

/// Return the start of a `str`: at most `limit` bytes of UTF-8, cut at a
/// character boundary. Decode no more of it than that.
pub(crate) fn unicode_start(text: &Unicode, limit: usize) -> String {
    match text {
        Unicode::Binary(text) => text[..text.floor_char_boundary(limit)].to_owned(),
        Unicode::Text(raw) => raw_unicode(raw, limit),
    }
}

/// Return the size of a `str` in the record.
pub(crate) fn unicode_len(text: &Unicode) -> usize {
    match text {
        Unicode::Binary(text) => text.len(),
        Unicode::Text(raw) => raw.len(),
    }
}

/// Return the size in the record of a str node or a Python 2 `str` node.
/// Look through memo puts and gets. Other nodes give `None`.
pub(crate) fn text_len<'t>(resolver: &Resolver<'t>, node: &'t Node) -> Option<usize> {
    match resolver.resolve(node)? {
        Node::Unicode(text) => Some(unicode_len(text)),
        Node::ByteString(string) => Some(byte_string_len(string)),
        _ => None,
    }
}

/// Return the text of a str node, or of a Python 2 `str` node that holds
/// UTF-8. Look through memo puts and gets. Other nodes give `None`.
pub(crate) fn text<'t>(resolver: &Resolver<'t>, node: &'t Node) -> Option<String> {
    match resolver.resolve(node)? {
        Node::Unicode(text) => Some(unicode(text)),
        Node::ByteString(string) => String::from_utf8(byte_string(string)).ok(),
        _ => None,
    }
}

/// Return the bytes of an 8-byte oid node, as ZODB writes it in a
/// persistent id. Look through memo puts and gets.
pub(crate) fn oid_bytes<'t>(resolver: &Resolver<'t>, node: &'t Node) -> Option<u64> {
    let bytes = match resolver.resolve(node)? {
        Node::Bytes(bytes) => bytes.data.clone(),
        Node::ByteString(string) => byte_string(string),
        _ => return None,
    };
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Decode the text of STRING without its quotes: the escapes of
/// `codecs.escape_decode`. An unknown escape stays as it is. Stop after
/// `limit` bytes.
fn unquote(quoted: &[u8], limit: usize) -> Vec<u8> {
    let inner = quoted.get(1..quoted.len().saturating_sub(1)).unwrap_or(&[]);
    let mut out = Vec::with_capacity(inner.len().min(limit));
    let mut i = 0;
    while i < inner.len() && out.len() < limit {
        let byte = inner[i];
        i += 1;
        if byte != b'\\' || i == inner.len() {
            out.push(byte);
            continue;
        }
        let escape = inner[i];
        i += 1;
        match escape {
            b'\\' | b'\'' | b'"' => out.push(escape),
            b'\n' => {}
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'x' => match inner.get(i..i + 2).and_then(hex_byte) {
                Some(value) => {
                    out.push(value);
                    i += 2;
                }
                None => out.extend(*b"\\x"),
            },
            b'0'..=b'7' => {
                let mut value = u32::from(escape - b'0');
                for _ in 0..2 {
                    match inner.get(i) {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            other => out.extend([b'\\', other]),
        }
    }
    out.truncate(limit);
    out
}

/// Return the value of two hex digits.
fn hex_byte(digits: &[u8]) -> Option<u8> {
    u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

/// Decode raw-unicode-escape: each byte is a Latin-1 character, except
/// `\uXXXX` and `\UXXXXXXXX` after an odd count of backslashes. A
/// surrogate or a code point out of range gives U+FFFD. Stop after
/// `limit` bytes of UTF-8, at a character boundary.
fn raw_unicode(raw: &[u8], limit: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(limit));
    let mut i = 0;
    while i < raw.len() && out.len() < limit {
        if raw[i] != b'\\' {
            out.push(char::from(raw[i]));
            i += 1;
            continue;
        }
        let run = raw[i..].iter().take_while(|&&b| b == b'\\').count();
        i += run;
        let width = match raw.get(i) {
            Some(b'u') if run % 2 == 1 => 4,
            Some(b'U') if run % 2 == 1 => 8,
            _ => 0,
        };
        let digits = raw
            .get(i + 1..i + 1 + width)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()));
        match digits {
            Some(digits) if width > 0 => {
                out.extend(std::iter::repeat_n('\\', run - 1));
                let code = u32::from_str_radix(digits, 16).unwrap_or(0xfffd);
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                i += 1 + width;
            }
            _ => out.extend(std::iter::repeat_n('\\', run)),
        }
    }
    out.truncate(out.floor_char_boundary(limit));
    out
}

/// Return bytes as Python writes them in a bytes literal, without the
/// quotes: printable ASCII as it is, other bytes as escapes.
pub(crate) fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out
}

/// Return bytes as lowercase hex digits.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The value of an int node.
pub(crate) enum IntValue {
    /// An int that fits an `i128`.
    Small(i128),
    /// The decimal text or the hex text of a larger int.
    Large(String),
}

/// Return the value of an int node.
pub(crate) fn int(int: &Int) -> IntValue {
    match int {
        Int::U8(value) => IntValue::Small(i128::from(*value)),
        Int::U16(value) => IntValue::Small(i128::from(*value)),
        Int::I32(value) => IntValue::Small(i128::from(*value)),
        Int::Long { bytes, .. } => long(bytes),
        Int::Text(text) => decimal(text),
        Int::LongText(text) => decimal(text.strip_suffix('L').unwrap_or(text)),
    }
}

/// Read decimal text. Text that does not fit an `i128` stays text.
fn decimal(text: &str) -> IntValue {
    match text.parse::<i128>() {
        Ok(value) => IntValue::Small(value),
        Err(_) => IntValue::Large(text.to_owned()),
    }
}

/// Read the little-endian two's complement bytes of LONG1 or LONG4. More
/// than 16 bytes give the hex text of the value.
fn long(bytes: &[u8]) -> IntValue {
    let negative = bytes.last().is_some_and(|byte| byte & 0x80 != 0);
    if bytes.len() <= 16 {
        let fill = if negative { 0xff } else { 0 };
        let mut wide = [fill; 16];
        wide[..bytes.len()].copy_from_slice(bytes);
        return IntValue::Small(i128::from_le_bytes(wide));
    }
    // Negate the two's complement to get the magnitude.
    let mut magnitude = bytes.to_vec();
    if negative {
        let mut carry = true;
        for byte in &mut magnitude {
            *byte = !*byte;
            if carry {
                (*byte, carry) = byte.overflowing_add(1);
            }
        }
    }
    let mut digits: String = magnitude
        .iter()
        .rev()
        .skip_while(|&&byte| byte == 0)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digits.is_empty() {
        digits.push('0');
    }
    let sign = if negative { "-" } else { "" };
    IntValue::Large(format!("{sign}0x{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_escapes_decode() {
        assert_eq!(unquote(br"'a\'b'", usize::MAX), b"a'b");
        assert_eq!(unquote(br#""a\nb\x00\\""#, usize::MAX), b"a\nb\x00\\");
        assert_eq!(unquote(br"'\101\7\q'", usize::MAX), b"A\x07\\q");
        assert_eq!(unquote(br"'\xg0'", usize::MAX), b"\\xg0");
    }

    #[test]
    fn raw_unicode_escapes_decode() {
        assert_eq!(raw_unicode(b"caf\\u00e9", usize::MAX), "caf\u{e9}");
        assert_eq!(raw_unicode(br"\U0001f600", usize::MAX), "\u{1f600}");
        assert_eq!(raw_unicode(b"\xe9", usize::MAX), "\u{e9}");
        // An even count of backslashes is no escape.
        assert_eq!(raw_unicode(br"\\u0041", usize::MAX), r"\\u0041");
        assert_eq!(raw_unicode(br"\\\u0041", usize::MAX), r"\\A");
        assert_eq!(raw_unicode(br"\ud800", usize::MAX), "\u{fffd}");
        assert_eq!(raw_unicode(br"\u12", usize::MAX), r"\u12");
    }

    #[test]
    fn a_start_stops_at_the_limit() {
        assert_eq!(unquote(br"'a\x00bc'", 2), b"a\x00");
        assert_eq!(raw_unicode(b"ab\\u00e9c", 3), "ab");
        assert_eq!(raw_unicode(b"ab\\u00e9c", 4), "ab\u{e9}");
        let text = Unicode::Binary("a\u{e9}b".to_owned());
        assert_eq!(unicode_start(&text, 2), "a");
        assert_eq!(unicode_len(&text), 4);
    }

    #[test]
    fn bytes_escape_as_in_python() {
        assert_eq!(escape_bytes(b"a\x00\n\\\xff"), r"a\x00\n\\\xff");
    }

    #[test]
    fn longs_read_as_twos_complement() {
        let small = |bytes: &[u8]| match long(bytes) {
            IntValue::Small(value) => value,
            IntValue::Large(text) => panic!("{text}"),
        };
        assert_eq!(small(b""), 0);
        assert_eq!(small(b"\xff"), -1);
        assert_eq!(small(b"\x00\x80\x00"), 0x8000);
        assert_eq!(small(b"\x00\x80"), -0x8000);
        assert_eq!(small(&[0xff; 16]), -1);
        let large = |bytes: &[u8]| match long(bytes) {
            IntValue::Small(value) => panic!("{value}"),
            IntValue::Large(text) => text,
        };
        let mut bytes = vec![0; 16];
        bytes.push(1);
        assert_eq!(large(&bytes), format!("0x01{}", "00".repeat(16)));
        assert_eq!(large(&[0xff; 17]), "-0x01");
    }

    #[test]
    fn decimal_text_reads_as_an_int() {
        assert!(matches!(decimal("-12"), IntValue::Small(-12)));
        let huge = "1".repeat(50);
        assert!(matches!(decimal(&huge), IntValue::Large(text) if text == huge));
    }
}
