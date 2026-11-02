//! The text deltas: a scratch key space for a spike. Not merged.
//!
//! `text_deltas` holds one row for each version of the text of one
//! document in one text index of one catalog. Its key is `text_key`: the
//! oid of the catalog (8 bytes), the length of the index name (1 byte),
//! the index name (1 to 255 bytes of UTF-8), the rid (4 bytes, the sign
//! bit flipped, so the byte order is the order of the rids), then the tid
//! (8 bytes). All numbers are big-endian. The value is a frame with tag 9.
//! Its previous serial is the tid of the previous version of the same
//! text. Its payload is the length of the language (1 byte), the language
//! (ASCII), then the text (UTF-8). An empty payload marks an unindex.
//!
//! `text_txn` is the index of the deltas by tid. Its key is `text_txn_key`:
//! the tid, then the key of the row without its tid. Its value is empty,
//! so it has no frame, as `txn_oids`.

/// Return the key of the text of `rid` in `index` of `catalog` at `tid`.
pub fn text_key(catalog: u64, index: &str, rid: i32, tid: u64) -> Vec<u8> {
    let mut key = text_prefix(catalog, index, rid);
    key.extend_from_slice(&tid.to_be_bytes());
    key
}

/// Return the key of `text_deltas` without its tid.
pub fn text_prefix(catalog: u64, index: &str, rid: i32) -> Vec<u8> {
    let name = index.as_bytes();
    assert!(!name.is_empty() && name.len() <= 255, "an index name has 1 to 255 bytes");
    let mut key = Vec::with_capacity(8 + 1 + name.len() + 4 + 8);
    key.extend_from_slice(&catalog.to_be_bytes());
    key.push(name.len() as u8);
    key.extend_from_slice(name);
    key.extend_from_slice(&((rid as u32) ^ 0x8000_0000).to_be_bytes());
    key
}

/// Split the key of a row of `text_deltas` into the catalog, the index,
/// the rid and the tid.
pub fn split_text_key(key: &[u8]) -> Option<(u64, &str, i32, u64)> {
    let catalog = u64::from_be_bytes(key.get(..8)?.try_into().ok()?);
    let len = usize::from(*key.get(8)?);
    let name = std::str::from_utf8(key.get(9..9 + len)?).ok()?;
    let rest = key.get(9 + len..)?;
    if rest.len() != 12 {
        return None;
    }
    let rid = (u32::from_be_bytes(rest[..4].try_into().ok()?) ^ 0x8000_0000) as i32;
    let tid = u64::from_be_bytes(rest[4..].try_into().ok()?);
    Some((catalog, name, rid, tid))
}

/// Return the key of the row of `text_txn` for a row of `text_deltas`.
pub fn text_txn_key(tid: u64, catalog: u64, index: &str, rid: i32) -> Vec<u8> {
    let mut key = tid.to_be_bytes().to_vec();
    key.extend_from_slice(&text_prefix(catalog, index, rid));
    key
}

/// Encode the payload of a text delta.
pub fn encode_text(language: &str, text: &str) -> Vec<u8> {
    let lang = language.as_bytes();
    assert!(lang.len() <= 35, "a language tag has at most 35 bytes");
    let mut out = Vec::with_capacity(1 + lang.len() + text.len());
    out.push(lang.len() as u8);
    out.extend_from_slice(lang);
    out.extend_from_slice(text.as_bytes());
    out
}

/// Decode the payload of a text delta: `None` for an unindex.
pub fn decode_text(payload: &[u8]) -> Option<Option<(&str, &str)>> {
    if payload.is_empty() {
        return Some(None);
    }
    let len = usize::from(payload[0]);
    let lang = std::str::from_utf8(payload.get(1..1 + len)?).ok()?;
    let text = std::str::from_utf8(payload.get(1 + len..)?).ok()?;
    Some(Some((lang, text)))
}
