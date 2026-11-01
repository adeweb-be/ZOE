//! The text forms of pickle values: str, Python 2 str, bytes and ints.

use zoe_pickle::{ByteString, Int, Node, Resolver, Unicode, raw_unicode, unquote};

/// Return the bytes of a Python 2 `str`.
pub(crate) fn byte_string(string: &ByteString) -> Vec<u8> {
    byte_string_start(string, usize::MAX).0
}

/// Return the first `limit` bytes of a Python 2 `str`, and tell if they
/// are all of it. Decode no more of it than that.
pub(crate) fn byte_string_start(string: &ByteString, limit: usize) -> (Vec<u8>, bool) {
    match string {
        ByteString::Binary(bytes) => {
            let end = limit.min(bytes.data.len());
            (bytes.data[..end].to_vec(), end == bytes.data.len())
        }
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
    unicode_start(text, usize::MAX).0
}

/// Return the start of a `str`: at most `limit` bytes of UTF-8, cut at a
/// character boundary. Also tell if it is all of the text. Decode no more
/// of it than that.
pub(crate) fn unicode_start(text: &Unicode, limit: usize) -> (String, bool) {
    match text {
        Unicode::Binary(text) => {
            let end = text.floor_char_boundary(limit);
            (text[..end].to_owned(), end == text.len())
        }
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

/// Return the size of a character in a JSON string, as serde_json writes
/// it: a quote and a backslash take two bytes, a control character two or
/// six.
pub(crate) fn json_cost(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Return the longest start of `text` whose JSON size is at most
/// `allowance` bytes: the end of the start and its JSON size.
pub(crate) fn fit(text: &str, allowance: usize) -> (usize, usize) {
    let mut cost = 0;
    for (index, c) in text.char_indices() {
        let next = cost + json_cost(c);
        if next > allowance {
            return (index, cost);
        }
        cost = next;
    }
    (text.len(), cost)
}

/// Write the start of `bytes` as Python writes a bytes literal, without
/// the quotes: printable ASCII as it is, other bytes as escapes. Stop
/// before the JSON size of the text gets above `allowance`. Return the
/// text, the count of bytes that it shows and its JSON size.
pub(crate) fn escape_within(bytes: &[u8], allowance: usize) -> (String, usize, usize) {
    let mut out = String::new();
    let mut cost = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let start = out.len();
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
        let piece: usize = out[start..].chars().map(json_cost).sum();
        if cost + piece > allowance {
            out.truncate(start);
            return (out, index, cost);
        }
        cost += piece;
    }
    (out, bytes.len(), cost)
}

/// Return bytes as lowercase hex digits.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The value of an int node.
pub(crate) enum IntValue {
    /// An int that fits an `i128`.
    Small(i128),
    /// A larger int: the start of its decimal text or hex text, the size
    /// of the int in the record, and if the start is the whole text.
    Large {
        start: String,
        length: usize,
        whole: bool,
    },
}

/// Return the value of an int node. The text of a large int has at most
/// `limit` bytes, and the call reads no more of the int than that needs.
pub(crate) fn int(int: &Int, limit: usize) -> IntValue {
    match int {
        Int::Long { bytes, .. } => long(bytes, limit),
        Int::Text(text) => decimal(text, limit),
        Int::LongText(text) => decimal(text.strip_suffix('L').unwrap_or(text), limit),
        // BININT1, BININT2 and BININT always fit.
        Int::U8(_) | Int::U16(_) | Int::I32(_) => {
            IntValue::Small(int.to_i128().unwrap_or_default())
        }
    }
}

/// Read decimal text. Text that does not fit an `i128` stays text. The
/// decoder gives only digits and a sign, so the text is ASCII.
fn decimal(text: &str, limit: usize) -> IntValue {
    // An i128 has at most 39 digits and a sign.
    if text.len() <= 40
        && let Ok(value) = text.parse::<i128>()
    {
        return IntValue::Small(value);
    }
    let end = limit.min(text.len());
    IntValue::Large {
        start: text[..end].to_owned(),
        length: text.len(),
        whole: end == text.len(),
    }
}

/// Read the little-endian two's complement bytes of LONG1 or LONG4. More
/// than 16 bytes give the hex text of the value, from its most
/// significant digits.
fn long(bytes: &[u8], limit: usize) -> IntValue {
    if let Some(value) = Int::long_to_i128(bytes) {
        return IntValue::Small(value);
    }
    let negative = bytes.last().is_some_and(|byte| byte & 0x80 != 0);
    let cut = |start: String| IntValue::Large {
        start,
        length: bytes.len(),
        whole: false,
    };
    if limit == 0 {
        return cut(String::new());
    }
    // The magnitude of a negative int is its two's complement negated:
    // below the lowest byte that is not zero, the bytes are zero; that byte
    // is negated; the bytes above it are inverted.
    let lowest = bytes.iter().position(|&byte| byte != 0).unwrap_or(0);
    let magnitude = |index: usize| match (negative, index.cmp(&lowest)) {
        (false, _) => bytes[index],
        (true, std::cmp::Ordering::Less) => 0,
        (true, std::cmp::Ordering::Equal) => bytes[index].wrapping_neg(),
        (true, std::cmp::Ordering::Greater) => !bytes[index],
    };
    let mut text = String::from(if negative { "-0x" } else { "0x" });
    let mut digits = (0..bytes.len())
        .rev()
        .map(magnitude)
        .skip_while(|&byte| byte == 0)
        .peekable();
    if digits.peek().is_none() {
        text.push('0');
    }
    for byte in digits {
        if text.len() + 2 > limit {
            text.truncate(limit);
            return cut(text);
        }
        text.push_str(&format!("{byte:02x}"));
    }
    if text.len() > limit {
        text.truncate(limit);
        return cut(text);
    }
    IntValue::Large {
        start: text,
        length: bytes.len(),
        whole: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_stops_at_the_limit() {
        assert_eq!(unquote(br"'a\x00bc'", 2).0, b"a\x00");
        assert_eq!(raw_unicode(b"ab\\u00e9c", 3).0, "ab");
        assert_eq!(raw_unicode(b"ab\\u00e9c", 4).0, "ab\u{e9}");
        let text = Unicode::Binary("a\u{e9}b".to_owned());
        assert_eq!(unicode_start(&text, 2), ("a".to_owned(), false));
        assert_eq!(unicode_start(&text, 4), ("a\u{e9}b".to_owned(), true));
        assert_eq!(unicode_len(&text), 4);
    }

    #[test]
    fn bytes_escape_as_in_python() {
        let (text, shown, cost) = escape_within(b"a\x00\n\\\xff\"", usize::MAX);
        assert_eq!(text, r#"a\x00\n\\\xff""#);
        assert_eq!(shown, 6);
        // The JSON string doubles each backslash and the quote.
        assert_eq!(cost, serde_json::to_string(&text).unwrap().len() - 2);
        let (text, shown, cost) = escape_within(b"ab\x00c", 6);
        assert_eq!((text.as_str(), shown, cost), ("ab", 2, 2));
    }

    #[test]
    fn a_start_fits_its_json_size() {
        assert_eq!(fit("ab\"c", 3), (2, 2));
        assert_eq!(fit("ab\"c", 4), (3, 4));
        assert_eq!(fit("a\u{1}", 6), (1, 1));
        assert_eq!(fit("a\u{e9}", 3), (3, 3));
    }

    /// Return the start and the wholeness of a large int.
    fn large(value: IntValue) -> (String, bool) {
        match value {
            IntValue::Small(value) => panic!("{value}"),
            IntValue::Large { start, whole, .. } => (start, whole),
        }
    }

    #[test]
    fn longs_read_as_twos_complement() {
        let small = |bytes: &[u8]| match long(bytes, usize::MAX) {
            IntValue::Small(value) => value,
            IntValue::Large { start, .. } => panic!("{start}"),
        };
        assert_eq!(small(b""), 0);
        assert_eq!(small(b"\xff"), -1);
        assert_eq!(small(b"\x00\x80\x00"), 0x8000);
        assert_eq!(small(b"\x00\x80"), -0x8000);
        assert_eq!(small(&[0xff; 16]), -1);
        let mut bytes = vec![0; 16];
        bytes.push(1);
        let whole = format!("0x01{}", "00".repeat(16));
        assert_eq!(large(long(&bytes, usize::MAX)), (whole.clone(), true));
        assert_eq!(large(long(&bytes, 6)), (whole[..6].to_owned(), false));
        assert_eq!(large(long(&bytes, 0)), (String::new(), false));
        assert_eq!(
            large(long(&[0xff; 17], usize::MAX)),
            ("-0x01".to_owned(), true)
        );
        assert_eq!(large(long(&[0; 17], usize::MAX)), ("0x0".to_owned(), true));
    }

    #[test]
    fn a_negative_long_with_zero_low_bytes_has_its_magnitude() {
        // -2^(8*20): twenty zero bytes, then 0x00 0xff with the sign bit.
        let mut bytes = vec![0; 20];
        bytes.push(0xff);
        let (text, whole) = large(long(&bytes, usize::MAX));
        assert!(whole);
        assert_eq!(text, format!("-0x01{}", "00".repeat(20)));
        // -0x0102: the low byte 0xfe is negated, the next is inverted.
        let mut bytes = vec![0xfe, 0xfe];
        bytes.extend([0xff; 15]);
        assert_eq!(large(long(&bytes, usize::MAX)).0, "-0x0102");
    }

    #[test]
    fn decimal_text_reads_as_an_int() {
        assert!(matches!(decimal("-12", usize::MAX), IntValue::Small(-12)));
        let huge = "1".repeat(50);
        assert_eq!(large(decimal(&huge, usize::MAX)), (huge.clone(), true));
        assert_eq!(large(decimal(&huge, 3)), ("111".to_owned(), false));
    }
}
