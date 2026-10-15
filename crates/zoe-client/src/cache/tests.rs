use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;

const KB: u64 = 1024;

fn record(data: &[u8], tid: u64, end: Option<u64>) -> Record {
    Record {
        data: data.to_vec(),
        tid: Tid(tid),
        end: end.map(Tid),
    }
}

/// A cache of `size` bytes whose `applied` is `tid`.
fn cache_at(size: u64, tid: u64) -> Cache {
    let cache = Cache::new(size).unwrap();
    cache.apply(Tid(tid), &[]);
    cache
}

#[test]
fn an_empty_cache_misses() {
    let cache = cache_at(64 * KB, 10);
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load_before(Oid(1), Tid(11)), None);
    assert_eq!(cache.applied(), Tid(10));
}

#[test]
fn a_size_rounds_down_to_a_word_and_needs_one() {
    assert_eq!(Cache::new(100).unwrap().size(), 96);
    assert!(Cache::new(7).is_err());
}

#[test]
fn a_current_entry_serves_the_snapshots_up_to_applied() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    // `before` is one more than the tid of the snapshot.
    assert_eq!(
        cache.load_before(Oid(1), Tid(11)),
        Some(record(b"five", 5, None))
    );
    assert_eq!(
        cache.load_before(Oid(1), Tid(6)),
        Some(record(b"five", 5, None))
    );
    // A later snapshot gets the newest revision that the cache knows, as
    // from the cache of ZEO.
    assert_eq!(
        cache.load_before(Oid(1), Tid(12)),
        Some(record(b"five", 5, None))
    );
    // The revision did not exist before its start.
    assert_eq!(cache.load_before(Oid(1), Tid(5)), None);
    // `load` serves the current entry, as `ZEO.ClientStorage.load` does.
    assert_eq!(cache.load(Oid(1)), Some((b"five".to_vec(), Tid(5))));
}

/// The `maxtid` of ZODB, which `ZODB.utils.load_current` gives to
/// `loadBefore`.
const MAXTID: Tid = Tid(0x7fff_ffff_ffff_ffff);

#[test]
fn a_current_entry_serves_load_before_at_maxtid() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    // The cache has no server: a hit is the whole answer.
    for before in [Tid(12), Tid(1000), MAXTID, Tid::MAX] {
        assert_eq!(
            cache.load_before(Oid(1), before),
            Some(record(b"five", 5, None)),
            "{before:?}"
        );
    }
    // After a commit of the process, the new current entry serves it.
    cache.commit(Tid(12), &[(Oid(1), Some(b"twelve".as_slice()))]);
    assert_eq!(
        cache.load_before(Oid(1), MAXTID),
        Some(record(b"twelve", 12, None))
    );
}

#[test]
fn an_entry_that_is_not_current_never_serves_load_before_at_maxtid() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"six", Tid(10)));
    assert!(cache.store(Oid(3), Tid(3), Some(Tid(7)), b"three", Tid(10)));
    // A commit of another client ends the entry of oid 1.
    cache.apply(Tid(12), &[Oid(1)]);
    assert_eq!(cache.load_before(Oid(1), MAXTID), None);
    assert_eq!(cache.load_before(Oid(1), Tid(13)), None);
    // A conflict drops the entry of oid 2.
    cache.drop_current(Oid(2));
    assert_eq!(cache.load_before(Oid(2), MAXTID), None);
    // An entry with an end serves only the snapshots before its end.
    assert_eq!(cache.load_before(Oid(3), MAXTID), None);
    // A current reply that the commit 12 made stale never goes in.
    assert!(!cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert_eq!(cache.load_before(Oid(1), MAXTID), None);
}

#[test]
fn after_a_skipped_commit_a_current_entry_serves_only_up_to_applied() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"six", Tid(10)));
    // The pool delivers its commit 15 without the commits in (10, 15) of
    // the other clients: the cache cannot tell if they changed oid 1.
    cache.skip(Tid(15), &[Oid(2)]);
    assert_eq!(cache.applied(), Tid(10));
    assert_eq!(cache.load_before(Oid(1), MAXTID), None);
    assert_eq!(cache.load_before(Oid(1), Tid(16)), None);
    assert_eq!(cache.load_before(Oid(1), Tid(12)), None);
    assert_eq!(cache.load_before(Oid(2), MAXTID), None);
    // The snapshots up to `applied` stay true.
    assert_eq!(
        cache.load_before(Oid(1), Tid(11)),
        Some(record(b"five", 5, None))
    );
    // A verification at 15 or later moves `applied` past the skipped tid.
    cache.verify(Tid(15), Some(&[]));
    assert_eq!(
        cache.load_before(Oid(1), MAXTID),
        Some(record(b"five", 5, None))
    );
}

#[test]
fn a_commit_of_another_client_ends_the_current_entry() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"six", Tid(10)));
    cache.apply(Tid(12), &[Oid(1)]);
    assert_eq!(cache.applied(), Tid(12));
    // The ended entry still serves the snapshots before its end.
    assert_eq!(
        cache.load_before(Oid(1), Tid(12)),
        Some(record(b"five", 5, Some(12)))
    );
    assert_eq!(cache.load_before(Oid(1), Tid(13)), None);
    assert_eq!(cache.load(Oid(1)), None);
    // The other oid stays current, now up to 12.
    assert_eq!(
        cache.load_before(Oid(2), Tid(13)),
        Some(record(b"six", 6, None))
    );
}

#[test]
fn the_race_of_zeo_issue_166_refuses_a_late_current_reply() {
    let cache = cache_at(64 * KB, 10);
    // A connection at the position 10 carries a reply: oid 7 is current
    // from tid 5. The reply waits.
    let (data, start, seen) = (b"five", Tid(5), Tid(10));
    // The dispatcher applies tid 12, which changed oid 7.
    cache.apply(Tid(12), &[Oid(7)]);
    // The reply is stored now, with the position of its arrival: refused.
    assert!(!cache.store(Oid(7), start, None, data, seen));
    assert_eq!(cache.counts().refused, 1);
    assert_eq!(cache.load_before(Oid(7), Tid(13)), None);
    assert_eq!(cache.load(Oid(7)), None);
    // A reply that arrived after the connection read tid 12 goes in.
    assert!(cache.store(Oid(7), Tid(12), None, b"twelve", Tid(12)));
    assert_eq!(
        cache.load_before(Oid(7), Tid(13)),
        Some(record(b"twelve", 12, None))
    );
}

#[test]
fn a_record_with_an_end_always_goes_in() {
    let cache = cache_at(64 * KB, 20);
    assert!(cache.store(Oid(1), Tid(5), Some(Tid(8)), b"five", Tid(3)));
    assert_eq!(
        cache.load_before(Oid(1), Tid(8)),
        Some(record(b"five", 5, Some(8)))
    );
    assert_eq!(cache.load_before(Oid(1), Tid(9)), None);
    assert_eq!(cache.load(Oid(1)), None);
    // A second store of the same revision changes nothing.
    assert!(!cache.store(Oid(1), Tid(5), Some(Tid(8)), b"five", Tid(3)));
}

#[test]
fn several_revisions_of_an_oid_serve_their_snapshots() {
    let cache = cache_at(64 * KB, 20);
    assert!(cache.store(Oid(1), Tid(5), Some(Tid(8)), b"five", Tid(20)));
    assert!(cache.store(Oid(1), Tid(8), Some(Tid(15)), b"eight", Tid(20)));
    assert!(cache.store(Oid(1), Tid(15), None, b"fifteen", Tid(20)));
    assert_eq!(cache.load_before(Oid(1), Tid(6)).unwrap().data, b"five");
    assert_eq!(cache.load_before(Oid(1), Tid(8)).unwrap().data, b"five");
    assert_eq!(cache.load_before(Oid(1), Tid(9)).unwrap().data, b"eight");
    assert_eq!(cache.load_before(Oid(1), Tid(15)).unwrap().data, b"eight");
    assert_eq!(cache.load_before(Oid(1), Tid(21)).unwrap().data, b"fifteen");
    assert_eq!(cache.load_before(Oid(1), Tid(5)), None);
}

#[test]
fn a_newer_current_reply_drops_the_older_current_entry() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    // A connection that read up to 20 saw the revision 15. The revision of
    // 5 ended somewhere in (5, 15]: the cache drops it.
    assert!(cache.store(Oid(1), Tid(15), None, b"fifteen", Tid(20)));
    assert_eq!(cache.load_before(Oid(1), Tid(11)), None);
    assert_eq!(cache.load(Oid(1)), Some((b"fifteen".to_vec(), Tid(15))));
    // An older current reply does not replace a newer one.
    assert!(!cache.store(Oid(1), Tid(5), None, b"five", Tid(20)));
    // The revision 15 serves the snapshots from 15 once applied reaches 15.
    assert_eq!(cache.load_before(Oid(1), Tid(11)), None);
    cache.apply(Tid(15), &[Oid(1)]);
    assert_eq!(
        cache.load_before(Oid(1), Tid(16)),
        Some(record(b"fifteen", 15, None))
    );
}

#[test]
fn a_commit_of_the_process_ends_and_stores() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"six", Tid(10)));
    // The server resolved oid 2: the cache does not hold its pickle.
    cache.commit(
        Tid(12),
        &[(Oid(1), Some(b"twelve".as_slice())), (Oid(2), None)],
    );
    assert_eq!(cache.applied(), Tid(12));
    assert_eq!(cache.load(Oid(1)), Some((b"twelve".to_vec(), Tid(12))));
    assert_eq!(
        cache.load_before(Oid(1), Tid(13)),
        Some(record(b"twelve", 12, None))
    );
    assert_eq!(
        cache.load_before(Oid(1), Tid(12)),
        Some(record(b"five", 5, Some(12)))
    );
    assert_eq!(cache.load(Oid(2)), None);
    assert_eq!(
        cache.load_before(Oid(2), Tid(12)),
        Some(record(b"six", 6, Some(12)))
    );
}

#[test]
fn a_commit_keeps_the_entry_that_a_load_already_stored() {
    let cache = cache_at(64 * KB, 10);
    // A connection read the commit 12 before the turn of its finish.
    assert!(cache.store(Oid(1), Tid(12), None, b"twelve", Tid(12)));
    cache.commit(Tid(12), &[(Oid(1), Some(b"twelve".as_slice()))]);
    assert_eq!(
        cache.load_before(Oid(1), Tid(13)),
        Some(record(b"twelve", 12, None))
    );
}

#[test]
fn a_conflict_drops_the_current_entry_only() {
    let cache = cache_at(64 * KB, 20);
    assert!(cache.store(Oid(1), Tid(5), Some(Tid(8)), b"five", Tid(20)));
    assert!(cache.store(Oid(1), Tid(8), None, b"eight", Tid(20)));
    cache.drop_current(Oid(1));
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load_before(Oid(1), Tid(21)), None);
    assert_eq!(cache.load_before(Oid(1), Tid(8)).unwrap().data, b"five");
    // The next load stores the current revision again.
    assert!(cache.store(Oid(1), Tid(8), None, b"eight", Tid(20)));
    assert_eq!(cache.load(Oid(1)), Some((b"eight".to_vec(), Tid(8))));
}

#[test]
fn clear_drops_every_entry_and_keeps_applied() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(2), Tid(3), Some(Tid(4)), b"three", Tid(10)));
    cache.clear();
    assert_eq!(cache.applied(), Tid(10));
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load_before(Oid(2), Tid(4)), None);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert_eq!(cache.load(Oid(1)), Some((b"five".to_vec(), Tid(5))));
}

#[test]
fn the_entries_are_those_that_a_lookup_finds() {
    let cache = cache_at(64 * KB, 10);
    assert_eq!(cache.entries(), vec![]);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    assert!(cache.store(Oid(1), Tid(3), Some(Tid(5)), b"three", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"six", Tid(10)));
    let mut entries = cache.entries();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            (Oid(1), Tid(3), Some(Tid(5))),
            (Oid(1), Tid(5), None),
            (Oid(2), Tid(6), None),
        ]
    );
    // A dropped entry is not in them.
    cache.drop_current(Oid(2));
    assert!(!cache.entries().contains(&(Oid(2), Tid(6), None)));
    cache.clear();
    assert_eq!(cache.entries(), vec![]);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    cache.close();
    assert_eq!(cache.entries(), vec![]);
}

#[test]
fn a_verification_drops_the_current_entries_of_the_changed_oids() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"one", Tid(10)));
    assert!(cache.store(Oid(2), Tid(6), None, b"two", Tid(10)));
    assert!(cache.store(Oid(1), Tid(3), Some(Tid(5)), b"old", Tid(10)));
    cache.verify(Tid(20), Some(&[Oid(1)]));
    assert_eq!(cache.applied(), Tid(20));
    // The changed oid has no current entry. Its entry with an end stays.
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load_before(Oid(1), Tid(21)), None);
    assert_eq!(
        cache.load_before(Oid(1), Tid(4)),
        Some(record(b"old", 3, Some(5)))
    );
    // The other current entry now serves the snapshots up to 20.
    assert_eq!(
        cache.load_before(Oid(2), Tid(21)),
        Some(record(b"two", 6, None))
    );
    // A current reply from before the verification is refused.
    assert!(!cache.store(Oid(1), Tid(15), None, b"late", Tid(12)));
}

#[test]
fn a_verification_without_the_commits_drops_every_entry() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"one", Tid(10)));
    assert!(cache.store(Oid(2), Tid(3), Some(Tid(5)), b"old", Tid(10)));
    cache.verify(Tid(20), None);
    assert_eq!(cache.applied(), Tid(20));
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load_before(Oid(2), Tid(4)), None);
    assert!(cache.store(Oid(1), Tid(18), None, b"new", Tid(20)));
}

#[test]
fn a_disabled_cache_drops_every_entry_and_stores_nothing() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    cache.disable();
    assert_eq!(cache.load(Oid(1)), None);
    assert!(!cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    cache.commit(Tid(12), &[(Oid(2), Some(b"twelve".as_slice()))]);
    assert_eq!(cache.load(Oid(2)), None);
}

#[test]
fn a_closed_cache_serves_and_writes_nothing() {
    let cache = cache_at(64 * KB, 10);
    assert!(cache.store(Oid(1), Tid(5), None, b"five", Tid(10)));
    cache.close();
    assert!(!cache.verified());
    assert_eq!(cache.load(Oid(1)), None);
    assert!(!cache.store(Oid(2), Tid(5), None, b"five", Tid(10)));
    cache.apply(Tid(12), &[Oid(1)]);
    cache.verify(Tid(20), None);
    assert_eq!(cache.applied(), Tid(10));
    cache.close();
}

#[test]
fn an_entry_larger_than_half_of_the_ring_is_never_cached() {
    let cache = cache_at(100 * KB, 10);
    let large = vec![7u8; (50 * KB) as usize];
    let quarter = vec![8u8; (25 * KB) as usize];
    assert!(!cache.fits(large.len()));
    assert!(!cache.store(Oid(1), Tid(5), None, &large, Tid(10)));
    assert_eq!(cache.load(Oid(1)), None);
    cache.commit(Tid(11), &[(Oid(1), Some(large.as_slice()))]);
    assert_eq!(cache.load(Oid(1)), None);
    assert!(cache.store(Oid(2), Tid(5), None, &quarter, Tid(11)));
    assert_eq!(cache.load(Oid(2)), Some((quarter, Tid(5))));
}

#[test]
fn the_last_oid_is_never_cached() {
    let cache = cache_at(64 * KB, 10);
    assert!(!cache.store(Oid(u64::MAX), Tid(5), None, b"five", Tid(10)));
    cache.commit(Tid(12), &[(Oid(u64::MAX), Some(b"twelve".as_slice()))]);
    assert_eq!(cache.load(Oid(u64::MAX)), None);
    assert_eq!(cache.counts().stored, 0);
}

#[test]
fn an_entry_that_no_slot_takes_is_not_stored() {
    // 128 KB of ring has 1,024 slots, and holds 1,500 entries of 8 bytes.
    let cache = cache_at(128 * KB, 10);
    let mut refused = 0;
    for i in 0..1500 {
        if !cache.store(Oid(i), Tid(5), None, &i.to_le_bytes(), Tid(10)) {
            refused += 1;
            assert_eq!(cache.load(Oid(i)), None);
        }
    }
    let counts = cache.counts();
    assert!(refused >= 1500 - 1024, "{refused} refused");
    assert_eq!(counts.unindexed, refused);
    assert_eq!(counts.stored, 1500 - refused);
}

#[test]
fn the_data_of_any_length_comes_back() {
    let cache = cache_at(64 * KB, 100);
    for len in 0..40u64 {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
        assert!(cache.store(Oid(len), Tid(1 + len), None, &data, Tid(100)));
    }
    for len in 0..40u64 {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
        assert_eq!(cache.load(Oid(len)), Some((data, Tid(1 + len))));
    }
}

#[test]
fn a_large_entry_comes_back_in_blocks() {
    let cache = cache_at(1024 * KB, 100);
    // Lengths around the block of 4 KiB, and a record of 100 KB with a tail
    // that is not a whole word.
    let lengths = [4095, 4096, 4097, 8191, 8192, 12_289, 100 * 1024 + 3];
    let data = |len: usize| -> Vec<u8> { (0..len).map(|i| (i * 31 + len) as u8).collect() };
    for (i, len) in lengths.iter().enumerate() {
        assert!(cache.store(Oid(i as u64), Tid(5), None, &data(*len), Tid(100)));
    }
    for (i, len) in lengths.iter().enumerate() {
        let oid = Oid(i as u64);
        assert_eq!(cache.load(oid), Some((data(*len), Tid(5))));
        // The copy gives whole blocks of 4 KiB, then the rest, in order.
        let (blocks, _) = cache
            .load_with(oid, |total, copy| {
                let mut blocks = Vec::new();
                copy(&mut |block| blocks.push(block.to_vec())).then_some((total, blocks))
            })
            .map(|((total, blocks), start)| {
                assert_eq!((total, start), (*len, Tid(5)));
                (blocks, start)
            })
            .unwrap();
        let (last, whole) = blocks.split_last().unwrap();
        assert!(whole.iter().all(|block| block.len() == 4096));
        assert!(!last.is_empty() && last.len() <= 4096);
        assert_eq!(blocks.concat(), data(*len));
    }
}

#[test]
fn a_store_writes_the_data_its_xxh3_and_zeros_after_it() {
    let cache = cache_at(1024 * KB, 100);
    // Fill the whole ring with ones: the next entries take words that
    // held ones, so the zeros after their data come from the store.
    for oid in 1000..1020 {
        assert!(cache.store(Oid(oid), Tid(5), None, &[0xff; 64 * 1024], Tid(100)));
    }
    let lengths: [usize; 10] = [0, 1, 7, 8, 9, 4095, 4096, 4097, 8200, 100 * 1024 + 3];
    for (i, len) in lengths.into_iter().enumerate() {
        let data: Vec<u8> = (0..len).map(|k| (k * 13 + len) as u8 | 1).collect();
        let oid = Oid(i as u64);
        assert!(cache.store(oid, Tid(5), None, &data, Tid(100)));
        let entry = cache.chain(oid.0).next().unwrap();
        assert_eq!(entry.data_sum, xxh3_64(&data));
        let at = cache.at(entry.pos) + HEAD_WORDS;
        let stored: Vec<u8> = cache.map.words()[at..at + len.div_ceil(8)]
            .iter()
            .flat_map(|word| word.load(Ordering::Relaxed).to_le_bytes())
            .collect();
        assert_eq!(stored[..len], data[..]);
        assert!(stored[len..].iter().all(|byte| *byte == 0));
    }
}

#[test]
fn a_copy_that_the_ring_overtakes_fails() {
    let cache = Arc::new(cache_at(256 * KB, 10));
    let data = vec![9u8; 100 * 1024 + 5];
    assert!(cache.store(Oid(1), Tid(5), None, &data, Tid(10)));
    let found = cache.load_with(Oid(1), |_, copy| {
        // A writer overwrites the entry after the lookup found it, before
        // the copy.
        for i in 2..10 {
            cache.store(Oid(i), Tid(5), None, &data, Tid(10));
        }
        let mut copied = 0;
        let good = copy(&mut |block| copied += block.len());
        assert_eq!(copied, data.len());
        assert!(!good);
        good.then_some(())
    });
    assert_eq!(found, None);
}

#[test]
fn the_ring_holds_at_most_its_size() {
    let cache = cache_at(100 * KB, 10);
    let data = |i: u64| vec![i as u8; KB as usize];
    for i in 0..1000 {
        assert!(cache.store(Oid(i), Tid(5), None, &data(i), Tid(10)));
    }
    let held: Vec<u64> = (0..1000)
        .filter(|i| cache.load(Oid(*i)).is_some())
        .collect();
    // Each entry takes 1 KB and its head of 72 bytes.
    assert!(held.len() as u64 <= 100, "{} entries", held.len());
    assert!(held.len() >= 85, "{} entries", held.len());
    // The ring keeps the newest entries.
    assert_eq!(held.last(), Some(&999));
    for i in &held {
        assert_eq!(cache.load(Oid(*i)).unwrap().0, data(*i));
    }
}

#[test]
fn a_hit_in_the_oldest_fifth_moves_to_the_head() {
    let cache = cache_at(100 * KB, 10);
    for i in 0..90 {
        assert!(cache.store(Oid(i), Tid(5), None, &[i as u8; 1000], Tid(10)));
    }
    // Oid 0 is the oldest entry. Its hit copies it to the head.
    assert!(cache.load(Oid(0)).is_some());
    assert_eq!(cache.counts().copied, 1);
    // Oid 50 is not in the oldest fifth.
    assert!(cache.load(Oid(50)).is_some());
    assert_eq!(cache.counts().copied, 1);
    // The next stores overwrite oid 1 before oid 0.
    for i in 90..100 {
        assert!(cache.store(Oid(i), Tid(5), None, &[i as u8; 1000], Tid(10)));
    }
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load(Oid(0)), Some((vec![0; 1000], Tid(5))));
}

#[test]
fn many_oids_share_the_index() {
    // 1 MB of ring has 4,096 slots, and holds about 2,200 entries of 400
    // bytes. 20,000 oids take the slots in turn.
    let cache = cache_at(1024 * KB, 10);
    let data = |i: u64| vec![i as u8; 400];
    for i in 0..20_000 {
        cache.store(Oid(i), Tid(5), None, &data(i), Tid(10));
    }
    // The ring holds the newest oids, and the index finds each of them.
    for i in 19_000..20_000 {
        assert_eq!(cache.load(Oid(i)), Some((data(i), Tid(5))));
    }
    assert_eq!(cache.counts().unindexed, 0);
}

/// Readers check each hit against the history while a writer applies
/// commits of another client, stores current replies by the rule, and
/// wraps the ring many times.
#[test]
fn readers_never_get_a_stale_or_torn_hit() {
    const OIDS: u64 = 64;
    const COMMITS: u64 = 3000;
    let cache = Arc::new(cache_at(32 * KB, 1));
    // Each commit T writes the oid T % OIDS, with data that names T.
    let data = |tid: u64| -> Vec<u8> {
        let len = 8 + (tid % 200) as usize;
        (0..len).map(|i| (tid as usize + i) as u8).collect()
    };
    // The tid of the revision of `oid` that the snapshot `before - 1`
    // sees, or 0 for none.
    let revision = move |oid: u64, before: u64| -> u64 {
        (2..before).rev().find(|tid| tid % OIDS == oid).unwrap_or(0)
    };
    let done = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..4u64)
        .map(|seed| {
            let (cache, done, hits) = (cache.clone(), done.clone(), hits.clone());
            std::thread::spawn(move || {
                let mut n = seed;
                while !done.load(Ordering::Relaxed) {
                    n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let oid = (n >> 33) % OIDS;
                    let applied = cache.applied().0;
                    let before = applied + 1 - ((n >> 20) % 50).min(applied - 1);
                    if let Some(found) = cache.load_before(Oid(oid), Tid(before)) {
                        let tid = revision(oid, before);
                        assert_eq!(found.tid, Tid(tid), "oid {oid} before {before}");
                        assert_eq!(found.data, data(tid));
                        if let Some(end) = found.end {
                            assert!(end.0 >= before);
                        }
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                    // A current entry at `maxtid` is never older than the
                    // revision at `applied` before the lookup. A commit
                    // stores its entry before it moves `applied`, so the
                    // entry can be one tid after `applied` after the lookup.
                    let low = cache.applied().0;
                    let found = cache.load_before(Oid(oid), MAXTID);
                    let high = cache.applied().0;
                    if let Some(found) = found {
                        assert!(found.end.is_none(), "oid {oid}");
                        assert!(found.tid.0 >= revision(oid, low + 1), "oid {oid} at {low}");
                        assert!(
                            found.tid.0 <= revision(oid, high + 2),
                            "oid {oid} at {high}"
                        );
                        assert_eq!(found.data, data(found.tid.0));
                        hits.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for tid in 2..COMMITS {
        let oid = tid % OIDS;
        // A load reply of a connection at the position tid - 1: the
        // current revision of another oid.
        let other = (tid * 7) % OIDS;
        let start = revision(other, tid);
        if start != 0 {
            cache.store(Oid(other), Tid(start), None, &data(start), Tid(tid - 1));
        }
        if tid % 3 == 0 {
            cache.apply(Tid(tid), &[Oid(oid)]);
        } else {
            cache.commit(Tid(tid), &[(Oid(oid), Some(data(tid).as_slice()))]);
        }
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    assert!(hits.load(Ordering::Relaxed) > 0);
}
