//! A `Data.fs` path that is not a regular file is refused before its open.
//!
//! The test makes a FIFO with the program `mkfifo`. Until the child
//! process starts that program, it holds a copy of each descriptor of the
//! test process, so it also holds each `flock` of the other tests. A test
//! that then takes such a lock again gets `Locked`. So this test has its
//! own process, and it opens its store only after the child ended.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;

use zoe_convert::{Error, from_datafs};
use zoe_core::Store;

mod support;
use support::fresh_dir;

#[test]
fn a_path_that_is_not_a_regular_file_is_refused() {
    let dir = fresh_dir("not-regular");
    let fifo = dir.join("fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    let store = Store::open(dir.join("store")).unwrap();
    // The open of a FIFO without a writer waits forever, so a hang here
    // means that the check is missing.
    for path in [fifo, dir.to_path_buf(), PathBuf::from("/dev/null")] {
        let error = from_datafs(&path, None, &store).err().unwrap();
        let Error::Io { source, .. } = &error else {
            panic!("not an I/O error: {error:?}");
        };
        assert_eq!(source.kind(), ErrorKind::InvalidInput, "{error:?}");
    }
    assert_eq!(store.last_transaction(), zoe_core::Tid::ZERO);
}
