//! The tests of the blob store.

use super::*;
use crate::clock::SystemClock;
use crate::format::blob::tid_repr;
use crate::guard::SizeLimit;
use crate::store::{Options, Store};
use crate::vfs::{CrashImage, MemVfs, Vfs};

const PICKLE: &[u8] = b"pickle";

/// Return a new blob file of `store` that holds `bytes`.
fn blob_file(store: &Store, bytes: &[u8]) -> BlobFile {
    let mut file = store.new_blob_file().unwrap();
    file.write_all(bytes).unwrap();
    file
}

/// Commit a blob that holds `bytes` for each oid of `oids`.
fn commit_blobs(store: &Store, oids: &[Oid], bytes: &[u8]) -> Tid {
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for &oid in oids {
        txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(store, bytes))
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap()
}

fn read(store: &Store, oid: Oid, tid: Tid) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    store.open_blob(oid, tid).unwrap().read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Return the names of the files in `blobs/tmp/`.
fn temp_files(vfs: &MemVfs) -> Vec<String> {
    vfs.names()
        .into_iter()
        .filter(|name| name.starts_with("blobs/tmp/"))
        .collect()
}

#[test]
fn one_commit_holds_several_blobs() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oids = [store.new_oid().unwrap(), store.new_oid().unwrap()];
    let tid = commit_blobs(&store, &oids, b"two");
    for oid in oids {
        assert_eq!(read(&store, oid, tid).unwrap(), b"two");
    }
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_store_without_blobs_has_no_blob_directory() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let oid = store.new_oid().unwrap();
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert!(!Vfs::exists(&vfs, BLOB_DIR).unwrap());
    assert!(matches!(
        store.open_blob(oid, tid),
        Err(ZoeError::NotFound { .. })
    ));
    assert!(matches!(
        store.blob_path(oid, tid),
        Err(ZoeError::NotFound { .. })
    ));
}

#[test]
fn an_unused_blob_file_is_removed() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    drop(blob_file(&store, b"dropped"));
    assert!(temp_files(&vfs).is_empty());

    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"voted"))
        .unwrap();
    txn.vote().unwrap();
    assert_eq!(temp_files(&vfs).len(), 1);
    txn.abort();
    assert!(temp_files(&vfs).is_empty());
    assert!(vfs.names().iter().all(|name| !name.ends_with(".blob")));
}

#[test]
fn a_blob_over_a_size_limit_is_refused_and_its_file_removed() {
    let vfs = MemVfs::new();
    let options = Options {
        max_record_size: Some(PICKLE.len() as u64),
        max_blob_size: Some(4),
        ..Options::default()
    };
    let store = Store::open_full(vfs.clone(), SystemClock, options).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    // A blob one byte too long, then a pickle one byte too long.
    for (pickle, bytes, limit, size) in [
        (PICKLE, &b"abcde"[..], SizeLimit::Blob, 5),
        (&b"pickles"[..], &b"abcd"[..], SizeLimit::Record, 7),
    ] {
        let error = txn
            .store_blob(oid, Tid::ZERO, pickle, blob_file(&store, bytes))
            .unwrap_err();
        assert!(
            matches!(error, ZoeError::TooLarge { limit: refused, size: s, max, .. }
                if refused == limit && s == size && max == size - 1),
            "{error:?}"
        );
        assert!(temp_files(&vfs).is_empty());
    }
    assert!(txn.staged(oid).unwrap().is_none());
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"abcd"))
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"abcd");
}

#[test]
fn a_restore_transaction_takes_a_blob_over_the_limits() {
    let options = Options {
        max_record_size: Some(1),
        max_transaction_size: Some(1),
        max_blob_size: Some(1),
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = Tid(100);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"a copy"))
        .unwrap();
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);
    assert_eq!(read(&store, oid, tid).unwrap(), b"a copy");
}

#[test]
fn the_owner_of_a_blob_file_is_checked_before_its_size() {
    let options = Options {
        max_blob_size: Some(1),
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let other = Store::open_vfs(MemVfs::new()).unwrap();
    let closed = Store::open_vfs(MemVfs::new()).unwrap();
    let of_other = blob_file(&other, b"too large");
    let of_closed = blob_file(&closed, b"too large");
    drop(closed);

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let error = txn.store_blob(Oid(1), Tid::ZERO, PICKLE, of_other);
    assert!(matches!(error, Err(ZoeError::Protocol(_))), "{error:?}");
    let error = txn.store_blob(Oid(1), Tid::ZERO, PICKLE, of_closed);
    assert!(matches!(error, Err(ZoeError::Closed)), "{error:?}");
    let error = txn.store_blob(Oid(1), Tid::ZERO, PICKLE, blob_file(&store, b"ab"));
    assert!(matches!(error, Err(ZoeError::TooLarge { .. })), "{error:?}");
}

#[test]
fn the_pickle_of_a_blob_counts_toward_the_transaction_size() {
    let options = Options {
        max_transaction_size: Some(PICKLE.len() as u64),
        ..Options::default()
    };
    let store = Store::open_full(MemVfs::new(), SystemClock, options).unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(Oid(1), Tid::ZERO, PICKLE, blob_file(&store, &[7; 1000]))
        .unwrap();
    let error = txn.store(Oid(2), Tid::ZERO, b"a").unwrap_err();
    let size = PICKLE.len() as u64 + 1;
    assert!(
        matches!(error, ZoeError::TooLarge { limit: SizeLimit::Transaction, size: s, .. }
            if s == size),
        "{error:?}"
    );
}

#[test]
fn a_later_store_keeps_the_blob_of_its_oid() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
        .unwrap();
    txn.store(oid, Tid::ZERO, b"later").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"later".to_vec(), tid));
    assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_blob_survives_a_resolved_conflict() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blobs(&store, &[oid], b"first");

    // This transaction read `first`, then another one commits.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, first, b"mine", blob_file(&store, b"mine"))
        .unwrap();
    let other = {
        let mut other = store.tpc_begin(b"", b"", b"").unwrap();
        other
            .store_blob(oid, first, b"other", blob_file(&store, b"other"))
            .unwrap();
        other.vote().unwrap();
        other.finish().unwrap()
    };

    let conflicts = txn.vote_resolving().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!((conflicts[0].0, conflicts[0].1.tid), (oid, other));
    assert_eq!(temp_files(&vfs).len(), 1, "the blob survives the vote");
    txn.store(oid, other, b"resolved").unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(store.load(oid).unwrap(), (b"resolved".to_vec(), tid));
    assert_eq!(read(&store, oid, tid).unwrap(), b"mine");
    assert_eq!(read(&store, oid, other).unwrap(), b"other");
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn store_blob_from_another_thread_is_refused() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let file = blob_file(&store, b"blob");
    let refused = std::thread::scope(|scope| {
        scope
            .spawn(|| txn.store_blob(oid, Tid::ZERO, PICKLE, file))
            .join()
            .unwrap()
    });
    assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
    assert!(txn.staged(oid).unwrap().is_none());
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn the_last_share_of_a_store_removes_the_staged_blob_files() {
    let vfs = MemVfs::new();
    let store = std::sync::Arc::new(Store::open_vfs(vfs.clone()).unwrap());
    let oid = store.new_oid().unwrap();
    let mut txn = crate::commit::Txn::begin(std::sync::Arc::clone(&store), b"", b"", b"");
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
        .unwrap();
    // The transaction holds the last share of the store.
    drop(store);
    assert_eq!(temp_files(&vfs).len(), 1);
    drop(txn);
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_transaction_that_shares_its_store_commits_a_blob() {
    let store = std::sync::Arc::new(Store::open_vfs(MemVfs::new()).unwrap());
    let oid = store.new_oid().unwrap();
    let mut txn = crate::commit::Txn::begin(std::sync::Arc::clone(&store), b"", b"", b"");
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"shared"))
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"shared");
}

#[test]
fn a_blob_file_of_another_size_fails_the_read() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blobs(&store, &[oid], b"blob");
    let name = blob_name(oid, tid);

    for bytes in [&b"blob+"[..], b"blo", b""] {
        Vfs::remove(&vfs, &name).unwrap();
        let mut file = Vfs::create_file(&vfs, &name).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        let error = read(&store, oid, tid).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bytes:?}");
        let inner = error.into_inner().unwrap().downcast::<ZoeError>().unwrap();
        assert!(matches!(*inner, ZoeError::CorruptBlob { .. }), "{bytes:?}");
    }
}

#[test]
fn a_missing_blob_file_is_a_corrupt_blob() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blobs(&store, &[oid], b"blob");
    Vfs::remove(&vfs, &blob_name(oid, tid)).unwrap();
    for result in [
        store.open_blob(oid, tid).map(drop),
        store.verify_blob(oid, tid),
    ] {
        assert!(
            matches!(result, Err(ZoeError::CorruptBlob { oid: o, tid: t }) if o == oid && t == tid),
            "{result:?}"
        );
    }
}

#[test]
fn opening_removes_only_the_blob_files_without_a_record() {
    let (vfs, leftovers) = leftovers_of_a_crash();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    assert!(!Vfs::exists(&vfs, &leftovers.orphan).unwrap());
    assert_eq!(read(&store, leftovers.oid, leftovers.tid).unwrap(), b"kept");
    assert!(temp_files(&vfs).is_empty());
}

/// What `leftovers_of_a_crash` leaves.
struct Leftovers {
    /// An oid with a committed blob that holds `kept`.
    oid: Oid,
    /// The tid of that blob.
    tid: Tid,
    /// The name of a blob file that has no blob record.
    orphan: String,
}

/// Return a store with a committed blob, and the leftovers of a crash:
/// a blob file that a finish placed before its backend commit, with
/// its intent file, the intent file of a finish that committed, and a
/// temporary file.
fn leftovers_of_a_crash() -> (MemVfs, Leftovers) {
    let vfs = MemVfs::new();
    let (oid, orphan, tid) = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        let orphan = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"kept");
        (oid, orphan, tid)
    };
    let crashed = Tid(tid.0 + 1);
    for level in oid_levels(orphan) {
        if !Vfs::exists(&vfs, &level).unwrap() {
            Vfs::create_dir(&vfs, &level).unwrap();
        }
    }
    for (name, bytes) in [
        (blob_name(orphan, crashed), b"orphan".to_vec()),
        (intent_name(crashed), encode_intent(crashed, &[orphan])),
        (intent_name(tid), encode_intent(tid, &[oid])),
        (format!("{TMP_DIR}/7.tmp"), b"temporary".to_vec()),
    ] {
        let mut file = Vfs::create_file(&vfs, &name).unwrap();
        file.write_all(&bytes).unwrap();
    }
    let orphan = blob_name(orphan, crashed);
    (vfs, Leftovers { oid, tid, orphan })
}

#[test]
fn a_pack_syncs_the_directory_of_a_blob_file_that_is_gone() {
    let vfs = MemVfs::new();
    let (oid, tid) = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        (oid, commit_blobs(&store, &[oid], b"blob"))
    };
    let vfs = vfs.durable_copy();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    // A killed process removed the file and did not sync its
    // directory.
    Vfs::remove(&vfs, &blob_name(oid, tid)).unwrap();
    store.blobs().remove_files(&[(oid, tid)]).unwrap();
    vfs.mark("synced");

    for image in vfs.crash_images().iter().filter(|i| i.is_after("synced")) {
        let names = image.vfs().names();
        assert!(!names.contains(&blob_name(oid, tid)), "{image:?}");
    }
}

#[test]
fn a_removal_intent_keeps_the_oids_of_an_older_intent() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let tid = Tid(7);
    let first = store.blobs().intend_removal(tid, &[Oid(2)]).unwrap();
    let second = store
        .blobs()
        .intend_removal(tid, &[Oid(1), Oid(3)])
        .unwrap();
    assert_eq!(first, second);
    let mut bytes = Vec::new();
    Vfs::read_file(&vfs, &second)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(
        decode_intent(tid, &bytes).unwrap(),
        [Oid(1), Oid(2), Oid(3)]
    );
    // The temporary files are gone: each one became the intent file.
    assert_eq!(temp_files(&vfs), [second]);
}

#[test]
fn a_torn_intent_is_removed() {
    let vfs = MemVfs::new();
    let tid = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        commit_blobs(&store, &[oid], b"blob")
    };
    let crashed = Tid(tid.0 + 1);
    let bytes = encode_intent(crashed, &[Oid(9)]);
    let mut file = Vfs::create_file(&vfs, &intent_name(crashed)).unwrap();
    file.write_all(&bytes[..bytes.len() / 2]).unwrap();
    drop(file);

    Store::open_vfs(vfs.clone()).unwrap();
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_layout_other_than_bushy_is_refused() {
    let vfs = MemVfs::new();
    drop(Store::open_vfs(vfs.clone()).unwrap());
    Vfs::create_dir(&vfs, BLOB_DIR).unwrap();
    let mut file = Vfs::create_file(&vfs, LAYOUT_FILE).unwrap();
    file.write_all(b"lawn").unwrap();
    drop(file);
    assert!(matches!(Store::open_vfs(vfs), Err(ZoeError::Format(_))));
}

/// The crash simulation reaches the states that the recovery handles:
/// a blob file in its place without its blob record.
#[test]
fn a_crash_can_leave_a_placed_blob_without_its_record() {
    let vfs = MemVfs::new();
    let (oid, tid) = {
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        (oid, commit_blobs(&store, &[oid], b"blob"))
    };
    let name = blob_name(oid, tid);
    let orphans = vfs
        .crash_images()
        .iter()
        .filter(|image| {
            let image_vfs = image.vfs();
            if !Vfs::exists(&image_vfs, &name).unwrap() {
                return false;
            }
            let store = Store::open_vfs(image_vfs.clone()).unwrap();
            let orphan = store.load(oid).is_err();
            if orphan {
                assert!(!Vfs::exists(&image_vfs, &name).unwrap(), "{image:?}");
            }
            orphan
        })
        .count();
    assert!(orphans > 0);
}

/// Return a filesystem with an empty store, all durable.
fn empty_store() -> MemVfs {
    let vfs = MemVfs::new();
    drop(Store::open_vfs(vfs.clone()).unwrap());
    vfs.durable_copy()
}

/// Open each crash image of `vfs`. A blob record must have its file,
/// and each image after the mark `acknowledged` must have the blob
/// `bytes` of `oid` at `tid`.
fn check_images(vfs: &MemVfs, oid: Oid, tid: Tid, bytes: &[u8]) {
    check_images_in(vfs, BLOB_DIR, oid, tid, bytes);
}

/// Check the crash images of `vfs` as `check_images` does, for a store
/// whose blob directory is `blob_dir` in the same filesystem.
fn check_images_in(vfs: &MemVfs, blob_dir: &str, oid: Oid, tid: Tid, bytes: &[u8]) {
    for image in vfs.crash_images() {
        let image_vfs = image.vfs();
        let opened = if blob_dir == BLOB_DIR {
            Store::open_vfs(image_vfs.clone())
        } else {
            Store::open_relocated(image_vfs.clone(), image_vfs.clone(), blob_dir)
        };
        let store = opened.unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
        assert_no_orphan_in(&store, &image_vfs, &image, blob_dir);
        match store.load(oid) {
            Ok((_, found)) => {
                assert_eq!(found, tid, "{image:?}");
                let mut read = Vec::new();
                let result = store
                    .open_blob(oid, tid)
                    .and_then(|mut blob| Ok(blob.read_to_end(&mut read)?));
                assert!(result.is_ok(), "{image:?} lost a blob file: {result:?}");
                assert_eq!(read, bytes, "{image:?}");
            }
            Err(ZoeError::NotFound { .. }) => {
                assert!(!image.is_after("acknowledged"), "{image:?} lost a blob");
            }
            Err(e) => panic!("{image:?}: {e}"),
        }
    }
}

/// Return the oid and the tid of the blob file `name`, or `None` when
/// `name` is not the name of a blob file.
fn blob_of(name: &str) -> Option<(Oid, Tid)> {
    let parts: Vec<&str> = name.strip_prefix("blobs/")?.split('/').collect();
    let [bytes @ .., file] = &parts[..] else {
        return None;
    };
    if bytes.len() != 8 {
        return None;
    }
    let mut oid = 0;
    for byte in bytes {
        oid = oid << 8 | u64::from(u8::from_str_radix(byte.strip_prefix("0x")?, 16).ok()?);
    }
    let tid = file.strip_prefix("0x")?.strip_suffix(".blob")?;
    Some((Oid(oid), Tid(u64::from_str_radix(tid, 16).ok()?)))
}

/// Check that every blob file of `vfs` has its blob record in `store`.
fn assert_no_orphan(store: &Store, vfs: &MemVfs, image: &CrashImage) {
    assert_no_orphan_in(store, vfs, image, BLOB_DIR);
}

/// Check that every blob file in the blob directory `blob_dir` of `vfs`
/// has its blob record in `store`.
fn assert_no_orphan_in(store: &Store, vfs: &MemVfs, image: &CrashImage, blob_dir: &str) {
    for name in vfs.names().iter().filter(|name| name.ends_with(".blob")) {
        let Some(rest) = name.strip_prefix(blob_dir) else {
            continue;
        };
        let (oid, tid) = blob_of(&format!("{BLOB_DIR}{rest}"))
            .unwrap_or_else(|| panic!("{image:?}: {name} is no blob file"));
        assert!(
            store.blob_path(oid, tid).is_ok(),
            "{image:?} kept {name}, which has no blob record"
        );
    }
}

/// No crash image of a blob commit keeps a blob file without its blob
/// record after an open.
#[test]
fn every_crash_image_of_a_blob_commit_opens_without_an_orphan() {
    let vfs = empty_store();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blobs(&store, &[oid], b"blob");
    vfs.mark("acknowledged");
    drop(store);
    check_images(&vfs, oid, tid, b"blob");
}

/// A finish dies after it places its blob file. The first open
/// removes the file and dies at the sync of its directory. A second
/// open finds the intent file but no blob file. A power loss after it
/// must not bring back a blob file that no record names.
#[test]
fn a_killed_recovery_then_an_open_then_a_power_loss() {
    let built = MemVfs::new();
    let oid = {
        let store = Store::open_vfs(built.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        commit_blobs(&store, &[oid], b"one");
        oid
    };
    let vfs = built.durable_copy();
    let dir = oid_dir(oid);

    let store = Store::open_vfs(vfs.clone()).unwrap();
    let serial = store.load(oid).unwrap().1;
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, serial, PICKLE, blob_file(&store, b"two"))
        .unwrap();
    txn.vote().unwrap();
    vfs.kill_at_dir_sync(&dir);
    assert!(txn.finish().is_err());
    std::mem::forget(store);
    vfs.restart();

    vfs.kill_at_dir_sync(&dir);
    assert!(Store::open_vfs(vfs.clone()).is_err());
    vfs.restart();
    drop(Store::open_vfs(vfs.clone()).unwrap());
    vfs.mark("opened");

    for image in vfs.crash_images() {
        if !image.is_after("opened") {
            continue;
        }
        let image_vfs = image.vfs();
        let store = Store::open_vfs(image_vfs.clone()).unwrap();
        assert_no_orphan(&store, &image_vfs, &image);
    }
}

/// A process dies after it creates `blobs/` and before it syncs the
/// store directory. The next process finds `blobs/` and must still
/// make it durable.
#[test]
fn a_blob_directory_of_a_killed_process_becomes_durable() {
    let vfs = empty_store();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    vfs.kill_at_dir_sync("");
    assert!(store.new_blob_file().is_err());
    // A killed process runs no destructor.
    std::mem::forget(store);
    vfs.restart();
    assert!(Vfs::exists(&vfs, BLOB_DIR).unwrap());

    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let tid = commit_blobs(&store, &[oid], b"blob");
    vfs.mark("acknowledged");
    drop(store);
    check_images(&vfs, oid, tid, b"blob");
}

/// A finish dies after it creates the second level of an oid
/// directory and before it syncs the first level. The next process
/// finds the level and must still make it durable.
#[test]
fn an_oid_directory_of_a_killed_finish_becomes_durable() {
    let vfs = empty_store();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"lost"))
        .unwrap();
    txn.vote().unwrap();
    vfs.kill_at_dir_sync(&oid_levels(oid)[0]);
    assert!(txn.finish().is_err());
    std::mem::forget(store);
    vfs.restart();
    assert!(Vfs::exists(&vfs, &oid_levels(oid)[1]).unwrap());

    let store = Store::open_vfs(vfs.clone()).unwrap();
    let tid = commit_blobs(&store, &[oid], b"blob");
    vfs.mark("acknowledged");
    drop(store);
    check_images(&vfs, oid, tid, b"blob");
}

/// A crash while an open removes an orphan must leave its intent file
/// while the orphan is there. Then the next open removes the orphan.
#[test]
fn a_crash_during_recovery_leaves_no_orphan_behind() {
    let (vfs, leftovers) = leftovers_of_a_crash();
    // The leftovers are on disk when the recovery starts.
    let vfs = vfs.durable_copy();
    drop(Store::open_vfs(vfs.clone()).unwrap());

    let images = vfs.crash_images();
    assert!(images.len() > 1);
    for image in images {
        let image_vfs = image.vfs();
        let store = Store::open_vfs(image_vfs.clone()).unwrap();
        let kept = read(&store, leftovers.oid, leftovers.tid);
        assert_eq!(kept.ok().as_deref(), Some(&b"kept"[..]), "{image:?}");
        assert_no_orphan(&store, &image_vfs, &image);
        assert!(
            !Vfs::exists(&image_vfs, &leftovers.orphan).unwrap(),
            "{image:?} kept an orphan"
        );
    }
}

#[test]
fn a_blob_file_of_another_store_is_refused() {
    let (vfs_a, vfs_b) = (MemVfs::new(), MemVfs::new());
    let a = Store::open_vfs(vfs_a.clone()).unwrap();
    let b = Store::open_vfs(vfs_b.clone()).unwrap();
    // Both temporary files have the same name, each in its own store.
    let own = blob_file(&b, b"own");
    let other = blob_file(&a, b"other");
    assert_eq!(temp_files(&vfs_a), temp_files(&vfs_b));

    let oid = b.new_oid().unwrap();
    let mut txn = b.tpc_begin(b"", b"", b"").unwrap();
    let refused = txn.store_blob(oid, Tid::ZERO, PICKLE, other);
    assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    assert!(temp_files(&vfs_a).is_empty());
    assert_eq!(temp_files(&vfs_b).len(), 1);
    drop(own);
    assert!(temp_files(&vfs_b).is_empty());
}

#[test]
fn a_blob_file_does_not_keep_its_store_open() {
    let dir = std::env::temp_dir().join(format!("zoe-blob-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    let mut file = store.new_blob_file().unwrap();
    file.write_all(b"before").unwrap();
    drop(store);

    // The lock of the store is free, so the store opens again.
    let store = Store::open(&dir).unwrap();
    let error = file.write_all(b"after").unwrap_err();
    let inner = error.into_inner().unwrap().downcast::<ZoeError>().unwrap();
    assert!(matches!(*inner, ZoeError::Closed), "{inner:?}");

    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let refused = txn.store_blob(oid, Tid::ZERO, PICKLE, file);
    assert!(matches!(refused, Err(ZoeError::Closed)), "{refused:?}");
    drop(txn);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Return every directory and file under `dir`, with the bytes of each
/// file.
fn tree(dir: &std::path::Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut found = BTreeMap::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.insert(path.clone(), None);
                todo.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                found.insert(path, Some(bytes));
            }
        }
    }
    found
}

/// A read-only open of a store with the leftovers of a crash reads the
/// committed blobs and changes no file under `blobs/`.
#[test]
fn a_read_only_open_changes_nothing_under_blobs() {
    let dir = std::env::temp_dir().join(format!("zoe-blob-read-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (oid, tid, orphan) = {
        let store = Store::open(&dir).unwrap();
        let oid = store.new_oid().unwrap();
        let orphan = store.new_oid().unwrap();
        let tid = commit_blobs(&store, &[oid], b"kept");
        (oid, tid, orphan)
    };

    // A finish that placed its blob file and stopped before its backend
    // commit, with its intent file, and a temporary file.
    let crashed = Tid(tid.0 + 1);
    std::fs::create_dir_all(dir.join(oid_dir(orphan))).unwrap();
    for (name, bytes) in [
        (blob_name(orphan, crashed), b"orphan".to_vec()),
        (intent_name(crashed), encode_intent(crashed, &[orphan])),
        (format!("{TMP_DIR}/7.tmp"), b"temporary".to_vec()),
    ] {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let before = tree(&dir.join(BLOB_DIR));

    let store = Store::open_read_only(&dir).unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"kept");
    let orphan_blob = store.open_blob(orphan, crashed);
    assert!(matches!(orphan_blob, Err(ZoeError::NotFound { .. })));
    assert!(matches!(store.new_blob_file(), Err(ZoeError::ReadOnly)));
    assert!(matches!(store.blob_temp_dir(), Err(ZoeError::ReadOnly)));
    drop(store);
    assert_eq!(tree(&dir.join(BLOB_DIR)), before);

    // The next writer removes the leftovers.
    drop(Store::open(&dir).unwrap());
    assert!(!dir.join(blob_name(orphan, crashed)).exists());
    assert!(!dir.join(intent_name(crashed)).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_store_owns_only_its_names_in_tmp() {
    for name in [
        "0.tmp",
        "7.tmp",
        "1234567890.tmp",
        "layout.tmp",
        "0x03f0000000000010.intent",
        "0x00000000000000FF.intent",
        "upload-0",
        "upload-12",
    ] {
        assert!(is_owned_name(name), "{name}");
    }
    for name in [
        "tmpk3x9q2b_",
        "tmp7.tmp",
        "client.tmp-",
        "7.tmp-",
        "12a.tmp",
        "-7.tmp",
        ".tmp",
        "7.tmp.part",
        "0x10.intent",
        "0x03f000000000001g.intent",
        "0x03f0000000000010.intent.old",
        "layout.tmp.1",
        "Layout.tmp",
        "upload-",
        "upload-3x",
        "upload--3",
        "upload-+3",
        "Upload-3",
        "",
    ] {
        assert!(!is_owned_name(name), "{name}");
    }
}

/// A writer open removes the files of the store in `blobs/tmp/`, syncs
/// the directory, and keeps every other file with its bytes.
#[test]
fn an_open_keeps_a_file_of_a_client_in_tmp() {
    let vfs = MemVfs::new();
    drop(blob_file(&Store::open_vfs(vfs.clone()).unwrap(), b"x"));
    let client = format!("{TMP_DIR}/tmpk3x9q2b_");
    let owned = format!("{TMP_DIR}/upload-3");
    for name in [&client, &owned] {
        let mut file = Vfs::create_file(&vfs, name).unwrap();
        file.write_all(b"client").unwrap();
        file.sync().unwrap();
    }
    Vfs::sync_dir_at(&vfs, TMP_DIR).unwrap();
    drop(Store::open_vfs(vfs.clone()).unwrap());
    assert_eq!(temp_files(&vfs), std::slice::from_ref(&client));
    assert_eq!(contents(&vfs, &client), b"client");
    // The removal is durable.
    let image = vfs.crash_now(|_| 0);
    assert_eq!(temp_files(&image), [client]);
}

#[test]
fn a_directory_in_tmp_does_not_stop_an_open() {
    let vfs = MemVfs::new();
    drop(blob_file(&Store::open_vfs(vfs.clone()).unwrap(), b"x"));
    Vfs::create_dir(&vfs, &format!("{TMP_DIR}/stray")).unwrap();
    let mut file = Vfs::create_file(&vfs, &format!("{TMP_DIR}/stray/f")).unwrap();
    file.write_all(b"x").unwrap();
    drop(file);
    Store::open_vfs(vfs.clone()).unwrap();
    assert!(Vfs::exists(&vfs, &format!("{TMP_DIR}/stray/f")).unwrap());
}

/// Write `bytes` into the new file `name`, without a sync, as a client
/// writes a blob in the temporary directory. Return the path.
fn client_file(vfs: &MemVfs, name: &str, bytes: &[u8]) -> PathBuf {
    let mut file = Vfs::create_file(vfs, name).unwrap();
    file.write_all(bytes).unwrap();
    PathBuf::from(name)
}

/// Return the bytes of the file `name`.
fn contents(vfs: &MemVfs, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    Vfs::read_file(vfs, name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn a_file_of_the_temporary_directory_commits_by_its_path() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    assert_eq!(store.blob_temp_dir().unwrap(), PathBuf::from(TMP_DIR));
    Vfs::create_dir(&vfs, &format!("{TMP_DIR}/sub")).unwrap();
    let direct = client_file(&vfs, &format!("{TMP_DIR}/direct"), b"direct");
    let nested = client_file(&vfs, &format!("{TMP_DIR}/sub/nested"), b"nested");
    let oids = [store.new_oid().unwrap(), store.new_oid().unwrap()];

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob_at(oids[0], Tid::ZERO, PICKLE, &direct)
        .unwrap();
    txn.store_blob_at(oids[1], Tid::ZERO, PICKLE, &nested)
        .unwrap();
    // The transaction took the files: they have temporary names of the
    // core now.
    assert!(!Vfs::exists(&vfs, &format!("{TMP_DIR}/direct")).unwrap());
    assert_eq!(temp_files(&vfs).len(), 2);
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert_eq!(read(&store, oids[0], tid).unwrap(), b"direct");
    assert_eq!(read(&store, oids[1], tid).unwrap(), b"nested");
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn an_abort_removes_a_taken_file() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    let path = client_file(&vfs, &format!("{TMP_DIR}/client"), b"client");
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob_at(oid, Tid::ZERO, PICKLE, &path).unwrap();
    assert_eq!(temp_files(&vfs).len(), 1);
    txn.abort();
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_refused_take_leaves_the_file_in_its_place() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    let name = format!("{TMP_DIR}/client");
    let path = client_file(&vfs, &name, b"client");
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();

    let refused = std::thread::scope(|scope| {
        scope
            .spawn(|| txn.store_blob_at(oid, Tid::ZERO, PICKLE, &path))
            .join()
            .unwrap()
    });
    assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
    for (oid, data) in [(oid, &b""[..]), (Oid(u64::MAX), PICKLE)] {
        let refused = txn.store_blob_at(oid, Tid::ZERO, data, &path);
        assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    }
    // A path outside the temporary directory, or a name that the core
    // makes there, is refused.
    let intent = intent_name(Tid(9));
    for outside in [
        "client",
        "blobs/client",
        TMP_DIR,
        "blobs/tmp/../client",
        "blobs/tmp/7.tmp",
        &intent,
        NEW_LAYOUT_FILE,
    ] {
        let refused = txn.store_blob_at(oid, Tid::ZERO, PICKLE, Path::new(outside));
        assert!(
            matches!(refused, Err(ZoeError::Protocol(_))),
            "{outside}: {refused:?}"
        );
    }
    let refused = txn.restore_blob_at(oid, PICKLE, &path);
    assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    txn.store(oid, Tid::ZERO, PICKLE).unwrap();
    txn.vote().unwrap();
    let refused = txn.store_blob_at(oid, Tid::ZERO, PICKLE, &path);
    assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    txn.abort();

    assert_eq!(contents(&vfs, &name), b"client");
    assert!(store.lock_table_is_empty());
}

/// No crash image of the commit of a taken file keeps a blob file
/// without its blob record, and each image after the acknowledgement
/// keeps the blob. The file can be in a subdirectory of `blobs/tmp/`.
#[test]
fn every_crash_image_of_a_taken_blob_opens_without_an_orphan() {
    for name in ["blobs/tmp/client", "blobs/tmp/sub/client"] {
        let vfs = empty_store();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oid = store.new_oid().unwrap();
        store.blob_temp_dir().unwrap();
        if parent(name) != TMP_DIR {
            Vfs::create_dir(&vfs, parent(name)).unwrap();
        }
        let path = client_file(&vfs, name, b"taken");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob_at(oid, Tid::ZERO, PICKLE, &path).unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        vfs.mark("acknowledged");
        drop(store);
        check_images(&vfs, oid, tid, b"taken");
    }
}

#[test]
fn a_taken_file_keeps_its_inode_and_a_link_is_refused() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let dir = std::env::temp_dir().join(format!("zoe-blob-take-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    let tmp = store.blob_temp_dir().unwrap();
    assert_eq!(tmp, dir.join(TMP_DIR));
    let path = tmp.join("client");
    std::fs::write(&path, b"client").unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let outside = dir.join("outside");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, tmp.join("link")).unwrap();

    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let refused = txn.store_blob_at(oid, Tid::ZERO, PICKLE, &tmp.join("link"));
    assert!(matches!(refused, Err(ZoeError::Io(_))), "{refused:?}");
    txn.store_blob_at(oid, Tid::ZERO, PICKLE, &path).unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    let committed = std::fs::metadata(store.blob_path(oid, tid).unwrap()).unwrap();
    assert_eq!(committed.ino(), inode);
    assert!(committed.permissions().readonly());
    assert!(!path.exists());
    assert!(tmp.join("link").is_symlink());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Create the levels of the directory of `oid` in `vfs`, as a client
/// that shares the blob directory does, with no sync. Then write the
/// file `name` there, with no sync either. Return the name of the file
/// in `vfs`.
fn shared_file(vfs: &MemVfs, oid: Oid, name: &str, bytes: &[u8]) -> String {
    for level in oid_levels(oid) {
        if !Vfs::exists(vfs, &level).unwrap() {
            Vfs::create_dir(vfs, &level).unwrap();
        }
    }
    let file = format!("{}/{name}", oid_dir(oid));
    client_file(vfs, &file, bytes);
    file
}

#[test]
fn a_file_of_the_oid_directory_commits_by_its_name() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    let oids = [store.new_oid().unwrap(), store.new_oid().unwrap()];
    let first = shared_file(&vfs, oids[0], "0x00abc.tmp-", b"first");
    shared_file(&vfs, oids[1], "0x00def.tmp", b"second");

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob_shared(oids[0], Tid::ZERO, PICKLE, "0x00abc.tmp-")
        .unwrap();
    txn.store_blob_shared(oids[1], Tid::ZERO, PICKLE, "0x00def.tmp")
        .unwrap();
    // The transaction took the files: they have temporary names of the
    // core now.
    assert!(!Vfs::exists(&vfs, &first).unwrap());
    assert_eq!(temp_files(&vfs).len(), 2);
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert_eq!(read(&store, oids[0], tid).unwrap(), b"first");
    assert_eq!(read(&store, oids[1], tid).unwrap(), b"second");
    assert!(temp_files(&vfs).is_empty());
    for oid in oids {
        let names = Vfs::list_entries(&vfs, &oid_dir(oid)).unwrap();
        assert_eq!(names, [format!("{}.blob", tid_repr(tid))]);
    }
}

#[test]
fn a_refused_shared_name_leaves_every_file_in_its_place() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    let oid = store.new_oid().unwrap();
    let file = shared_file(&vfs, oid, "client.tmp-", b"client");
    client_file(&vfs, "blobs/escape.tmp", b"escape");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for name in [
        "",
        ".",
        "..",
        "client",
        "client.tmpx",
        "client.blob",
        "../escape.tmp",
        "sub/client.tmp",
        "/escape.tmp",
        "client\0.tmp",
    ] {
        let refused = txn.store_blob_shared(oid, Tid::ZERO, PICKLE, name);
        assert!(
            matches!(refused, Err(ZoeError::Protocol(_))),
            "{name:?}: {refused:?}"
        );
    }
    // A file that does not exist is an error of the filesystem.
    let refused = txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "missing.tmp-");
    assert!(matches!(refused, Err(ZoeError::Io(_))), "{refused:?}");
    // The checks of the transaction come first.
    let refused = txn.store_blob_shared(oid, Tid::ZERO, b"", "client.tmp-");
    assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
    txn.abort();

    assert_eq!(contents(&vfs, &file), b"client");
    assert_eq!(contents(&vfs, "blobs/escape.tmp"), b"escape");
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn an_abort_removes_a_taken_shared_file() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    store.blob_temp_dir().unwrap();
    let oid = store.new_oid().unwrap();
    let file = shared_file(&vfs, oid, "client.tmp-", b"client");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "client.tmp-")
        .unwrap();
    assert!(!Vfs::exists(&vfs, &file).unwrap());
    assert_eq!(temp_files(&vfs).len(), 1);
    txn.abort();
    assert!(temp_files(&vfs).is_empty());
}

/// A client of a shared blob directory can create the layout file and
/// `tmp/` before the first blob of the store. The store accepts a
/// layout file that says `bushy`, at its open and at its first blob.
#[test]
fn a_layout_file_of_a_client_is_accepted() {
    for open_after in [false, true] {
        let vfs = MemVfs::new();
        let early = (!open_after).then(|| Store::open_vfs(vfs.clone()).unwrap());
        Vfs::create_dir(&vfs, BLOB_DIR).unwrap();
        Vfs::create_dir(&vfs, TMP_DIR).unwrap();
        client_file(&vfs, LAYOUT_FILE, b"bushy\n");
        let store = early.unwrap_or_else(|| Store::open_vfs(vfs.clone()).unwrap());
        let oid = store.new_oid().unwrap();
        shared_file(&vfs, oid, "client.tmp-", b"client");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "client.tmp-")
            .unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(read(&store, oid, tid).unwrap(), b"client");
        assert_eq!(contents(&vfs, LAYOUT_FILE), b"bushy\n");
    }
}

/// No crash image of the commit of a shared file keeps a blob file
/// without its blob record, and each image after the acknowledgement
/// keeps the blob. The client did not sync the levels of the oid
/// directory, nor the file. An image before the rename can keep the
/// file of the client in the oid directory, as ZEO's server does.
#[test]
fn every_crash_image_of_a_shared_blob_opens_without_an_orphan() {
    for neighbour in [false, true] {
        let vfs = empty_store();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        store.blob_temp_dir().unwrap();
        // The blob of a neighbour makes the upper levels of the oid
        // directory, so the client creates only the last one.
        if neighbour {
            commit_blobs(&store, &[store.new_oid().unwrap()], b"neighbour");
        }
        let oid = store.new_oid().unwrap();
        shared_file(&vfs, oid, "client.tmp-", b"shared");
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "client.tmp-")
            .unwrap();
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        vfs.mark("acknowledged");
        drop(store);
        check_images(&vfs, oid, tid, b"shared");
    }
}

#[test]
fn a_shared_file_keeps_its_inode_and_a_link_at_the_check_is_refused() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let dir = std::env::temp_dir().join(format!("zoe-blob-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    store.blob_temp_dir().unwrap();
    let oid = store.new_oid().unwrap();
    let oid_path = dir.join(oid_dir(oid));
    std::fs::create_dir_all(&oid_path).unwrap();
    let path = oid_path.join("client.tmp-");
    std::fs::write(&path, b"client").unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let outside = dir.join("outside.tmp");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, oid_path.join("link.tmp-")).unwrap();

    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let refused = txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "link.tmp-");
    assert!(matches!(refused, Err(ZoeError::Io(_))), "{refused:?}");
    txn.store_blob_shared(oid, Tid::ZERO, PICKLE, "client.tmp-")
        .unwrap();
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    let committed = std::fs::metadata(store.blob_path(oid, tid).unwrap()).unwrap();
    assert_eq!(committed.ino(), inode);
    assert!(committed.permissions().readonly());
    assert!(!path.exists());
    assert!(oid_path.join("link.tmp-").is_symlink());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restore_blob_copies_a_version_with_its_blob() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let (oid, taken) = (store.new_oid().unwrap(), store.new_oid().unwrap());
    let first = commit_blobs(&store, &[oid], b"first");
    let path = client_file(&vfs, &format!("{TMP_DIR}/client"), b"taken");

    let tid = Tid(first.0 + 10);
    let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
    txn.restore_blob(oid, b"restored", blob_file(&store, b"restored"))
        .unwrap();
    txn.restore_blob_at(taken, b"taken", &path).unwrap();
    // The previous serial is the current serial of the store, not a
    // serial of the client.
    let (serial, data) = txn.staged(oid).unwrap().unwrap();
    assert_eq!((serial, &*data), (first, &b"restored"[..]));
    let (serial, data) = txn.staged(taken).unwrap().unwrap();
    assert_eq!((serial, &*data), (Tid::ZERO, &b"taken"[..]));
    txn.vote().unwrap();
    assert_eq!(txn.finish().unwrap(), tid);

    assert_eq!(store.load(oid).unwrap(), (b"restored".to_vec(), tid));
    assert_eq!(read(&store, oid, tid).unwrap(), b"restored");
    assert_eq!(read(&store, oid, first).unwrap(), b"first");
    assert_eq!(read(&store, taken, tid).unwrap(), b"taken");
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn a_change_before_the_vote_fails_a_restored_blob() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let first = commit_blobs(&store, &[oid], b"first");
    let mut txn = store
        .tpc_begin_restore(Tid(u64::MAX - 1), b"", b"", b"")
        .unwrap();
    txn.restore_blob(oid, b"restored", blob_file(&store, b"restored"))
        .unwrap();
    let mut other = store.tpc_begin(b"", b"", b"").unwrap();
    other.store(oid, first, b"other").unwrap();
    other.vote().unwrap();
    let second = other.finish().unwrap();

    // A restored blob is a copy, so the vote never resolves it.
    let refused = txn.vote_resolving();
    assert!(
        matches!(refused, Err(ZoeError::Conflict { oid: o }) if o == oid),
        "{refused:?}"
    );
    assert_eq!(txn.conflict_serials(), Some((second, first)));
    assert!(store.lock_table_is_empty());
    drop(txn);
    assert!(temp_files(&vfs).is_empty());
}

#[test]
fn restore_blob_needs_a_restore_transaction_and_a_pickle() {
    let vfs = MemVfs::new();
    let store = Store::open_vfs(vfs.clone()).unwrap();
    let oid = store.new_oid().unwrap();
    let mut normal = store.tpc_begin(b"", b"", b"").unwrap();
    let mut restore = store.tpc_begin_restore(Tid(100), b"", b"", b"").unwrap();
    for (txn, data) in [(&mut normal, PICKLE), (&mut restore, &b""[..])] {
        let refused = txn.restore_blob(oid, data, blob_file(&store, b"blob"));
        assert!(matches!(refused, Err(ZoeError::Protocol(_))), "{refused:?}");
        assert!(txn.staged(oid).unwrap().is_none());
    }
    let file = blob_file(&store, b"blob");
    let refused = std::thread::scope(|scope| {
        scope
            .spawn(|| restore.restore_blob(oid, PICKLE, file))
            .join()
            .unwrap()
    });
    assert!(matches!(refused, Err(ZoeError::WrongThread)), "{refused:?}");
    // A refused call drops its blob file, and that removes the file.
    assert!(temp_files(&vfs).is_empty());

    // A restored deletion drops the blob of its oid.
    restore
        .restore_blob(oid, PICKLE, blob_file(&store, b"blob"))
        .unwrap();
    restore.restore(oid, None).unwrap();
    assert!(temp_files(&vfs).is_empty());
    restore.vote().unwrap();
    let tid = restore.finish().unwrap();
    assert!(matches!(
        store.open_blob(oid, tid),
        Err(ZoeError::NotFound { .. })
    ));
}

/// Commit a blob that holds `bytes` for a new oid. Return the oid and
/// the tid.
fn commit_new_blob(store: &Store, bytes: &[u8]) -> (Oid, Tid) {
    let oid = store.new_oid().unwrap();
    (oid, commit_blobs(store, &[oid], bytes))
}

#[test]
fn a_relocated_blob_store_keeps_its_files_in_its_directory() {
    let (vfs, parent) = (MemVfs::new(), MemVfs::new());
    let store = Store::open_relocated(vfs.clone(), parent.clone(), "elsewhere").unwrap();
    let tmp = store.blob_temp_dir().unwrap();
    assert_eq!(tmp, PathBuf::from("elsewhere/tmp"));
    let (oid, tid) = commit_new_blob(&store, b"new");
    let taken = store.new_oid().unwrap();
    let path = client_file(&parent, "elsewhere/tmp/client", b"taken");
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob_at(taken, Tid::ZERO, PICKLE, &path).unwrap();
    txn.vote().unwrap();
    let second = txn.finish().unwrap();

    assert_eq!(read(&store, oid, tid).unwrap(), b"new");
    assert_eq!(read(&store, taken, second).unwrap(), b"taken");
    let name = blob_name(oid, tid).replacen(BLOB_DIR, "elsewhere", 1);
    assert_eq!(store.blob_path(oid, tid).unwrap(), PathBuf::from(&name));
    assert_eq!(contents(&parent, &name), b"new");
    assert_eq!(contents(&parent, "elsewhere/.layout"), LAYOUT.as_bytes());
    assert!(!Vfs::exists(&vfs, BLOB_DIR).unwrap());
    drop(store);

    let store = Store::open_relocated(vfs, parent, "elsewhere").unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"new");
}

/// The blob file and its directories are durable in the filesystem of
/// the blob directory when `finish` returns: the parent of the blob
/// directory is synced, not the store directory.
/// The staging area of a store with its blob directory elsewhere stays
/// in the store directory: a spilled pickle goes to `staging/` there,
/// and the blob files go to the blob directory.
#[test]
fn a_relocated_store_keeps_its_staging_area_in_the_store_directory() {
    use crate::clock::SystemClock;
    use crate::store::Options;

    let (vfs, parent) = (MemVfs::new(), MemVfs::new());
    let options = Options {
        staging_memory: 0,
        ..Options::default()
    };
    let blob_parent: Box<dyn Dir> = Box::new(parent.clone());
    let relocated = Some((blob_parent, "elsewhere".to_owned()));
    let store = Store::open_parts(vfs.clone(), SystemClock, options, relocated).unwrap();
    let oid = store.new_oid().unwrap();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    txn.store_blob(oid, Tid::ZERO, PICKLE, blob_file(&store, b"blob"))
        .unwrap();
    assert_eq!(txn.staged_in_memory(), 0);
    let starts = |vfs: &MemVfs, prefix: &str| vfs.names().iter().any(|n| n.starts_with(prefix));
    assert!(starts(&vfs, "staging/"));
    assert!(starts(&parent, "elsewhere/tmp/"));
    txn.vote().unwrap();
    let tid = txn.finish().unwrap();

    assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
    assert!(!starts(&parent, "staging"));
    assert!(!starts(&vfs, BLOB_DIR));
    assert!(starts(&parent, "elsewhere/0x00/"));
}

#[test]
fn a_relocated_blob_is_durable_in_its_own_filesystem() {
    let parent = MemVfs::new();
    let store = Store::open_relocated(MemVfs::new(), parent.clone(), "elsewhere").unwrap();
    let (oid, tid) = commit_new_blob(&store, b"blob");
    parent.mark("acknowledged");
    let name = blob_name(oid, tid).replacen(BLOB_DIR, "elsewhere", 1);
    let images = parent.crash_images();
    assert!(images.iter().any(|image| !image.is_after("acknowledged")));
    for image in images.iter().filter(|image| image.is_after("acknowledged")) {
        assert_eq!(contents(&image.vfs(), &name), b"blob", "{image:?}");
    }
}

#[test]
fn every_crash_image_of_a_relocated_blob_commit_opens_without_an_orphan() {
    let vfs = empty_store();
    let store = Store::open_relocated(vfs.clone(), vfs.clone(), "elsewhere").unwrap();
    let (oid, tid) = commit_new_blob(&store, b"blob");
    vfs.mark("acknowledged");
    drop(store);
    check_images_in(&vfs, "elsewhere", oid, tid, b"blob");
}

#[test]
fn a_writer_refuses_a_store_whose_blob_directory_has_no_layout() {
    let (vfs, parent) = (MemVfs::new(), MemVfs::new());
    // A store without blobs takes any blob directory.
    drop(Store::open_relocated(vfs.clone(), MemVfs::new(), "other").unwrap());
    let (oid, tid) = {
        let store = Store::open_relocated(vfs.clone(), parent.clone(), "elsewhere").unwrap();
        commit_new_blob(&store, b"blob")
    };

    // The default directory, another directory, or a directory that
    // lost its layout file is not the blob directory of the store.
    let refused = [
        Store::open_vfs(vfs.clone()),
        Store::open_relocated(vfs.clone(), MemVfs::new(), "elsewhere"),
        Store::open_relocated(vfs.clone(), parent.clone(), "other"),
    ];
    for result in refused {
        assert!(
            matches!(result, Err(ZoeError::MissingBlobDir { .. })),
            "{:?}",
            result.err()
        );
    }
    assert!(!Vfs::exists(&vfs, BLOB_DIR).unwrap());
    Vfs::remove(&parent, "elsewhere/.layout").unwrap();
    let result = Store::open_relocated(vfs.clone(), parent.clone(), "elsewhere");
    assert!(matches!(result, Err(ZoeError::MissingBlobDir { .. })));

    let mut file = Vfs::create_file(&parent, "elsewhere/.layout").unwrap();
    file.write_all(LAYOUT.as_bytes()).unwrap();
    drop(file);
    let store = Store::open_relocated(vfs, parent, "elsewhere").unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
}

/// Write the file `name` of `vfs`, with its missing directories.
fn put(vfs: &MemVfs, name: &str, bytes: &[u8]) {
    let mut dir = String::new();
    for part in name.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
        dir = if dir.is_empty() {
            (*part).to_owned()
        } else {
            format!("{dir}/{part}")
        };
        if !Vfs::exists(vfs, &dir).unwrap() {
            Vfs::create_dir(vfs, &dir).unwrap();
        }
    }
    let mut file = Vfs::create_file(vfs, name).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn a_writer_refuses_a_blob_directory_with_other_files() {
    for files in [
        &["elsewhere/tmp/report.txt"][..],
        &["elsewhere/notes.txt"],
        &["elsewhere/sub/notes.txt"],
        &["elsewhere/tmp"],
        &["elsewhere/tmp/layout.tmp", "elsewhere/tmp/report.txt"],
    ] {
        let parent = MemVfs::new();
        for name in files {
            put(&parent, name, b"theirs");
        }
        // Each open refuses the directory and removes nothing.
        for _ in 0..2 {
            let result = Store::open_relocated(MemVfs::new(), parent.clone(), "elsewhere");
            assert!(
                matches!(result, Err(ZoeError::NotBlobDir { .. })),
                "{files:?}: {:?}",
                result.err()
            );
            for name in files {
                assert_eq!(contents(&parent, name), b"theirs", "{name}");
            }
        }
    }

    // The same holds for the blob directory in the store directory.
    let vfs = MemVfs::new();
    put(&vfs, "blobs/tmp/report.txt", b"theirs");
    assert!(matches!(
        Store::open_vfs(vfs.clone()),
        Err(ZoeError::NotBlobDir { .. })
    ));
    assert_eq!(contents(&vfs, "blobs/tmp/report.txt"), b"theirs");
}

/// A directory that the first blob of a store left without its layout
/// file is still a blob directory: empty, with an empty `tmp/`, or with
/// only the new layout file in `tmp/`.
#[test]
fn a_blob_directory_that_a_first_blob_left_opens() {
    for files in [&[][..], &["elsewhere/tmp/layout.tmp"]] {
        let parent = MemVfs::new();
        Vfs::create_dir(&parent, "elsewhere").unwrap();
        Vfs::create_dir(&parent, "elsewhere/tmp").unwrap();
        for name in files {
            put(&parent, name, b"bus");
        }
        let store = Store::open_relocated(MemVfs::new(), parent.clone(), "elsewhere").unwrap();
        let (oid, tid) = commit_new_blob(&store, b"blob");
        assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
        assert_eq!(contents(&parent, "elsewhere/.layout"), LAYOUT.as_bytes());
        assert!(!Vfs::exists(&parent, "elsewhere/tmp/layout.tmp").unwrap());
    }
}

/// A blob directory that another program fills after the open gets no
/// layout file: the first blob fails, and the files stay.
#[test]
fn a_first_blob_refuses_a_directory_filled_after_the_open() {
    let parent = MemVfs::new();
    let store = Store::open_relocated(MemVfs::new(), parent.clone(), "elsewhere").unwrap();
    put(&parent, "elsewhere/tmp/report.txt", b"theirs");
    assert!(matches!(
        store.new_blob_file(),
        Err(ZoeError::NotBlobDir { .. })
    ));
    assert!(
        !Vfs::exists(
            &parent,
            LAYOUT_FILE.replacen(BLOB_DIR, "elsewhere", 1).as_str()
        )
        .unwrap()
    );
    assert_eq!(contents(&parent, "elsewhere/tmp/report.txt"), b"theirs");
}

/// Return every file of `vfs` with its bytes.
fn snapshot(vfs: &MemVfs) -> Vec<(String, Vec<u8>)> {
    vfs.names()
        .into_iter()
        .map(|name| {
            let bytes = contents(vfs, &name);
            (name, bytes)
        })
        .collect()
}

/// Two open stores never share a blob directory: the second store gets
/// `Locked`, at its open or at its first blob, and removes nothing.
#[test]
fn a_blob_directory_takes_one_open_store() {
    let parent = MemVfs::new();
    // This store opens before the blob directory exists.
    let late = Store::open_relocated(MemVfs::new(), parent.clone(), "shared").unwrap();
    let first = Store::open_relocated(MemVfs::new(), parent.clone(), "shared").unwrap();
    commit_new_blob(&first, b"committed");
    // A pending blob of the first store: a temporary file in tmp/.
    let pending = blob_file(&first, b"pending");
    let before = snapshot(&parent);
    assert!(
        before
            .iter()
            .any(|(name, _)| name.starts_with("shared/tmp/"))
    );

    let second = Store::open_relocated(MemVfs::new(), parent.clone(), "shared");
    assert!(
        matches!(second, Err(ZoeError::Locked)),
        "{:?}",
        second.err()
    );
    assert!(matches!(late.new_blob_file(), Err(ZoeError::Locked)));
    assert_eq!(snapshot(&parent), before);
    drop(pending);
    drop(late);

    // The lock goes with the store that holds it.
    drop(first);
    drop(Store::open_relocated(MemVfs::new(), parent, "shared").unwrap());
}

#[test]
fn two_os_stores_never_share_a_blob_directory() {
    use crate::store::Options;

    let root = std::env::temp_dir().join(format!("zoe-blob-dir-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let options = Options {
        blob_dir: Some(root.join("shared")),
        ..Options::default()
    };
    let first = Store::open_with(root.join("a"), options.clone()).unwrap();
    commit_new_blob(&first, b"committed");
    let mut pending = first.new_blob_file().unwrap();
    pending.write_all(b"pending").unwrap();
    let before = tree(&root.join("shared"));

    let second = Store::open_with(root.join("b"), options.clone());
    assert!(
        matches!(second, Err(ZoeError::Locked)),
        "{:?}",
        second.err()
    );
    assert_eq!(tree(&root.join("shared")), before);
    drop(pending);
    drop(first);
    drop(Store::open_with(root.join("a"), options).unwrap());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_os_store_takes_its_blob_directory_from_the_options() {
    use crate::store::Options;

    let root = std::env::temp_dir().join(format!("zoe-blob-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (dir, blob_dir) = (root.join("store"), root.join("a").join("blobs"));
    let options = Options {
        blob_dir: Some(blob_dir.clone()),
        ..Options::default()
    };
    let (oid, tid) = {
        let store = Store::open_with(&dir, options.clone()).unwrap();
        assert_eq!(store.blob_temp_dir().unwrap(), blob_dir.join("tmp"));
        let (oid, tid) = commit_new_blob(&store, b"blob");
        let path = store.blob_path(oid, tid).unwrap();
        assert!(path.starts_with(&blob_dir), "{}", path.display());
        (oid, tid)
    };
    assert_eq!(std::fs::read(blob_dir.join(".layout")).unwrap(), b"bushy");
    assert!(!dir.join(BLOB_DIR).exists());

    let refused = Store::open(&dir);
    assert!(matches!(refused, Err(ZoeError::MissingBlobDir { .. })));
    let store = Store::open_with(&dir, options).unwrap();
    assert_eq!(read(&store, oid, tid).unwrap(), b"blob");
    drop(store);
    std::fs::remove_dir_all(&root).unwrap();
}

/// A blob directory that a store refuses before it opens leaves no
/// trace: no store directory, no database and no lock file.
#[test]
fn a_refused_blob_directory_creates_nothing() {
    use crate::store::Options;

    let root = std::env::temp_dir().join(format!("zoe-blob-dir-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("file"), b"x").unwrap();
    // A blob directory under a file cannot be created.
    let under_file = root.join("file").join("blobs");
    for blob_dir in [root.join("file"), PathBuf::from("/"), under_file] {
        let options = Options {
            blob_dir: Some(blob_dir.clone()),
            ..Options::default()
        };
        let result = Store::open_with(root.join("store"), options);
        assert!(
            matches!(&result, Err(ZoeError::Io(_))),
            "{}: {:?}",
            blob_dir.display(),
            result.err()
        );
        assert!(!root.join("store").exists());
    }
    std::fs::remove_dir_all(&root).unwrap();
}
