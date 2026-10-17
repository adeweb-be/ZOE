//! The KV namespaces of a store (ADR 0043).
//!
//! A namespace holds keys and values beside the object graph. The
//! ephemeral flavour gives each key a TTL. The durable flavour keeps a key
//! until a delete. Both keep one row for each key: no history, and pack
//! never reads them.
//!
//! A write joins the two-phase commit of a transaction: `Txn::kv_set` and
//! `Txn::kv_delete` stage it, and the leader of the commit batch writes it
//! with the records of the transaction, in the same backend write. A write
//! takes no lock, and the vote checks nothing for it: the commit with the
//! later tid wins. A read gives the last committed value of a key, not the
//! value at a snapshot. An expired key reads as absent.
//!
//! An expired key goes without a pack. A sweep step (`Store::sweep`) takes
//! the keys of the index of the expiries whose time has come, and removes
//! each one with its row, in a backend write of its own. The skins run the
//! steps: the core starts no thread.

use std::ops::Bound;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Result, ZoeError};
use crate::format::{self, FrameError, KeySpace};
use crate::store::Store;
use crate::types::Tid;

/// The most pairs that one scan gives.
pub const SCAN_LIMIT: usize = 1000;

/// The most keys of the index of the expiries that one sweep step takes.
pub const SWEEP_LIMIT: usize = 1000;

/// The time between two sweep steps of a store, when a step takes fewer
/// than `SWEEP_LIMIT` keys.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The longest TTL, in seconds: the longest timeout of a WebDAV lock.
pub const MAX_TTL_SECONDS: u64 = u32::MAX as u64;

/// The flavour of a namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Flavour {
    /// Every key has a TTL. Sessions and locks use it.
    Ephemeral,
    /// A key stays until a delete. The registry uses it.
    Durable,
}

impl Flavour {
    /// Return the name of the flavour in the protocol and in the Python API:
    /// `ephemeral` or `durable`.
    pub fn name(self) -> &'static str {
        match self {
            Flavour::Ephemeral => "ephemeral",
            Flavour::Durable => "durable",
        }
    }

    /// Return the flavour of `name`, or `None` for another name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "ephemeral" => Some(Flavour::Ephemeral),
            "durable" => Some(Flavour::Durable),
            _ => None,
        }
    }

    /// Return the key space of the rows of the flavour.
    pub(crate) fn space(self) -> KeySpace {
        match self {
            Flavour::Ephemeral => KeySpace::KvEphemeral,
            Flavour::Durable => KeySpace::KvDurable,
        }
    }
}

/// Return the time `time` in milliseconds since the Unix epoch. A time
/// before the epoch gives 0.
pub(crate) fn millis(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// Check the name of a namespace: 1 to 255 bytes. A `&str` is UTF-8.
pub(crate) fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(ZoeError::InvalidKv("the name of a namespace is empty"));
    }
    if name.len() > format::KV_MAX_NAME_LEN {
        return Err(ZoeError::InvalidKv(
            "the name of a namespace is longer than 255 bytes",
        ));
    }
    Ok(())
}

/// Check a key of a namespace: 1 to 1,024 bytes.
pub(crate) fn check_key(key: &[u8]) -> Result<()> {
    if key.is_empty() {
        return Err(ZoeError::InvalidKv("a key is empty"));
    }
    if key.len() > format::KV_MAX_KEY_LEN {
        return Err(ZoeError::InvalidKv("a key is longer than 1,024 bytes"));
    }
    Ok(())
}

/// Return the TTL of a write of `flavour` in milliseconds, or 0 for a
/// durable write. An ephemeral write needs a TTL of more than 0 and at
/// most `MAX_TTL_SECONDS`. A durable write takes none. A TTL below one
/// millisecond counts as one millisecond.
pub(crate) fn ttl_millis(flavour: Flavour, ttl: Option<Duration>) -> Result<u64> {
    match (flavour, ttl) {
        (Flavour::Durable, None) => Ok(0),
        (Flavour::Durable, Some(_)) => Err(ZoeError::InvalidKv("a durable key takes no TTL")),
        (Flavour::Ephemeral, None) => Err(ZoeError::InvalidKv("an ephemeral key needs a TTL")),
        (Flavour::Ephemeral, Some(ttl)) => {
            if ttl.is_zero() {
                return Err(ZoeError::InvalidKv("a TTL is not more than 0"));
            }
            if ttl > Duration::from_secs(MAX_TTL_SECONDS) {
                return Err(ZoeError::InvalidKv("a TTL is longer than 2^32 - 1 seconds"));
            }
            Ok(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX).max(1))
        }
    }
}

/// The corruption error of the row `row` of `flavour`. It names the
/// namespace and the length of the key, never the key: a key can be a
/// session id.
pub(crate) fn corrupt(flavour: Flavour, row: &[u8]) -> ZoeError {
    let (name, key_len) = match format::split_kv_key(row) {
        Some((name, key)) => (String::from_utf8_lossy(name).into_owned(), key.len()),
        None => (String::new(), row.len()),
    };
    ZoeError::CorruptKv {
        flavour,
        name,
        key_len,
    }
}

/// Return the error of the frame of the row `row` of `flavour` that
/// `decode_frame` refused. A frame of a newer format is a format error.
fn refused(flavour: Flavour, row: &[u8], error: FrameError) -> ZoeError {
    match error {
        FrameError::Unsupported { offset, value } => ZoeError::Format(format!(
            "a row of the {} namespaces: header byte {offset} is {value:#04x}, which this build cannot read",
            flavour.name()
        )),
        _ => corrupt(flavour, row),
    }
}

/// A row of a namespace, read and checked.
struct Row {
    /// The expiry of an ephemeral row, in milliseconds since the Unix
    /// epoch. A durable row has none.
    expiry: Option<u64>,
    value: Vec<u8>,
}

/// Decode and check the stored bytes of the row `row` of `flavour`. The
/// checksum comes first, so a flipped bit in an expiry is a corruption
/// error, never an expired key.
fn decode_row(flavour: Flavour, row: &[u8], bytes: &[u8]) -> Result<Row> {
    let frame =
        format::decode_frame(flavour.space(), row, bytes).map_err(|e| refused(flavour, row, e))?;
    match flavour {
        Flavour::Ephemeral => {
            let (expiry, value) =
                format::decode_kv_ephemeral(&frame.data).map_err(|_| corrupt(flavour, row))?;
            Ok(Row {
                expiry: Some(expiry),
                value: value.to_vec(),
            })
        }
        Flavour::Durable => Ok(Row {
            expiry: None,
            value: frame.data.into_owned(),
        }),
    }
}

impl Row {
    /// Tell if the row is expired at `now`, in milliseconds.
    fn expired(&self, now: u64) -> bool {
        self.expiry.is_some_and(|expiry| expiry <= now)
    }
}

/// What one sweep step did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// The keys of the index of the expiries that the step took and
    /// removed. A step that takes `SWEEP_LIMIT` keys can leave more.
    pub taken: usize,
    /// The ephemeral rows that the step removed.
    pub removed: usize,
}

/// A durable row, as a dump or a copy carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableRow {
    /// The name of the namespace.
    pub name: String,
    /// The key. The empty key is the generation row of the namespace.
    pub key: Vec<u8>,
    /// The payload: the value, or the tid of a generation row in 8
    /// little-endian bytes.
    pub payload: Vec<u8>,
}

/// A problem that `Store::verify_kv` found in a row of a namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvFinding {
    /// The flavour of the row.
    pub flavour: Flavour,
    /// The name of the namespace, when the key of the row gives one.
    pub namespace: Option<String>,
    /// The xxh3-64 hash of the key of the row, never the key.
    pub key_hash: Option<u64>,
    /// What is wrong.
    pub problem: String,
}

/// What `Store::verify_kv` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KvCheck {
    /// The keys of the ephemeral namespaces, expired keys included.
    pub ephemeral_keys: u64,
    /// The keys of the durable namespaces, without the generation rows.
    pub durable_keys: u64,
    /// The ephemeral keys that are expired and wait for a sweep step.
    pub expired_keys: u64,
    /// The problems.
    pub findings: Vec<KvFinding>,
}

/// The rows that one backend read of a scan gives.
const SCAN_CHUNK: usize = 256;

impl Store {
    /// Return the value of `key` in the namespace `name` of `flavour`, or
    /// `None` when the key has no row or is expired.
    ///
    /// The read gives the last committed value, not the value at a
    /// snapshot. A row that fails its checksum raises `CorruptKv`. A bad
    /// name or key raises `InvalidKv`.
    pub fn kv_get(&self, flavour: Flavour, name: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        check_name(name)?;
        check_key(key)?;
        let row = format::kv_key(name.as_bytes(), key);
        let Some(bytes) = self.backend.kv_row(flavour, &row)? else {
            return Ok(None);
        };
        let decoded = decode_row(flavour, &row, &bytes)?;
        if decoded.expired(millis(self.clock.now())) {
            return Ok(None);
        }
        Ok(Some(decoded.value))
    }

    /// Return the keys and the values of the namespace `name` of
    /// `flavour`, in byte order of the keys, from `start` (included) to
    /// `stop` (excluded). `None` is an open end. The scan gives at most
    /// `limit` pairs, and at most `SCAN_LIMIT`. It skips expired keys and
    /// the generation row of a durable namespace.
    ///
    /// A `limit` of 0 raises `InvalidKv`.
    pub fn kv_scan(
        &self,
        flavour: Flavour,
        name: &str,
        start: Option<&[u8]>,
        stop: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        check_name(name)?;
        if limit == 0 {
            return Err(ZoeError::InvalidKv("the limit of a scan is 0"));
        }
        let limit = limit.min(SCAN_LIMIT);
        let name = name.as_bytes();
        let now = millis(self.clock.now());
        // The generation row has the empty key, so a scan from the empty
        // key starts after it.
        let mut from = Bound::Excluded(format::kv_key(name, b""));
        if let Some(start) = start.filter(|start| !start.is_empty()) {
            from = Bound::Included(format::kv_key(name, start));
        }
        // The rows of the namespace end before the key of the next name
        // length or name. That key is the name with its last byte raised,
        // or the next length.
        let end = match stop {
            Some(stop) => Bound::Excluded(format::kv_key(name, stop)),
            None => namespace_end(name),
        };
        let mut pairs = Vec::new();
        while pairs.len() < limit {
            let rows = self
                .backend
                .kv_rows(flavour, slice(&from), slice(&end), SCAN_CHUNK)?;
            let Some((last, _)) = rows.last() else {
                break;
            };
            from = Bound::Excluded(last.clone());
            let full = rows.len() == SCAN_CHUNK;
            for (row, bytes) in rows {
                let decoded = decode_row(flavour, &row, &bytes)?;
                if decoded.expired(now) {
                    continue;
                }
                let key = row[1 + name.len()..].to_vec();
                pairs.push((key, decoded.value));
                if pairs.len() == limit {
                    break;
                }
            }
            if !full {
                break;
            }
        }
        Ok(pairs)
    }

    /// Return the generation of the durable namespace `name`: the tid of
    /// the last commit that wrote it, or `Tid::ZERO` before the first one.
    /// A client cache of the namespace stays valid while its generation
    /// stays the same.
    pub fn kv_generation(&self, name: &str) -> Result<Tid> {
        check_name(name)?;
        let row = format::kv_key(name.as_bytes(), b"");
        let Some(bytes) = self.backend.kv_row(Flavour::Durable, &row)? else {
            return Ok(Tid::ZERO);
        };
        let frame = format::decode_frame(KeySpace::KvDurable, &row, &bytes)
            .map_err(|e| refused(Flavour::Durable, &row, e))?;
        format::decode_kv_generation(&frame.data).map_err(|_| corrupt(Flavour::Durable, &row))
    }

    /// Tell if the store has the table of the ephemeral namespaces: a
    /// commit wrote an ephemeral key once. Only such a store needs sweep
    /// steps.
    pub fn has_ephemeral_namespaces(&self) -> Result<bool> {
        self.backend.has_kv(Flavour::Ephemeral)
    }

    /// Run one sweep step: take the keys of the index of the expiries
    /// whose expiry is at or before now, at most `limit`, in key order,
    /// and remove them in one backend write of its own, with a sync.
    ///
    /// In the same write, the step removes the row of each key when the row
    /// passes its checksum and its own expiry is at or before now. So a row
    /// that a later write refreshed stays, and a damaged index key never
    /// removes a live key. A row that fails its checksum stays, as
    /// evidence.
    ///
    /// The step runs outside the commit sequencer, as a sequence
    /// reservation does. A read-only store fails with `ReadOnly`, and a
    /// halted store with `Halted`.
    pub fn sweep(&self, limit: usize) -> Result<Swept> {
        self.writable()?;
        if self.lock_sequencer().is_none() {
            return Err(ZoeError::Halted);
        }
        if !self.backend.has_kv(Flavour::Ephemeral)? {
            return Ok(Swept::default());
        }
        let now = millis(self.clock.now());
        let mut write = self.backend.begin_write()?;
        let swept = write.sweep_kv(now, limit)?;
        if swept.taken == 0 {
            // An empty step writes nothing.
            return Ok(swept);
        }
        write.commit()?;
        Ok(swept)
    }

    /// Return the durable rows from the row key `after` (excluded), at most
    /// `limit`, in key order, with the generation rows. `None` starts at the
    /// first row. A dump and a copy of a store carry these rows. Each row
    /// passes its checksum, else the call raises `CorruptKv`.
    pub fn kv_durable_rows(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<DurableRow>, Option<Vec<u8>>)> {
        let from = match after {
            Some(after) => Bound::Excluded(after),
            None => Bound::Unbounded,
        };
        let rows = self
            .backend
            .kv_rows(Flavour::Durable, from, Bound::Unbounded, limit)?;
        let next = rows.last().map(|(row, _)| row.clone());
        let mut out = Vec::with_capacity(rows.len());
        for (row, bytes) in rows {
            let frame = format::decode_frame(KeySpace::KvDurable, &row, &bytes)
                .map_err(|e| refused(Flavour::Durable, &row, e))?;
            let (name, key) =
                format::split_kv_key(&row).ok_or_else(|| corrupt(Flavour::Durable, &row))?;
            let name = std::str::from_utf8(name)
                .map_err(|_| corrupt(Flavour::Durable, &row))?
                .to_owned();
            out.push(DurableRow {
                name,
                key: key.to_vec(),
                payload: frame.data.into_owned(),
            });
        }
        Ok((out, next))
    }

    /// Write `rows`, durable rows of a dump or of a copy, generation rows
    /// included, in one backend write of its own, with a sync. A row of a
    /// key that has one already replaces it. A read-only store fails with
    /// `ReadOnly`.
    pub fn kv_restore_durable(&self, rows: &[DurableRow]) -> Result<()> {
        self.writable()?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut write = self.backend.begin_write()?;
        for row in rows {
            check_name(&row.name)?;
            if row.key.len() > format::KV_MAX_KEY_LEN {
                return Err(ZoeError::InvalidKv("a key is longer than 1,024 bytes"));
            }
            let key = format::kv_key(row.name.as_bytes(), &row.key);
            write.put_kv(Flavour::Durable, &key, &row.payload)?;
        }
        write.commit()
    }

    /// Check every row of the namespaces, for `zoe verify`.
    ///
    /// A finding names the namespace and the xxh3-64 hash of the key,
    /// never the key. The findings are: a frame that fails its checksum, a
    /// key whose name length runs past the key, an empty name or a name
    /// that is not UTF-8, an ephemeral payload shorter than 8 bytes, an
    /// ephemeral row without its key in the index of the expiries, a
    /// durable namespace with rows and no generation row, and a generation
    /// after the last tid. An index key without its row, or with another
    /// expiry, is no finding: writes leave such keys. A scan that fails
    /// gives one finding and ends the check of its flavour.
    pub fn verify_kv(&self) -> Result<KvCheck> {
        let mut check = KvCheck::default();
        let now = millis(self.clock.now());
        let last = self.last_transaction();
        for flavour in [Flavour::Ephemeral, Flavour::Durable] {
            if !self.backend.has_kv(flavour)? {
                continue;
            }
            // The namespaces with keys, and those with a generation row.
            let mut with_keys = std::collections::BTreeSet::new();
            let mut with_generation = std::collections::BTreeSet::new();
            let mut from: Bound<Vec<u8>> = Bound::Unbounded;
            loop {
                let rows =
                    match self
                        .backend
                        .kv_rows(flavour, slice(&from), Bound::Unbounded, SCAN_CHUNK)
                    {
                        Ok(rows) => rows,
                        Err(error) => {
                            check.findings.push(KvFinding {
                                flavour,
                                namespace: None,
                                key_hash: None,
                                problem: format!("the scan of the rows failed: {error}"),
                            });
                            break;
                        }
                    };
                let Some((last_row, _)) = rows.last() else {
                    break;
                };
                from = Bound::Excluded(last_row.clone());
                let full = rows.len() == SCAN_CHUNK;
                for (row, bytes) in rows {
                    let finding = |namespace: Option<&str>, problem: &str| KvFinding {
                        flavour,
                        namespace: namespace.map(str::to_owned),
                        key_hash: Some(xxh3_64(&row)),
                        problem: problem.to_owned(),
                    };
                    let Some((name, key)) = format::split_kv_key(&row) else {
                        check.findings.push(finding(
                            None,
                            "the name length of the key is 0 or runs past the key",
                        ));
                        continue;
                    };
                    let Ok(name) = std::str::from_utf8(name) else {
                        check
                            .findings
                            .push(finding(None, "the name of the namespace is not UTF-8"));
                        continue;
                    };
                    let frame = match format::decode_frame(flavour.space(), &row, &bytes) {
                        Ok(frame) => frame,
                        Err(_) => {
                            check
                                .findings
                                .push(finding(Some(name), "the frame fails its checksum"));
                            continue;
                        }
                    };
                    match flavour {
                        Flavour::Ephemeral => {
                            check.ephemeral_keys += 1;
                            let Ok((expiry, _)) = format::decode_kv_ephemeral(&frame.data) else {
                                check.findings.push(finding(
                                    Some(name),
                                    "the payload is shorter than its expiry",
                                ));
                                continue;
                            };
                            if expiry <= now {
                                check.expired_keys += 1;
                            }
                            let index = format::kv_expiry_key(expiry, &row);
                            if !self.backend.kv_expiry_has(&index)? {
                                check.findings.push(finding(
                                    Some(name),
                                    "the index of the expiries has no key for the row",
                                ));
                            }
                        }
                        Flavour::Durable if key.is_empty() => {
                            with_generation.insert(name.to_owned());
                            match format::decode_kv_generation(&frame.data) {
                                Ok(generation) if generation > last => {
                                    check.findings.push(finding(
                                        Some(name),
                                        "the generation is after the last tid",
                                    ));
                                }
                                Ok(_) => {}
                                Err(_) => check
                                    .findings
                                    .push(finding(Some(name), "the generation row is not 8 bytes")),
                            }
                        }
                        Flavour::Durable => {
                            check.durable_keys += 1;
                            with_keys.insert(name.to_owned());
                        }
                    }
                }
                if !full {
                    break;
                }
            }
            for name in with_keys.difference(&with_generation) {
                check.findings.push(KvFinding {
                    flavour,
                    namespace: Some(name.clone()),
                    key_hash: None,
                    problem: "the namespace has keys and no generation row".to_owned(),
                });
            }
        }
        Ok(check)
    }
}

/// Return `bound` as a bound of a slice.
fn slice(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    bound.as_ref().map(Vec::as_slice)
}

/// Return the end of the rows of the namespace `name`: the first row key
/// after every key of `name`. The keys of a namespace start with its
/// length and its name, so the end is the same prefix with its last byte
/// raised by one. A name whose bytes are all 0xff ends at the next length.
fn namespace_end(name: &[u8]) -> Bound<Vec<u8>> {
    let mut prefix = format::kv_key(name, b"");
    while let Some(last) = prefix.pop() {
        if last < u8::MAX {
            prefix.push(last + 1);
            return Bound::Excluded(prefix);
        }
    }
    Bound::Unbounded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flavour_has_a_name() {
        for flavour in [Flavour::Ephemeral, Flavour::Durable] {
            assert_eq!(Flavour::from_name(flavour.name()), Some(flavour));
        }
        assert_eq!(Flavour::from_name("Durable"), None);
    }

    #[test]
    fn a_ttl_is_more_than_0_and_at_most_2_to_the_32_minus_1_seconds() {
        let ephemeral = |ttl| ttl_millis(Flavour::Ephemeral, ttl);
        assert_eq!(ephemeral(Some(Duration::from_secs(1))).unwrap(), 1000);
        assert_eq!(ephemeral(Some(Duration::from_micros(1))).unwrap(), 1);
        let longest = Duration::from_secs(MAX_TTL_SECONDS);
        assert_eq!(ephemeral(Some(longest)).unwrap(), MAX_TTL_SECONDS * 1000);
        for ttl in [
            None,
            Some(Duration::ZERO),
            Some(longest + Duration::from_millis(1)),
        ] {
            assert!(
                matches!(ephemeral(ttl), Err(ZoeError::InvalidKv(_))),
                "{ttl:?}"
            );
        }
        assert_eq!(ttl_millis(Flavour::Durable, None).unwrap(), 0);
        assert!(ttl_millis(Flavour::Durable, Some(Duration::from_secs(1))).is_err());
    }

    #[test]
    fn names_and_keys_have_limits() {
        assert!(check_name("sessions").is_ok());
        assert!(check_name(&"n".repeat(255)).is_ok());
        assert!(check_name("").is_err());
        assert!(check_name(&"n".repeat(256)).is_err());
        assert!(check_key(b"k").is_ok());
        assert!(check_key(&[0; 1024]).is_ok());
        assert!(check_key(b"").is_err());
        assert!(check_key(&[0; 1025]).is_err());
    }

    #[test]
    fn the_end_of_a_namespace_is_after_each_of_its_keys() {
        let Bound::Excluded(end) = namespace_end(b"ab") else {
            panic!("an open end");
        };
        assert!(format::kv_key(b"ab", &[0xff; 1024]) < end);
        assert!(end <= format::kv_key(b"ac", b""));
        assert_eq!(
            namespace_end(&[0xff; 255]),
            Bound::Unbounded,
            "no row key comes after the last name"
        );
    }

    #[test]
    fn a_corruption_error_names_no_key() {
        let error = corrupt(Flavour::Ephemeral, &format::kv_key(b"sessions", b"secret"));
        let text = error.to_string();
        assert!(text.contains("sessions") && text.contains('6'), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }
}
