//! The pickle opcodes of protocols 0 to 3, and the readers of their
//! arguments. [`argument_len`] gives the length of the argument of an
//! opcode. [`memo_index`] reads the index of a put or a get from its
//! argument. [`memo_puts`] gives the memo indices that a sequence of whole
//! opcodes puts. The decoder of this crate refuses the opcodes of protocols
//! 4 and 5.

/// The highest PROTO value that Python accepts.
pub const HIGHEST_PROTOCOL: u8 = 5;

// Protocol 0.
/// Push a mark on the stack.
pub const MARK: u8 = b'(';
/// Stop: the pickle ends, and its value is on top of the stack.
pub const STOP: u8 = b'.';
/// Discard the value on top of the stack.
pub const POP: u8 = b'0';
/// Discard the values above the last mark, and the mark.
pub const POP_MARK: u8 = b'1';
/// Push the value on top of the stack again.
pub const DUP: u8 = b'2';
/// Push a float, written as decimal text and a newline.
pub const FLOAT: u8 = b'F';
/// Push an int, written as decimal text and a newline. `00` and `01` are
/// the bools.
pub const INT: u8 = b'I';
/// Push an int, written as decimal text, an optional `L` and a newline.
pub const LONG: u8 = b'L';
/// Push `None`.
pub const NONE: u8 = b'N';
/// Push the object of a persistent id, written as text and a newline.
pub const PERSID: u8 = b'P';
/// Pop a callable and a tuple of arguments. Push the result of the call.
pub const REDUCE: u8 = b'R';
/// Push a Python 2 str, written as quoted text and a newline.
pub const STRING: u8 = b'S';
/// Push a str, written as raw-unicode-escape text and a newline.
pub const UNICODE: u8 = b'V';
/// Pop a value and append it to the list on top of the stack.
pub const APPEND: u8 = b'a';
/// Pop a state and give it to the object on top of the stack.
pub const BUILD: u8 = b'b';
/// Push a class or a function: a module line and a name line.
pub const GLOBAL: u8 = b'c';
/// Make a dict of the keys and values above the last mark.
pub const DICT: u8 = b'd';
/// Push a value of the memo. The index is decimal text and a newline.
pub const GET: u8 = b'g';
/// Make an object of a class (a module line and a name line) with the
/// arguments above the last mark.
pub const INST: u8 = b'i';
/// Make a list of the values above the last mark.
pub const LIST: u8 = b'l';
/// Store the value on top of the stack in the memo. The index is decimal
/// text and a newline.
pub const PUT: u8 = b'p';
/// Pop a key and a value and set them in the dict on top of the stack.
pub const SETITEM: u8 = b's';
/// Make a tuple of the values above the last mark.
pub const TUPLE: u8 = b't';

// Protocol 1.
/// Push a float: 8 bytes, IEEE 754, big-endian.
pub const BINFLOAT: u8 = b'G';
/// Push an int: 4 bytes, signed, little-endian.
pub const BININT: u8 = b'J';
/// Push an int: 1 byte, unsigned.
pub const BININT1: u8 = b'K';
/// Push an int: 2 bytes, unsigned, little-endian.
pub const BININT2: u8 = b'M';
/// Pop a persistent id. Push its object.
pub const BINPERSID: u8 = b'Q';
/// Push a Python 2 str: a length of 4 bytes, then the bytes.
pub const BINSTRING: u8 = b'T';
/// Push a Python 2 str: a length of 1 byte, then the bytes.
pub const SHORT_BINSTRING: u8 = b'U';
/// Push a str: a length of 4 bytes, then the UTF-8 bytes.
pub const BINUNICODE: u8 = b'X';
/// Append the values above the last mark to the list below the mark.
pub const APPENDS: u8 = b'e';
/// Push a value of the memo. The index is 1 byte.
pub const BINGET: u8 = b'h';
/// Push a value of the memo. The index is 4 bytes, little-endian.
pub const LONG_BINGET: u8 = b'j';
/// Push an empty list.
pub const EMPTY_LIST: u8 = b']';
/// Make an object of the class at the last mark with the arguments above
/// it.
pub const OBJ: u8 = b'o';
/// Store the value on top of the stack in the memo. The index is 1 byte.
pub const BINPUT: u8 = b'q';
/// Store the value on top of the stack in the memo. The index is 4 bytes,
/// little-endian.
pub const LONG_BINPUT: u8 = b'r';
/// Set the keys and values above the last mark in the dict below the mark.
pub const SETITEMS: u8 = b'u';
/// Push an empty tuple.
pub const EMPTY_TUPLE: u8 = b')';
/// Push an empty dict.
pub const EMPTY_DICT: u8 = b'}';

// Protocol 2.
/// Give the protocol of the pickle: 1 byte. It comes first.
pub const PROTO: u8 = 0x80;
/// Pop a class and a tuple of arguments. Push `cls.__new__(cls, *args)`.
pub const NEWOBJ: u8 = 0x81;
/// Push an object of the extension registry. The code is 1 byte.
pub const EXT1: u8 = 0x82;
/// Push an object of the extension registry. The code is 2 bytes,
/// little-endian.
pub const EXT2: u8 = 0x83;
/// Push an object of the extension registry. The code is 4 bytes,
/// little-endian.
pub const EXT4: u8 = 0x84;
/// Make a tuple of the value on top of the stack.
pub const TUPLE1: u8 = 0x85;
/// Make a tuple of the two values on top of the stack.
pub const TUPLE2: u8 = 0x86;
/// Make a tuple of the three values on top of the stack.
pub const TUPLE3: u8 = 0x87;
/// Push `True`.
pub const NEWTRUE: u8 = 0x88;
/// Push `False`.
pub const NEWFALSE: u8 = 0x89;
/// Push an int: a length of 1 byte, then the two's complement bytes,
/// little-endian.
pub const LONG1: u8 = 0x8a;
/// Push an int: a length of 4 bytes, then the two's complement bytes,
/// little-endian.
pub const LONG4: u8 = 0x8b;

// Protocol 3.
/// Push a bytes: a length of 4 bytes, then the bytes.
pub const BINBYTES: u8 = b'B';
/// Push a bytes: a length of 1 byte, then the bytes.
pub const SHORT_BINBYTES: u8 = b'C';

/// Returns the memo indices that the opcodes in `bytes` put, in order.
///
/// `bytes` is a sequence of whole opcodes, as an opaque node keeps them.
/// The scan stops at an opcode that it cannot read.
pub fn memo_puts(bytes: &[u8]) -> Vec<u32> {
    let mut puts = Vec::new();
    let mut pos = 0;
    while let Some(&opcode) = bytes.get(pos) {
        let rest = &bytes[pos + 1..];
        let Some(len) = argument_len(opcode, rest) else {
            break;
        };
        if matches!(opcode, BINPUT | LONG_BINPUT | PUT) {
            puts.extend(memo_index(opcode, &rest[..len]));
        }
        pos += 1 + len;
    }
    puts
}

/// Returns the memo index of a put or a get, read from `argument`, the
/// argument of the opcode as [`argument_len`] gives its length.
///
/// BINPUT and BINGET take 1 byte. LONG_BINPUT and LONG_BINGET take 4
/// bytes, little-endian. PUT and GET take decimal text and a newline, with
/// no sign and no leading zero. Returns `None` for another opcode, or for
/// an argument that does not read.
pub fn memo_index(opcode: u8, argument: &[u8]) -> Option<u32> {
    match opcode {
        BINPUT | BINGET => match argument {
            [byte] => Some(u32::from(*byte)),
            _ => None,
        },
        LONG_BINPUT | LONG_BINGET => argument.try_into().ok().map(u32::from_le_bytes),
        PUT | GET => match argument.split_last()? {
            (b'\n', digits) => crate::text::index(digits),
            _ => None,
        },
        _ => None,
    }
}

/// Returns the length of the argument of an opcode, read from `rest`, the
/// bytes after the opcode. Returns `None` if the bytes end too early or if
/// the decoder does not know the opcode. PROTO and STOP are not in the
/// list: they come only at the start and at the end of a pickle.
pub fn argument_len(opcode: u8, rest: &[u8]) -> Option<usize> {
    let line = |rest: &[u8]| rest.iter().position(|byte| *byte == b'\n').map(|n| n + 1);
    let len = match opcode {
        BININT1 | BINPUT | BINGET | EXT1 => 1,
        BININT2 | EXT2 => 2,
        BININT | LONG_BINPUT | LONG_BINGET | EXT4 => 4,
        BINFLOAT => 8,
        LONG1 | SHORT_BINBYTES | SHORT_BINSTRING => 1 + usize::from(*rest.first()?),
        LONG4 | BINBYTES | BINSTRING | BINUNICODE => {
            let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
            4 + usize::try_from(len).ok()?
        }
        INT | LONG | FLOAT | STRING | UNICODE | PERSID | GET | PUT => line(rest)?,
        GLOBAL | INST => {
            let first = line(rest)?;
            first + line(&rest[first..])?
        }
        MARK | POP | POP_MARK | DUP | NONE | REDUCE | APPEND | BUILD | DICT | LIST | SETITEM
        | TUPLE | BINPERSID | APPENDS | EMPTY_LIST | OBJ | SETITEMS | EMPTY_TUPLE | EMPTY_DICT
        | NEWOBJ | TUPLE1 | TUPLE2 | TUPLE3 | NEWTRUE | NEWFALSE => 0,
        _ => return None,
    };
    (len <= rest.len()).then_some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_puts_reads_every_put_form_and_skips_arguments() {
        let bytes = b"cm\nq\nq\x01X\x01\x00\x00\x00qr\x02\x01\x00\x00Vq\np300\nK\x71\x85q\x07";
        assert_eq!(memo_puts(bytes), [1, 258, 300, 7]);
    }

    #[test]
    fn memo_puts_stops_at_bytes_it_cannot_read() {
        assert_eq!(memo_puts(b"q\x01\xffq\x02"), [1]);
        assert_eq!(memo_puts(b"q\x01C\x05ab"), [1]);
    }

    #[test]
    fn memo_index_reads_each_form_of_put_and_get() {
        assert_eq!(memo_index(BINPUT, b"\x07"), Some(7));
        assert_eq!(memo_index(BINGET, b"\xff"), Some(255));
        assert_eq!(memo_index(LONG_BINPUT, b"\x02\x01\x00\x00"), Some(258));
        assert_eq!(memo_index(LONG_BINGET, b"\xff\xff\xff\xff"), Some(u32::MAX));
        assert_eq!(memo_index(PUT, b"300\n"), Some(300));
        assert_eq!(memo_index(GET, b"0\n"), Some(0));
    }

    #[test]
    fn memo_index_refuses_an_argument_that_does_not_read() {
        for (opcode, argument) in [
            (PUT, &b"+3\n"[..]),
            (PUT, b"03\n"),
            (GET, b"-1\n"),
            (GET, b"3"),
            (PUT, b"\n"),
            (PUT, b"4294967296\n"),
            (BINPUT, b""),
            (LONG_BINGET, b"\x01\x00"),
            (MARK, b""),
        ] {
            assert_eq!(memo_index(opcode, argument), None, "{argument:?}");
        }
    }
}
