//! The fixture of the zoe-loupe tests: a small site with the shapes of a
//! Plone site, which `fixture/generate.py` wrote with ZODB. This module
//! reads `fixture/records.txt` and writes its transactions to a new store
//! with their tids.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zoe_core::testing::TempDir;
use zoe_core::{Oid, Store, Tid};
use zoe_loupe::Folder;

/// The variable that names the directory where the end-to-end tests of
/// the window want the fixture store.
pub const E2E_STORE: &str = "ZOE_LOUPE_FIXTURE";

/// One transaction of the fixture.
pub struct Txn {
    pub tid: Tid,
    pub user: Vec<u8>,
    pub description: Vec<u8>,
    pub extension: Vec<u8>,
    /// The records in the order of the file.
    pub records: Vec<(Oid, Vec<u8>)>,
    /// Where each path leads after the transaction: the oid and the class
    /// of the object, or `None` for no object.
    pub paths: Vec<(String, Option<(Oid, String)>)>,
}

/// The transactions of the fixture, in commit order.
pub struct Fixture {
    pub txns: Vec<Txn>,
}

fn number(field: &str) -> u64 {
    u64::from_str_radix(field, 16).unwrap()
}

fn bytes(field: &str) -> Vec<u8> {
    let hex = field.strip_prefix('x').unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// Read `fixture/records.txt`.
pub fn fixture() -> Fixture {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixture/records.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut txns: Vec<Txn> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[..] {
            ["txn", tid, user, description, extension] => txns.push(Txn {
                tid: Tid(number(tid)),
                user: bytes(user),
                description: bytes(description),
                extension: bytes(extension),
                records: Vec::new(),
                paths: Vec::new(),
            }),
            ["rec", oid, data] => txns
                .last_mut()
                .unwrap()
                .records
                .push((Oid(number(oid)), bytes(data))),
            ["path", path, "-"] => txns.last_mut().unwrap().paths.push((path.to_owned(), None)),
            ["path", path, oid, class] => txns
                .last_mut()
                .unwrap()
                .paths
                .push((path.to_owned(), Some((Oid(number(oid)), class.to_owned())))),
            _ => panic!("unknown line in records.txt: {line}"),
        }
    }
    Fixture { txns }
}

impl Fixture {
    /// The tid of the last transaction.
    pub fn last_tid(&self) -> Tid {
        self.txns.last().unwrap().tid
    }

    /// The transaction with the tid `tid`.
    pub fn txn(&self, tid: Tid) -> &Txn {
        self.txns.iter().find(|txn| txn.tid == tid).unwrap()
    }

    /// The oid and the class that `path` leads to after the last
    /// transaction. Panics if it leads to no object.
    pub fn at_path(&self, path: &str) -> (Oid, String) {
        self.path_at(path, self.last_tid())
            .unwrap_or_else(|| panic!("{path} leads to no object"))
    }

    /// The oid and the class that `path` leads to as of `tid`.
    pub fn path_at(&self, path: &str, tid: Tid) -> Option<(Oid, String)> {
        let txn = self.txns.iter().rev().find(|txn| txn.tid <= tid)?;
        let (_, target) = txn.paths.iter().find(|(p, _)| p == path).unwrap();
        target.clone()
    }

    /// The newest revision of `oid` as of `tid`: its tid and its record.
    pub fn revision(&self, oid: Oid, tid: Tid) -> Option<(Tid, &[u8])> {
        self.txns
            .iter()
            .filter(|txn| txn.tid <= tid)
            .flat_map(|txn| {
                txn.records
                    .iter()
                    .filter(move |(o, _)| *o == oid)
                    .map(move |(_, data)| (txn.tid, data.as_slice()))
            })
            .last()
    }

    /// Every revision of `oid`, oldest first: its tid and its record.
    pub fn revisions(&self, oid: Oid) -> Vec<(Tid, &[u8])> {
        self.txns
            .iter()
            .flat_map(|txn| {
                txn.records
                    .iter()
                    .filter(move |(o, _)| *o == oid)
                    .map(move |(_, data)| (txn.tid, data.as_slice()))
            })
            .collect()
    }

    /// The tid of the first record of each oid.
    pub fn created(&self) -> BTreeMap<Oid, Tid> {
        let mut created = BTreeMap::new();
        for txn in &self.txns {
            for (oid, _) in &txn.records {
                created.entry(*oid).or_insert(txn.tid);
            }
        }
        created
    }
}

/// The module and the name of the class of a record, from its class
/// pickle. Every record of the fixture names its class with GLOBAL.
pub fn class_name(data: &[u8]) -> String {
    let start = data.iter().position(|&b| b == b'c').unwrap() + 1;
    let mut lines = data[start..].splitn(3, |&b| b == b'\n');
    let module = std::str::from_utf8(lines.next().unwrap()).unwrap();
    let name = std::str::from_utf8(lines.next().unwrap()).unwrap();
    format!("{module}.{name}")
}

/// Return a new, empty directory for the test `name` of this test binary.
/// The drop of the directory removes it, except when the test panics.
pub fn fresh_dir(name: &str) -> TempDir {
    zoe_core::testing::fresh_dir(&format!("loupe-{name}"))
}

/// Open the fixture store in a new directory. The folder needs the
/// directory, so keep the directory while the folder is open.
pub fn open(name: &str) -> (TempDir, Fixture, Folder) {
    let dir = fresh_dir(name);
    let fixture = write_store(&dir);
    let folder = Folder::open(&dir).unwrap();
    (dir, fixture, folder)
}

/// Write the transactions of the fixture to a new store in `dir`, with
/// their tids, and close the store. Return the fixture.
pub fn write_store(dir: &Path) -> Fixture {
    let fixture = fixture();
    let store = Store::open(dir).unwrap();
    for txn in &fixture.txns {
        let mut copy = store
            .tpc_begin_restore(txn.tid, &txn.user, &txn.description, &txn.extension)
            .unwrap();
        for (oid, data) in &txn.records {
            copy.restore(*oid, Some(data.as_slice())).unwrap();
        }
        copy.vote().unwrap();
        assert_eq!(copy.finish().unwrap(), txn.tid);
    }
    fixture
}

/// The bytes of every file under `dir`, by path.
pub fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
        } else {
            files.insert(path.clone(), std::fs::read(&path).unwrap());
        }
    }
    files
}
