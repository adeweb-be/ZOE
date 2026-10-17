//! Fuzz tier 1 of the conflict resolution: `zoe_core::merge::tier1`.
//!
//! An input is three records and a flag: a byte whose two low bits give
//! the mode of the posting-dict rule (0 off, 1 on, else shadow), then the
//! lengths of the old and of the committed record, each as 2
//! bytes big-endian, then the old, the committed and the new record. A
//! panic fails the run, and so does a merged record that does not decode,
//! and an outcome in shadow mode that differs from the outcome with the
//! rule off.

#![no_main]

use libfuzzer_sys::fuzz_target;

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
    let posting = match flags & 3 {
        0 => zoe_core::Mode::Off,
        1 => zoe_core::Mode::On,
        _ => zoe_core::Mode::Shadow,
    };
    let merge = zoe_core::merge::tier1(old, committed, new, posting);
    if posting == zoe_core::Mode::Shadow {
        let off = zoe_core::merge::tier1(old, committed, new, zoe_core::Mode::Off);
        assert_eq!(
            merge.as_ref().map(|merge| &merge.outcome),
            off.as_ref().map(|merge| &merge.outcome),
            "shadow mode ends a conflict as the rule off does"
        );
    }
    if let Some(merge) = merge
        && let zoe_core::merge::Outcome::Merged(record) = merge.outcome
    {
        zoe_pickle::decode_record(&record).expect("a merged record decodes");
    }
});
