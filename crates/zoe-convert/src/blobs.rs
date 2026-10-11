//! The blob directory of a FileStorage and its blob records.
//!
//! ZODB keeps the blob of `(oid, tid)` in one file. The layout of the
//! directory gives its path:
//!
//! - bushy: one directory for each of the 8 big-endian bytes of the oid,
//!   for example `0x00/0x00/0x00/0x00/0x00/0x00/0x00/0x0b/<tid>.blob`;
//! - lawn: one directory for the oid, for example `0x0b/<tid>.blob`.
//!
//! The oid of the lawn directory and the tid of the file name are written
//! as ZODB's `oid_repr` writes them.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use zoe_core::{Oid, Tid};

use crate::Error;

/// The file in a blob directory that names its layout.
pub(crate) const LAYOUT_FILE: &str = ".layout";

/// The layout of a blob directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    Bushy,
    Lawn,
}

/// The blob directory of a FileStorage.
#[derive(Clone, Debug)]
pub(crate) struct BlobDir {
    root: PathBuf,
    layout: Layout,
}

impl BlobDir {
    /// Open the blob directory `root` and find its layout.
    ///
    /// The `.layout` file names the layout. A directory without it gets
    /// the layout that ZODB selects: lawn when the directory holds an
    /// entry whose name does not start with a dot, bushy otherwise. Another
    /// name in `.layout` is refused with `Layout`.
    pub(crate) fn open(root: &Path) -> Result<Self, Error> {
        let io = |source| Error::Io {
            path: root.to_owned(),
            source,
        };
        let layout = match fs::read_to_string(root.join(LAYOUT_FILE)) {
            Ok(name) => match name.trim() {
                "bushy" => Layout::Bushy,
                "lawn" => Layout::Lawn,
                other => {
                    return Err(Error::Layout {
                        path: root.to_owned(),
                        layout: other.to_owned(),
                    });
                }
            },
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let mut layout = Layout::Bushy;
                for entry in fs::read_dir(root).map_err(io)? {
                    if !entry
                        .map_err(io)?
                        .file_name()
                        .as_encoded_bytes()
                        .starts_with(b".")
                    {
                        layout = Layout::Lawn;
                        break;
                    }
                }
                layout
            }
            Err(error) => return Err(io(error)),
        };
        Ok(Self {
            root: root.to_owned(),
            layout,
        })
    }

    /// Return the layout of the directory.
    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    /// Return the path of the blob file of `oid` at `tid`.
    pub(crate) fn path(&self, oid: Oid, tid: Tid) -> PathBuf {
        let mut path = self.root.clone();
        match self.layout {
            Layout::Bushy => {
                for byte in oid.0.to_be_bytes() {
                    path.push(format!("0x{byte:02x}"));
                }
            }
            Layout::Lawn => path.push(repr(oid.0)),
        }
        path.push(format!("{}.blob", repr(tid.0)));
        path
    }
}

/// Write an oid or a tid as ZODB's `oid_repr` does: `0x`, then the hex
/// digits without leading zeros, padded to an even count. Zero is `0x00`.
fn repr(n: u64) -> String {
    let digits = format!("{n:x}");
    if digits.len() % 2 == 1 {
        format!("0x0{digits}")
    } else {
        format!("0x{digits}")
    }
}

/// Tell if `data` is the record of a blob: its class pickle is the class
/// `ZODB.blob.Blob` itself.
///
/// This is the test of ZODB's `is_blob_record`. It reads only the class
/// pickle: an optional PROTO of protocol 0 to 3, the GLOBAL
/// `ZODB.blob Blob`, an optional memo put, then STOP. A record that only
/// refers to a blob, or a subclass of `Blob`, is not a blob record.
pub(crate) fn is_blob_record(data: &[u8]) -> bool {
    let rest = match data {
        [0x80, 0..=3, rest @ ..] => rest,
        [0x80, ..] => return false,
        rest => rest,
    };
    let Some(rest) = rest.strip_prefix(b"cZODB.blob\nBlob\n") else {
        return false;
    };
    let rest = match rest {
        // BINPUT and LONG_BINPUT.
        [b'q', _, rest @ ..] | [b'r', _, _, _, _, rest @ ..] => rest,
        // PUT: decimal digits and a newline.
        [b'p', rest @ ..] => {
            let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
            match rest[digits..] {
                [b'\n', ..] if digits > 0 => &rest[digits + 1..],
                _ => return false,
            }
        }
        rest => rest,
    };
    rest.first() == Some(&b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filestorage::tests::test_dir;

    /// The fixture directory.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data-fs")
    }

    #[test]
    fn the_paths_of_both_layouts_match_the_fixture() {
        let (oid, tid) = (Oid(0x0b), Tid(0x033f_2eec_6aaa_aaaa));
        for (dir, name) in [
            (
                "blobs-bushy",
                "0x00/0x00/0x00/0x00/0x00/0x00/0x00/0x0b/0x033f2eec6aaaaaaa.blob",
            ),
            ("blobs-lawn", "0x0b/0x033f2eec6aaaaaaa.blob"),
        ] {
            let root = fixture().join(dir);
            let blobs = BlobDir::open(&root).unwrap();
            assert_eq!(blobs.path(oid, tid), root.join(name));
            assert!(blobs.path(oid, tid).is_file());
        }
    }

    #[test]
    fn repr_strips_leading_zeros_to_an_even_count() {
        assert_eq!(repr(0), "0x00");
        assert_eq!(repr(0x0b), "0x0b");
        assert_eq!(repr(0x1ab), "0x01ab");
        assert_eq!(repr(u64::MAX), "0xffffffffffffffff");
    }

    #[test]
    fn a_directory_without_a_layout_file_gets_the_layout_of_zodb() {
        let dir = test_dir("layout-auto");
        std::fs::create_dir(dir.join(".hidden")).unwrap();
        let blobs = BlobDir::open(&dir).unwrap();
        assert_eq!(blobs.layout, Layout::Bushy);

        std::fs::create_dir(dir.join("0x0b")).unwrap();
        let blobs = BlobDir::open(&dir).unwrap();
        assert_eq!(blobs.layout, Layout::Lawn);
    }

    #[test]
    fn an_unknown_layout_is_refused() {
        let dir = test_dir("layout-unknown");
        std::fs::write(dir.join(LAYOUT_FILE), "zipped\n").unwrap();
        let error = BlobDir::open(&dir).unwrap_err();
        assert!(matches!(error, Error::Layout { .. }), "{error:?}");
    }

    #[test]
    fn a_missing_blob_directory_is_an_error() {
        let dir = test_dir("layout-missing").join("nothing");
        assert!(matches!(BlobDir::open(&dir), Err(Error::Io { .. })));
    }

    #[test]
    fn blob_records_are_found_in_each_protocol() {
        for record in [
            &b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N."[..],
            b"\x80\x02cZODB.blob\nBlob\nr\x00\x01\x00\x00.N.",
            b"cZODB.blob\nBlob\nq\x01.N.",
            b"cZODB.blob\nBlob\np0\n.N.",
            b"cZODB.blob\nBlob\n.N.",
        ] {
            assert!(is_blob_record(record), "{}", record.escape_ascii());
        }
    }

    #[test]
    fn other_records_are_not_blob_records() {
        for record in [
            // A record that refers to a blob in its state.
            &b"\x80\x03cexample.file\nFileObject\nq\x00.\x80\x03}q\x01X\x05\x00\x00\x00_blobq\x02C\x08\x00\x00\x00\x00\x00\x00\x00\x0bq\x03cZODB.blob\nBlob\nq\x04\x86q\x05Qs."[..],
            // The class with arguments.
            b"\x80\x03cZODB.blob\nBlob\nq\x00N\x86q\x01.N.",
            // Another class of the module.
            b"\x80\x03cZODB.blob\nBlobFile\nq\x00.N.",
            // A PUT without digits.
            b"cZODB.blob\nBlob\np\n.N.",
            // A protocol after 3.
            b"\x80\x04cZODB.blob\nBlob\nq\x00.\x80\x04N.",
            b"",
            b"cZODB.blob\nBlob\n",
        ] {
            assert!(!is_blob_record(record), "{}", record.escape_ascii());
        }
    }
}
