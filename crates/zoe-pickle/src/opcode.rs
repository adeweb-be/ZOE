//! The pickle opcodes that the decoder and the encoder know.

pub const MARK: u8 = b'(';
pub const STOP: u8 = b'.';
pub const PROTO: u8 = 0x80;

pub const NONE: u8 = b'N';
pub const NEWTRUE: u8 = 0x88;
pub const NEWFALSE: u8 = 0x89;

pub const BININT1: u8 = b'K';
pub const BININT2: u8 = b'M';
pub const BININT: u8 = b'J';
pub const LONG1: u8 = 0x8a;
pub const LONG4: u8 = 0x8b;
pub const BINFLOAT: u8 = b'G';

pub const SHORT_BINBYTES: u8 = b'C';
pub const BINBYTES: u8 = b'B';
pub const SHORT_BINSTRING: u8 = b'U';
pub const BINSTRING: u8 = b'T';
pub const BINUNICODE: u8 = b'X';

pub const EMPTY_TUPLE: u8 = b')';
pub const TUPLE1: u8 = 0x85;
pub const TUPLE2: u8 = 0x86;
pub const TUPLE3: u8 = 0x87;
pub const TUPLE: u8 = b't';

pub const EMPTY_LIST: u8 = b']';
pub const APPEND: u8 = b'a';
pub const APPENDS: u8 = b'e';

pub const EMPTY_DICT: u8 = b'}';
pub const SETITEM: u8 = b's';
pub const SETITEMS: u8 = b'u';

pub const GLOBAL: u8 = b'c';
pub const BINPERSID: u8 = b'Q';
pub const REDUCE: u8 = b'R';
pub const NEWOBJ: u8 = 0x81;
pub const BUILD: u8 = b'b';
pub const EXT1: u8 = 0x82;
pub const EXT2: u8 = 0x83;
pub const EXT4: u8 = 0x84;

pub const BINPUT: u8 = b'q';
pub const LONG_BINPUT: u8 = b'r';
pub const BINGET: u8 = b'h';
pub const LONG_BINGET: u8 = b'j';
