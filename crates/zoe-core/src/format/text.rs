//! The keys and the payloads of the text deltas (ADR 0060, section 1).
//! This module is a red zone.
//!
//! `text_deltas` holds the deltas. The key of a text delta is the oid of
//! the catalog (8 bytes), the length of the index id (1 byte, 1 to 255),
//! the index id (UTF-8), the rid (4 bytes, the sign bit flipped, so the
//! byte order is the order of the rids), then the tid (8 bytes). The key of
//! a clear delta is the oid of the catalog, the byte 0, the length of the
//! index id, the index id, then the tid. Numbers are big-endian. No index
//! id has the length 0, so the byte 0 tells a clear delta from a text
//! delta, and the clear deltas of a catalog sort before its text deltas.
//!
//! The value is a frame with the tag 9. Its previous serial is the tid of
//! the previous delta of the same key without its tid, or 0. The payload
//! of a text delta is the length of the language (1 byte), the language (0
//! to 35 bytes of ASCII), then the text (UTF-8). An unindex and a clear
//! delta have an empty payload.
//!
//! `text_txn` is the index of the deltas by tid: its key is the tid, then
//! the key of the delta without its tid. Its value is empty, so it has no
//! frame and no tag.

use crate::types::{Oid, Tid};

/// The most bytes of an index id.
pub const MAX_INDEX_LEN: usize = 255;

/// The most bytes of a language.
pub const MAX_LANGUAGE_LEN: usize = 35;

/// The length of the tid at the end of a key of `text_deltas`.
const TID_LEN: usize = 8;

/// The length of the rid in a key of a text delta.
const RID_LEN: usize = 4;

/// Return the 4 bytes of `rid` in a key: big-endian, the sign bit flipped.
fn rid_bytes(rid: i32) -> [u8; RID_LEN] {
    ((rid as u32) ^ 0x8000_0000).to_be_bytes()
}

/// Return the key of a text delta of `rid` in `index` of `catalog`,
/// without its tid. `index` has 1 to 255 bytes.
pub fn text_prefix(catalog: Oid, index: &str, rid: i32) -> Vec<u8> {
    let name = index.as_bytes();
    debug_assert!((1..=MAX_INDEX_LEN).contains(&name.len()));
    let mut key = Vec::with_capacity(8 + 1 + name.len() + RID_LEN + TID_LEN);
    key.extend_from_slice(&catalog.0.to_be_bytes());
    key.push(name.len() as u8);
    key.extend_from_slice(name);
    key.extend_from_slice(&rid_bytes(rid));
    key
}

/// Return the key of a clear delta of `index` of `catalog`, without its
/// tid. `index` has 1 to 255 bytes.
pub fn clear_prefix(catalog: Oid, index: &str) -> Vec<u8> {
    let name = index.as_bytes();
    debug_assert!((1..=MAX_INDEX_LEN).contains(&name.len()));
    let mut key = Vec::with_capacity(8 + 2 + name.len() + TID_LEN);
    key.extend_from_slice(&catalog.0.to_be_bytes());
    key.push(0);
    key.push(name.len() as u8);
    key.extend_from_slice(name);
    key
}

/// Return the key of `text_deltas` of the delta `prefix` at `tid`.
pub fn delta_key(prefix: &[u8], tid: Tid) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + TID_LEN);
    key.extend_from_slice(prefix);
    key.extend_from_slice(&tid.0.to_be_bytes());
    key
}

/// The parts of a key of `text_deltas`, or of a key without its tid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaKey<'a> {
    /// The oid of the catalog.
    pub catalog: Oid,
    /// The index id.
    pub index: &'a str,
    /// The rid of a text delta. A clear delta has none.
    pub rid: Option<i32>,
}

/// Read a key of `text_deltas` without its tid. A key that breaks the
/// rules of the format gives `None`: an index id of length 0, a length
/// that runs past the key, an index id that is not UTF-8, or bytes after
/// the rid.
pub fn split_prefix(prefix: &[u8]) -> Option<DeltaKey<'_>> {
    let catalog = Oid(u64::from_be_bytes(prefix.get(..8)?.try_into().ok()?));
    let (clear, at) = match *prefix.get(8)? {
        0 => (true, 9),
        _ => (false, 8),
    };
    let len = usize::from(*prefix.get(at)?);
    if len == 0 {
        return None;
    }
    let name = prefix.get(at + 1..at + 1 + len)?;
    let index = std::str::from_utf8(name).ok()?;
    let rest = &prefix[at + 1 + len..];
    let rid = if clear {
        if !rest.is_empty() {
            return None;
        }
        None
    } else {
        let bytes: [u8; RID_LEN] = rest.try_into().ok()?;
        Some((u32::from_be_bytes(bytes) ^ 0x8000_0000) as i32)
    };
    Some(DeltaKey {
        catalog,
        index,
        rid,
    })
}

/// Read a key of `text_deltas`: its parts, its prefix and its tid. A key
/// shorter than its tid, or whose prefix `split_prefix` refuses, gives
/// `None`.
pub fn split_key(key: &[u8]) -> Option<(DeltaKey<'_>, &[u8], Tid)> {
    let at = key.len().checked_sub(TID_LEN)?;
    let (prefix, tid) = key.split_at(at);
    let tid = Tid(u64::from_be_bytes(tid.try_into().ok()?));
    Some((split_prefix(prefix)?, prefix, tid))
}

/// Return the tid at the end of a key of `text_deltas`, or `None` for a
/// key shorter than a tid.
pub fn key_tid(key: &[u8]) -> Option<Tid> {
    let at = key.len().checked_sub(TID_LEN)?;
    Some(Tid(u64::from_be_bytes(key[at..].try_into().ok()?)))
}

/// Return the key of `text_txn` of the delta `prefix` at `tid`: the tid,
/// then the prefix.
pub fn txn_key(tid: Tid, prefix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(TID_LEN + prefix.len());
    key.extend_from_slice(&tid.0.to_be_bytes());
    key.extend_from_slice(prefix);
    key
}

/// Return the key of `text_txn` of the key `key` of `text_deltas`, or
/// `None` for a key shorter than a tid.
pub fn txn_key_of(key: &[u8]) -> Option<Vec<u8>> {
    let at = key.len().checked_sub(TID_LEN)?;
    let tid = key_tid(key)?;
    Some(txn_key(tid, &key[..at]))
}

/// Read a key of `text_txn`: its tid and the key of its delta without its
/// tid. A key shorter than a tid gives `None`.
pub fn split_txn_key(key: &[u8]) -> Option<(Tid, &[u8])> {
    let tid = Tid(u64::from_be_bytes(key.get(..TID_LEN)?.try_into().ok()?));
    Some((tid, &key[TID_LEN..]))
}

/// Encode the payload of a text delta: the length of `language`, the
/// language, then `text`. `language` has at most 35 bytes.
pub fn encode_payload(language: &str, text: &str) -> Vec<u8> {
    let lang = language.as_bytes();
    debug_assert!(lang.len() <= MAX_LANGUAGE_LEN);
    let mut out = Vec::with_capacity(1 + lang.len() + text.len());
    out.push(lang.len() as u8);
    out.extend_from_slice(lang);
    out.extend_from_slice(text.as_bytes());
    out
}

/// Decode the payload of a text delta. An empty payload is an unindex and
/// gives `None`. A language longer than 35 bytes or not ASCII, a length
/// that runs past the payload, and a text that is not UTF-8 give `Err`.
pub fn decode_payload(payload: &[u8]) -> Result<Option<(&str, &str)>, ()> {
    let Some((&len, rest)) = payload.split_first() else {
        return Ok(None);
    };
    let len = usize::from(len);
    if len > MAX_LANGUAGE_LEN || len > rest.len() {
        return Err(());
    }
    let (lang, text) = rest.split_at(len);
    if !lang.is_ascii() {
        return Err(());
    }
    let lang = std::str::from_utf8(lang).map_err(drop)?;
    let text = std::str::from_utf8(text).map_err(drop)?;
    Ok(Some((lang, text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_of_a_text_delta_reads_back() {
        for rid in [i32::MIN, -1, 0, 1, i32::MAX] {
            let prefix = text_prefix(Oid(7), "SearchableText", rid);
            let key = delta_key(&prefix, Tid(9));
            let (parts, read, tid) = split_key(&key).unwrap();
            assert_eq!(parts.catalog, Oid(7));
            assert_eq!(parts.index, "SearchableText");
            assert_eq!(parts.rid, Some(rid));
            assert_eq!(read, prefix.as_slice());
            assert_eq!(tid, Tid(9));
        }
    }

    #[test]
    fn the_byte_order_of_the_keys_is_the_order_of_the_rids() {
        let keys: Vec<Vec<u8>> = [i32::MIN, -5, -1, 0, 1, 5, i32::MAX]
            .into_iter()
            .map(|rid| text_prefix(Oid(1), "a", rid))
            .collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_clear_delta_sorts_before_the_text_deltas_of_its_catalog() {
        let clear = delta_key(&clear_prefix(Oid(1), "zzz"), Tid(5));
        let text = delta_key(&text_prefix(Oid(1), "a", i32::MIN), Tid(1));
        assert!(clear < text);
        let (parts, _, tid) = split_key(&clear).unwrap();
        assert_eq!(parts.rid, None);
        assert_eq!(parts.index, "zzz");
        assert_eq!(tid, Tid(5));
    }

    #[test]
    fn a_key_that_breaks_the_format_does_not_read() {
        let mut key = text_prefix(Oid(1), "a", 3);
        key.push(0);
        assert_eq!(split_prefix(&key), None);
        assert_eq!(split_prefix(&[0; 9]), None);
        assert_eq!(split_prefix(&[0, 0, 0, 0, 0, 0, 0, 1, 2, b'a']), None);
        let mut bad = text_prefix(Oid(1), "a", 3);
        bad[9] = 0xff;
        assert_eq!(split_prefix(&bad), None);
        assert_eq!(split_key(&[1, 2, 3]), None);
    }

    #[test]
    fn a_key_of_text_txn_reads_back() {
        let prefix = text_prefix(Oid(1), "Title", -4);
        let key = delta_key(&prefix, Tid(77));
        let index = txn_key_of(&key).unwrap();
        assert_eq!(index, txn_key(Tid(77), &prefix));
        assert_eq!(split_txn_key(&index), Some((Tid(77), prefix.as_slice())));
    }

    #[test]
    fn a_payload_reads_back_and_an_empty_one_is_an_unindex() {
        let payload = encode_payload("en", "some text");
        assert_eq!(decode_payload(&payload), Ok(Some(("en", "some text"))));
        assert_eq!(decode_payload(&encode_payload("", "")), Ok(Some(("", ""))));
        assert_eq!(decode_payload(&[]), Ok(None));
        assert_eq!(decode_payload(&[3, b'e', b'n']), Err(()));
        assert_eq!(decode_payload(&[36; 40]), Err(()));
        assert_eq!(decode_payload(&[1, 0xc3, b'x']), Err(()));
        assert_eq!(decode_payload(&[0, 0xff]), Err(()));
    }
}
