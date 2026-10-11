//! The pickle opcodes that the decoder and the encoder know: those of
//! protocols 0 to 3. The decoder refuses the opcodes of protocols 4 and 5.

/// The highest PROTO value that Python accepts.
pub const HIGHEST_PROTOCOL: u8 = 5;

// Protocol 0.
pub const MARK: u8 = b'(';
pub const STOP: u8 = b'.';
pub const POP: u8 = b'0';
pub const POP_MARK: u8 = b'1';
pub const DUP: u8 = b'2';
pub const FLOAT: u8 = b'F';
pub const INT: u8 = b'I';
pub const LONG: u8 = b'L';
pub const NONE: u8 = b'N';
pub const PERSID: u8 = b'P';
pub const REDUCE: u8 = b'R';
pub const STRING: u8 = b'S';
pub const UNICODE: u8 = b'V';
pub const APPEND: u8 = b'a';
pub const BUILD: u8 = b'b';
pub const GLOBAL: u8 = b'c';
pub const DICT: u8 = b'd';
pub const GET: u8 = b'g';
pub const INST: u8 = b'i';
pub const LIST: u8 = b'l';
pub const PUT: u8 = b'p';
pub const SETITEM: u8 = b's';
pub const TUPLE: u8 = b't';

// Protocol 1.
pub const BINFLOAT: u8 = b'G';
pub const BININT: u8 = b'J';
pub const BININT1: u8 = b'K';
pub const BININT2: u8 = b'M';
pub const BINPERSID: u8 = b'Q';
pub const BINSTRING: u8 = b'T';
pub const SHORT_BINSTRING: u8 = b'U';
pub const BINUNICODE: u8 = b'X';
pub const APPENDS: u8 = b'e';
pub const BINGET: u8 = b'h';
pub const LONG_BINGET: u8 = b'j';
pub const EMPTY_LIST: u8 = b']';
pub const OBJ: u8 = b'o';
pub const BINPUT: u8 = b'q';
pub const LONG_BINPUT: u8 = b'r';
pub const SETITEMS: u8 = b'u';
pub const EMPTY_TUPLE: u8 = b')';
pub const EMPTY_DICT: u8 = b'}';

// Protocol 2.
pub const PROTO: u8 = 0x80;
pub const NEWOBJ: u8 = 0x81;
pub const EXT1: u8 = 0x82;
pub const EXT2: u8 = 0x83;
pub const EXT4: u8 = 0x84;
pub const TUPLE1: u8 = 0x85;
pub const TUPLE2: u8 = 0x86;
pub const TUPLE3: u8 = 0x87;
pub const NEWTRUE: u8 = 0x88;
pub const NEWFALSE: u8 = 0x89;
pub const LONG1: u8 = 0x8a;
pub const LONG4: u8 = 0x8b;

// Protocol 3.
pub const BINBYTES: u8 = b'B';
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
        let argument = &rest[..len];
        let index = match opcode {
            BINPUT => Some(u32::from(argument[0])),
            LONG_BINPUT => argument.try_into().ok().map(u32::from_le_bytes),
            PUT => std::str::from_utf8(&argument[..len - 1])
                .ok()
                .and_then(|text| text.parse().ok()),
            _ => None,
        };
        puts.extend(index);
        pos += 1 + len;
    }
    puts
}

/// Returns the length of the argument of an opcode, read from `rest`, the
/// bytes after the opcode. Returns `None` if the bytes end too early or if
/// the decoder does not know the opcode. PROTO and STOP are not in the
/// list: they come only at the start and at the end of a pickle.
pub(crate) fn argument_len(opcode: u8, rest: &[u8]) -> Option<usize> {
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
}
