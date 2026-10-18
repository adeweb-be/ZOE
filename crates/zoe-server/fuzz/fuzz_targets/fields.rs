//! Fuzz tier 2 of the conflict resolution and the reader of the merge
//! policy: `zoe_core::merge::tier2`, `Policy::read` and `policy_oid`.
//!
//! An input is three records and a flag: a byte whose three low bits pick
//! the default strategy of the class (0 strict, 1 lww, 2 max, 3 sum, 4
//! union, 5 append, 6 keys, else strict) and whose bit 3 couples the
//! fields `a` and `b`, then the lengths of the old and of the committed record, each
//! as 2 bytes big-endian, then the old, the committed and the new record.
//! Each record also goes to the readers of the policy and of the root. A
//! panic fails the run, and so does a merged record that does not decode.

#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use zoe_core::Mode;
use zoe_core::merge::policy::{ClassPolicy, Policy, Strategy, policy_oid};

fuzz_target!(|data: &[u8]| {
    let [flags, a0, a1, b0, b1, rest @ ..] = data else {
        return;
    };
    let old_len = usize::from(u16::from_be_bytes([*a0, *a1]));
    let committed_len = usize::from(u16::from_be_bytes([*b0, *b1]));
    let Some((old, rest)) = rest.split_at_checked(old_len) else {
        return;
    };
    let Some((committed, new)) = rest.split_at_checked(committed_len) else {
        return;
    };
    let default = match flags & 7 {
        1 => Strategy::Lww,
        2 => Strategy::Max,
        3 => Strategy::Sum,
        4 => Strategy::Union,
        5 => Strategy::Append,
        6 => Strategy::Keys,
        _ => Strategy::Strict,
    };
    let coupled = if flags & 8 != 0 {
        vec![vec!["a".to_owned(), "b".to_owned()]]
    } else {
        Vec::new()
    };
    let entry = ClassPolicy {
        tier1: None,
        tier2: Mode::On,
        default,
        fields: BTreeMap::new(),
        coupled,
        blocked: false,
    };
    let class = zoe_core::merge::class_name(new).unwrap_or_else(|| "m.C".to_owned());
    let merge = zoe_core::merge::tier2(old, committed, new, &class, &entry);
    if let zoe_core::merge::Outcome::Merged(record) = merge.outcome {
        zoe_pickle::decode_record(&record).expect("a merged record decodes");
    }
    for record in [old, committed, new] {
        let _ = Policy::read(record);
        let _ = policy_oid(record);
    }
});
