//! Checks of the text arguments that the protocol-0 opcodes read up to a
//! newline, and of the UTF-8 of BINUNICODE. Each check accepts a subset of
//! what Python's unpickler accepts. It accepts all that Python's picklers
//! write.

/// Checks the text of INT or LONG: an optional sign, then decimal digits.
/// A number with two digits or more does not start with 0, unless all its
/// digits are 0, as for `int(text, 0)`. The text of LONG can end with `L`.
pub fn is_int(text: &[u8], long: bool) -> bool {
    let text = match text.strip_suffix(b"L") {
        Some(number) if long => number,
        _ => text,
    };
    let digits = match text {
        [b'-' | b'+', digits @ ..] => digits,
        digits => digits,
    };
    match digits {
        [] => false,
        [b'0', ..] => digits.iter().all(|digit| *digit == b'0'),
        _ => digits.iter().all(u8::is_ascii_digit),
    }
}

/// Reads the text of INT as a bool, as the C unpickler of Python does. A
/// text of two characters that gives 0 or 1 is a bool: `00`, `+0` and `-0`
/// give False, and `01` and `+1` give True. Other texts give `None`.
pub fn int_bool(text: &[u8]) -> Option<bool> {
    match text {
        b"01" | b"+1" => Some(true),
        b"00" | b"+0" | b"-0" => Some(false),
        _ => None,
    }
}

/// Checks the text of FLOAT: a number that Rust can parse as an `f64`,
/// for example `1.5`, `-1e+300`, `inf` or `nan`. The C unpickler refuses a
/// number too large for an `f64`, such as `1e999`, so this check does too.
pub fn is_float(text: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(text) else {
        return false;
    };
    match text.parse::<f64>() {
        Ok(value) if value.is_infinite() => {
            let unsigned = text.trim_start_matches(['+', '-']).to_ascii_lowercase();
            unsigned == "inf" || unsigned == "infinity"
        }
        Ok(_) => true,
        Err(_) => false,
    }
}

/// Checks the text of STRING: the value in quotes, with the escapes that
/// `codecs.escape_decode` reads. Python refuses a `\x` without two hex
/// digits and a backslash at the end.
pub fn is_quoted(text: &[u8]) -> bool {
    let [quote @ (b'\'' | b'"'), inner @ .., last] = text else {
        return false;
    };
    if quote != last || text.contains(&b'\n') {
        return false;
    }
    let mut rest = inner;
    while let Some(at) = rest.iter().position(|byte| *byte == b'\\') {
        match rest.get(at + 1) {
            None => return false,
            Some(b'x') => {
                let hex = rest.get(at + 2..at + 4);
                if !hex.is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit)) {
                    return false;
                }
                rest = &rest[at + 4..];
            }
            Some(_) => rest = &rest[at + 2..],
        }
    }
    true
}

/// Checks the text of UNICODE, which is in raw-unicode-escape: a `\u` has
/// four hex digits, and a `\U` has eight that give at most 0x10FFFF.
pub fn is_raw_unicode(text: &[u8]) -> bool {
    if text.contains(&b'\n') {
        return false;
    }
    let mut rest = text;
    while let Some(at) = rest.iter().position(|byte| *byte == b'\\') {
        let digits = match rest.get(at + 1) {
            Some(b'u') => 4,
            Some(b'U') => 8,
            Some(_) => {
                rest = &rest[at + 2..];
                continue;
            }
            None => break,
        };
        let Some(hex) = rest.get(at + 2..at + 2 + digits) else {
            return false;
        };
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return false;
        }
        let value = hex
            .iter()
            .fold(0_u64, |value, digit| value * 16 + hex_value(*digit));
        if value > 0x10_FFFF {
            return false;
        }
        rest = &rest[at + 2 + digits..];
    }
    true
}

/// Reads the text of PUT or GET: a memo index in decimal, with no sign and
/// no leading zero, as Python's picklers write it.
pub fn index(text: &[u8]) -> Option<u32> {
    match text {
        [b'0'] => Some(0),
        [b'1'..=b'9', ..] if text.iter().all(u8::is_ascii_digit) => {
            std::str::from_utf8(text).ok()?.parse().ok()
        }
        _ => None,
    }
}

/// Checks the bytes of BINUNICODE that are not UTF-8. Python reads them
/// with `surrogatepass`, so it accepts UTF-8 with encoded surrogates
/// (U+D800 to U+DFFF) and refuses other bytes.
pub fn is_utf8_with_surrogates(bytes: &[u8]) -> bool {
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(_) => return true,
            Err(error) => {
                let at = error.valid_up_to();
                match rest[at..] {
                    [0xed, 0xa0..=0xbf, 0x80..=0xbf, ..] => rest = &rest[at + 3..],
                    _ => return false,
                }
            }
        }
    }
}

fn hex_value(digit: u8) -> u64 {
    u64::from(match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_text_follows_python() {
        for text in ["0", "42", "-5", "+5", "000", "1180591620717411303424"] {
            assert!(is_int(text.as_bytes(), false), "{text}");
        }
        for text in ["", "-", "007", "1_000", " 1", "0x10", "1L", "1.0"] {
            assert!(!is_int(text.as_bytes(), false), "{text}");
        }
        assert!(is_int(b"-1208925819614629174706176L", true));
        assert!(is_int(b"12", true));
        assert!(!is_int(b"L", true));
        assert!(!is_int(b"12LL", true));
    }

    #[test]
    fn two_characters_that_give_0_or_1_are_a_bool() {
        for (text, value) in [
            ("01", true),
            ("+1", true),
            ("00", false),
            ("+0", false),
            ("-0", false),
        ] {
            assert_eq!(int_bool(text.as_bytes()), Some(value), "{text}");
        }
        for text in ["0", "1", "-1", "10", "001", "+01"] {
            assert_eq!(int_bool(text.as_bytes()), None, "{text}");
        }
    }

    #[test]
    fn float_text_is_a_number() {
        for text in ["1.5", "-2.25", "1e+300", "5e-324", "inf", "-inf", "nan"] {
            assert!(is_float(text.as_bytes()), "{text}");
        }
        for text in ["Infinity", "+inf", "-1e300"] {
            assert!(is_float(text.as_bytes()), "{text}");
        }
        for text in ["", "1.5x", "one", "\u{e9}", "1e999", "-1e999"] {
            assert!(!is_float(text.as_bytes()), "{text}");
        }
    }

    #[test]
    fn string_text_is_quoted_with_valid_escapes() {
        for text in [
            &b"''"[..],
            b"'abc'",
            b"\"it's\"",
            b"'a\\x00b'",
            b"'\\n\\t\\\\'",
            b"'\\7'",
        ] {
            assert!(is_quoted(text), "{text:?}");
        }
        for text in [
            &b"abc"[..],
            b"'",
            b"'abc\"",
            b"'a\\x0'",
            b"'a\\xzz'",
            b"'a\\'",
        ] {
            assert!(!is_quoted(text), "{text:?}");
        }
    }

    #[test]
    fn unicode_text_has_valid_escapes() {
        for text in [
            &b"abc"[..],
            b"caf\xe9",
            b"\\u00e9",
            b"\\U0001f511",
            b"\\ud800",
            b"a\\b",
            b"\\\\u12",
            b"end\\",
        ] {
            assert!(is_raw_unicode(text), "{text:?}");
        }
        for text in [&b"\\u12"[..], b"\\u12zz", b"\\U00110000", b"a\nb"] {
            assert!(!is_raw_unicode(text), "{text:?}");
        }
    }

    #[test]
    fn index_text_is_canonical() {
        assert_eq!(index(b"0"), Some(0));
        assert_eq!(index(b"300"), Some(300));
        assert_eq!(index(b"4294967295"), Some(u32::MAX));
        for text in [&b""[..], b"00", b"07", b"-1", b"+1", b"4294967296", b"1a"] {
            assert_eq!(index(text), None, "{text:?}");
        }
    }

    #[test]
    fn surrogates_pass_and_other_bad_bytes_do_not() {
        assert!(is_utf8_with_surrogates(b"lone-\xed\xa0\x80-surrogate"));
        assert!(is_utf8_with_surrogates(b"\xed\xbf\xbf"));
        assert!(!is_utf8_with_surrogates(b"\xff"));
        assert!(!is_utf8_with_surrogates(b"\xed\xa0"));
        assert!(!is_utf8_with_surrogates(b"\xc3"));
    }
}
