//! Tests of the cache file (ADR 0034, stage 2): `Cache::open`, the close,
//! the checks of the open, and the verification at the open of a pool.
//!
//! The cache files go to the temporary directory of cargo in the target
//! directory. The pool tests run `zoe serve` in the test process, on stores
//! in the temporary directory of the system (`common::start`).

mod common;

use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{commit_other, start};
use zoe_client::{Cache, Client, Connect, Error, FileCheck, Open, Pool, Reconnect, Tokio};
use zoe_core::{Oid, Tid};
use zoe_server::serve::Server;

const TIMEOUT: Duration = Duration::from_secs(30);
const SIZE: u64 = 64 * 1024;
const STORAGE: &[u8] = b"1";

// The words of the header page that the tests change.
const H_BOOT: u64 = 4;
const H_CLEAN: u64 = 6;
const H_STATE_SUM: u64 = 9;

/// Return a new empty directory for the test `name`.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cache-file-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Return 512 bytes for `oid` that come once in a file.
fn data(oid: u64) -> Vec<u8> {
    let mut state = oid.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..512)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Open the file `path`, and verify it as a server at `tid` without a
/// commit after its `applied` would.
fn open(path: &Path) -> Cache {
    let cache = Cache::open(path, SIZE, STORAGE).unwrap();
    let applied = cache.applied();
    cache.verify(applied, Some(&[]));
    cache
}

/// Write a file with the current entries of the oids 1 to `count`, applied
/// up to the tid 10, and close it.
fn fill(path: &Path, count: u64) {
    let cache = Cache::open(path, SIZE, STORAGE).unwrap();
    cache.apply(Tid(10), &[]);
    for oid in 1..=count {
        assert!(cache.store(Oid(oid), Tid(5), None, &data(oid), Tid(10)));
    }
    cache.close();
}

/// Write the word `index` of the file `path`.
fn poke(path: &Path, index: u64, value: u64) {
    let file = OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(&value.to_le_bytes(), index * 8).unwrap();
}

/// Return the offset of the data of `oid` in the file `path`, if any.
fn place_of(path: &Path, oid: u64) -> Option<usize> {
    let content = fs::read(path).unwrap();
    let window = &data(oid)[128..192];
    content.windows(window.len()).position(|w| w == window)
}

/// Return the offset of the data of `oid` in the file `path`.
fn place(path: &Path, oid: u64) -> usize {
    place_of(path, oid).expect("the file holds the data of the entry")
}

#[test]
fn a_closed_file_serves_its_entries_after_the_verification() {
    let path = dir("reopen").join("c-1.zoec");
    fill(&path, 3);

    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(10));
    // Before its verification, the file serves nothing and stores nothing.
    assert!(!cache.verified());
    assert_eq!(cache.load(Oid(1)), None);
    assert!(!cache.store(Oid(4), Tid(5), None, b"four", Tid(10)));
    cache.verify(Tid(10), Some(&[]));
    assert!(cache.verified());
    for oid in 1..=3 {
        assert_eq!(cache.load(Oid(oid)), Some((data(oid), Tid(5))));
    }
    assert_eq!(cache.load_before(Oid(2), Tid(11)).unwrap().data, data(2));
}

#[test]
fn a_new_file_serves_at_once_and_has_its_length_for_ever() {
    let path = dir("new").join("c-1.zoec");
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert!(cache.verified());
    assert_eq!(cache.applied(), Tid(0));
    let len = fs::metadata(&path).unwrap().len();
    // The header page, 1,024 slots of 16 bytes, and the ring.
    assert_eq!(len, 4096 + 1024 * 16 + SIZE);
    cache.apply(Tid(10), &[]);
    for oid in 1..=1000 {
        cache.store(Oid(oid), Tid(5), None, &data(oid), Tid(10));
    }
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    cache.close();
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
}

#[test]
fn a_second_open_of_the_file_fails_until_the_close() {
    let path = dir("lock").join("c-1.zoec");
    let first = Cache::open(&path, SIZE, STORAGE).unwrap();
    let error = Cache::open(&path, SIZE, STORAGE).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    first.close();
    // The closed cache keeps its mapping, and the lock is free.
    let second = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(first.load(Oid(1)), None);
    drop(first);
    second.close();
}

#[test]
fn a_file_of_another_storage_or_size_is_dropped() {
    let path = dir("other").join("c-1.zoec");
    fill(&path, 3);
    let cache = Cache::open(&path, SIZE, b"2").unwrap();
    assert_eq!(cache.applied(), Tid(0));
    assert!(cache.verified());
    assert_eq!(cache.load(Oid(1)), None);
    cache.close();

    fill(&path, 3);
    let cache = Cache::open(&path, SIZE * 2, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(0));
    assert_eq!(cache.load(Oid(1)), None);
    cache.close();
}

#[test]
fn a_file_of_another_version_is_dropped() {
    let path = dir("version").join("c-1.zoec");
    fill(&path, 3);
    let magic = u64::from_le_bytes(*b"ZOECACH\x02");
    poke(&path, 0, magic);
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(0));
    assert_eq!(cache.load(Oid(1)), None);
}

#[test]
fn a_file_that_is_not_a_cache_file_of_zoe_is_left_as_it_is() {
    let path = dir("foreign").join("c-1.zec");
    let content: Vec<u8> = b"ZEC3".iter().copied().chain(data(1)).collect();
    fs::write(&path, &content).unwrap();
    let error = Cache::open(&path, SIZE, STORAGE).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path).unwrap(), content);
}

#[test]
fn a_drop_zeroes_a_file_of_the_same_length_in_place() {
    let path = dir("in-place").join("c-1.zoec");
    fill(&path, 3);
    let first = open(&path);
    let len = fs::metadata(&path).unwrap().len();
    first.close();
    // Another storage drops the file. The closed cache still maps it, and
    // the file keeps its length, so a read of the closed cache never
    // faults.
    let second = Cache::open(&path, SIZE, b"2").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    assert_eq!(first.applied(), Tid(0));
    assert_eq!(second.applied(), Tid(0));
    assert!(place_of(&path, 1).is_none());
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "APFS makes no sparse copy: Linux only"
)]
fn a_kept_sparse_file_gets_its_disk_space_again() {
    let path = dir("sparse").join("c-1.zoec");
    fill(&path, 3);
    // Copy the file without its zero pages, as a copy tool can.
    let content = fs::read(&path).unwrap();
    let copy = path.with_extension("copy");
    let mut file = fs::File::create(&copy).unwrap();
    for (i, page) in content.chunks(4096).enumerate() {
        if page.iter().any(|b| *b != 0) {
            file.seek(SeekFrom::Start(i as u64 * 4096)).unwrap();
            file.write_all(page).unwrap();
        }
    }
    file.set_len(content.len() as u64).unwrap();
    drop(file);
    fs::rename(&copy, &path).unwrap();
    let len = content.len() as u64;
    assert!(fs::metadata(&path).unwrap().blocks() * 512 < len);

    let cache = open(&path);
    assert!(fs::metadata(&path).unwrap().blocks() * 512 >= len);
    assert_eq!(cache.load(Oid(1)).unwrap().0, data(1));
}

#[test]
fn a_name_of_the_storage_longer_than_256_bytes_is_refused() {
    let path = dir("name").join("c-1.zoec");
    let error = Cache::open(&path, SIZE, &[b'x'; 257]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn a_flipped_byte_drops_its_entry_only_and_the_entry_comes_back() {
    let path = dir("flip").join("c-1.zoec");
    fill(&path, 5);
    let offset = place(&path, 3) as u64;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut byte = [0u8];
    file.read_exact_at(&mut byte, offset + 10).unwrap();
    file.write_all_at(&[byte[0] ^ 0xFF], offset + 10).unwrap();
    drop(file);

    let cache = open(&path);
    assert_eq!(cache.load(Oid(3)), None);
    for oid in [1, 2, 4, 5] {
        assert_eq!(cache.load(Oid(oid)).unwrap().0, data(oid));
    }
    // A load from the server stores the same revision again.
    assert!(cache.store(Oid(3), Tid(5), None, &data(3), Tid(10)));
    assert_eq!(cache.load(Oid(3)).unwrap().0, data(3));
}

#[test]
fn a_file_cut_short_is_dropped() {
    let path = dir("cut").join("c-1.zoec");
    fill(&path, 3);
    let offset = place(&path, 1) as u64;
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(offset + 32)
        .unwrap();
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(0));
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(fs::metadata(&path).unwrap().len(), 4096 + 1024 * 16 + SIZE);
}

#[test]
fn a_file_that_a_process_left_open_keeps_its_entries_on_the_same_boot() {
    let path = dir("dirty").join("c-1.zoec");
    fill(&path, 3);
    // The open writes the boot id of this kernel and marks the file not
    // clean, as a process that dies leaves it.
    poke(&path, H_CLEAN, 0);
    let cache = open(&path);
    assert_eq!(cache.applied(), Tid(10));
    assert_eq!(cache.load(Oid(1)).unwrap().0, data(1));
}

#[test]
fn a_file_that_was_open_at_a_restart_of_the_kernel_is_dropped() {
    let path = dir("reboot").join("c-1.zoec");
    fill(&path, 3);
    poke(&path, H_CLEAN, 0);
    poke(&path, H_BOOT, 1);
    poke(&path, H_BOOT + 1, 2);
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(0));
    assert_eq!(cache.load(Oid(1)), None);
}

#[test]
fn a_clean_file_with_a_bad_state_checksum_is_dropped() {
    let path = dir("state").join("c-1.zoec");
    fill(&path, 3);
    poke(&path, H_STATE_SUM, 7);
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    assert_eq!(cache.applied(), Tid(0));
}

#[test]
fn a_cache_that_drops_without_a_close_is_closed() {
    let path = dir("drop").join("c-1.zoec");
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    cache.apply(Tid(10), &[]);
    assert!(cache.store(Oid(1), Tid(5), None, &data(1), Tid(10)));
    drop(cache);
    let cache = open(&path);
    assert_eq!(cache.load(Oid(1)).unwrap().0, data(1));
}

#[test]
fn before_its_verification_a_commit_drops_and_applied_stays() {
    let path = dir("unverified").join("c-1.zoec");
    fill(&path, 3);
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    cache.apply(Tid(12), &[Oid(1)]);
    cache.commit(Tid(13), &[(Oid(2), Some(b"thirteen".as_slice()))]);
    assert_eq!(cache.applied(), Tid(10));
    cache.verify(Tid(13), Some(&[]));
    assert_eq!(cache.load(Oid(1)), None);
    assert_eq!(cache.load(Oid(2)), None);
    assert_eq!(cache.load(Oid(3)).unwrap().0, data(3));
}

#[test]
fn a_verification_without_the_commits_moves_applied_down_too() {
    let path = dir("behind").join("c-1.zoec");
    fill(&path, 3);
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    cache.verify(Tid(7), None);
    assert_eq!(cache.applied(), Tid(7));
    assert_eq!(cache.load(Oid(1)), None);
    assert!(cache.store(Oid(1), Tid(5), None, &data(1), Tid(7)));
}

// The verification at the open of a pool.

/// Open a pool on `server` with the cache file `path`, and wait until the
/// cache serves.
async fn open_pool(server: &Server, path: &Path) -> Pool {
    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    let cache = Cache::open(path, SIZE, STORAGE).unwrap();
    let pool = Pool::with_cache(Arc::new(Tokio), listener, connect, 2, Some(cache));
    tokio::time::timeout(TIMEOUT, pool.ready()).await.unwrap();
    pool
}

/// Load each oid through `pool` so the cache stores it, then close the pool.
async fn load_all(pool: Pool, oids: &[Oid]) {
    for oid in oids {
        pool.load(*oid).await.unwrap();
        assert!(pool.cache().unwrap().load(*oid).is_some());
    }
    pool.disconnect();
}

#[tokio::test]
async fn a_pool_verifies_its_file_at_the_open() {
    let name = "pool-quick";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(3).await.unwrap();
    let mut serials = Vec::new();
    for (i, oid) in oids.iter().enumerate() {
        serials.push(commit_other(&other, *oid, Tid(0), &data(i as u64)).await);
    }
    load_all(open_pool(&server, &path).await, &oids).await;

    // An up-to-date file serves every oid.
    let pool = open_pool(&server, &path).await;
    assert_eq!(pool.take_file_check(), Some(FileCheck::Kept));
    assert_eq!(pool.take_file_check(), None);
    let cache = pool.cache().unwrap().clone();
    for oid in &oids {
        assert!(cache.load(*oid).is_some());
    }
    pool.disconnect();

    // A commit while the pool was closed drops its oid only.
    let tid = commit_other(&other, oids[0], serials[0], b"new").await;
    let pool = open_pool(&server, &path).await;
    assert_eq!(pool.take_file_check(), Some(FileCheck::Quick));
    let cache = pool.cache().unwrap().clone();
    assert_eq!(cache.applied(), tid);
    assert_eq!(cache.load(oids[0]), None);
    assert!(cache.load(oids[1]).is_some());
    assert_eq!(pool.load(oids[0]).await.unwrap().0, b"new");
    pool.disconnect();
}

#[tokio::test]
async fn a_pool_drops_a_file_ahead_of_its_server() {
    let name = "pool-behind";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oid = other.new_oids(1).await.unwrap()[0];
    let tid = commit_other(&other, oid, Tid(0), b"one").await;
    load_all(open_pool(&server, &path).await, &[oid]).await;

    // The file claims commits that the server does not have.
    poke(&path, 7, tid.0 + 1000);
    poke(&path, H_CLEAN, 0);
    let pool = open_pool(&server, &path).await;
    assert_eq!(
        pool.take_file_check(),
        Some(FileCheck::Ahead {
            applied: Tid(tid.0 + 1000),
            server: tid,
        })
    );
    let cache = pool.cache().unwrap().clone();
    assert_eq!(cache.applied(), tid);
    assert_eq!(cache.load(oid), None);
    pool.disconnect();
}

#[tokio::test]
async fn a_pool_drops_a_file_whose_last_transaction_the_server_does_not_hold() {
    let name = "pool-other";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(2).await.unwrap();
    let first = commit_other(&other, oids[0], Tid(0), b"zero").await;
    load_all(open_pool(&server, &path).await, &oids[..1]).await;
    let last = commit_other(&other, oids[1], Tid(0), b"one").await;
    assert!(other.holds_transaction(first).await.unwrap());
    assert!(!other.holds_transaction(Tid(first.0 + 1)).await.unwrap());

    // The file saw a transaction that the server does not hold, below the
    // last tid of the server: the server has another history, as a server
    // restored from an older copy and then committed to.
    poke(&path, 7, first.0 + 1);
    poke(&path, H_CLEAN, 0);
    let pool = open_pool(&server, &path).await;
    assert_eq!(
        pool.take_file_check(),
        Some(FileCheck::Other {
            applied: Tid(first.0 + 1),
        })
    );
    let cache = pool.cache().unwrap().clone();
    assert_eq!(cache.applied(), last);
    assert_eq!(cache.load(oids[0]), None);
    assert_eq!(pool.load(oids[0]).await.unwrap().0, b"zero");
    pool.disconnect();
}

/// Return an `Open` to `server` that fails while `gate` is false.
fn gated(server: std::net::SocketAddr, gate: Arc<AtomicBool>) -> Open {
    Arc::new(move |_| {
        let open = gate.load(Ordering::SeqCst);
        Box::pin(async move {
            if !open {
                return Err(Error::Disconnected);
            }
            let client = Client::connect(server, "1").await?;
            let connect: Connect = Arc::new(move || Box::pin(Client::connect(server, "1")));
            Ok((client, connect))
        })
    })
}

#[tokio::test]
async fn a_pool_without_a_server_at_the_open_verifies_its_file_at_its_first_connection() {
    let name = "pool-fresh";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oids = other.new_oids(2).await.unwrap();
    let first = commit_other(&other, oids[0], Tid(0), b"zero").await;
    commit_other(&other, oids[1], Tid(0), b"one").await;
    load_all(open_pool(&server, &path).await, &oids).await;
    let tid = commit_other(&other, oids[0], first, b"new").await;

    let gate = Arc::new(AtomicBool::new(false));
    let reconnect = Reconnect::new(
        gated(server.addr(), gate.clone()),
        Duration::from_millis(20),
    );
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, Some(cache), reconnect);
    // Without a server, the file waits for its verification: `ready` waits,
    // and the file serves nothing.
    let waited = tokio::time::timeout(Duration::from_millis(200), pool.ready()).await;
    assert!(waited.is_err());
    let cache = pool.cache().unwrap().clone();
    assert!(!cache.verified());
    assert_eq!(cache.load(oids[1]), None);
    assert_eq!(pool.take_file_check(), None);

    gate.store(true, Ordering::SeqCst);
    tokio::time::timeout(TIMEOUT, pool.ready()).await.unwrap();
    assert_eq!(pool.take_file_check(), Some(FileCheck::Quick));
    assert_eq!(cache.applied(), tid);
    assert_eq!(cache.load(oids[0]), None);
    assert_eq!(cache.load(oids[1]).unwrap().0, b"one");
    pool.disconnect();
}

#[tokio::test]
async fn ready_returns_at_the_close() {
    let name = "pool-closed";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    fill(&path, 1);
    let gate = Arc::new(AtomicBool::new(false));
    let reconnect = Reconnect::new(gated(server.addr(), gate), Duration::from_millis(20));
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    let pool = Pool::with_reconnect(Arc::new(Tokio), None, 2, Some(cache), reconnect);
    pool.disconnect();
    tokio::time::timeout(TIMEOUT, pool.ready()).await.unwrap();
}

/// Return the word `index` of the bytes `image` of a file.
fn word(image: &[u8], index: u64) -> u64 {
    let at = index as usize * 8;
    u64::from_le_bytes(image[at..at + 8].try_into().unwrap())
}

#[test]
fn an_open_that_writes_nothing_leaves_the_clean_file_as_it_was() {
    let path = dir("no-write").join("c-1.zoec");
    fill(&path, 3);
    let before = fs::read(&path).unwrap();
    let cache = open(&path);
    for oid in 1..=3 {
        assert_eq!(cache.load(Oid(oid)).unwrap().0, data(oid));
    }
    // A crash now leaves the file of the last close, clean: it opens with
    // its entries.
    let image = fs::read(&path).unwrap();
    assert!(image == before);
    let copy = path.with_file_name("image.zoec");
    fs::write(&copy, &image).unwrap();
    let reopened = open(&copy);
    assert_eq!(reopened.applied(), Tid(10));
    assert_eq!(reopened.load(Oid(2)).unwrap().0, data(2));
    // The close of a file that nothing wrote writes nothing.
    cache.close();
    assert!(fs::read(&path).unwrap() == before);
}

#[test]
fn the_first_write_marks_the_file_before_it_changes_it() {
    let path = dir("first-write").join("c-1.zoec");
    fill(&path, 3);
    let cache = open(&path);
    assert_eq!(word(&fs::read(&path).unwrap(), H_CLEAN), 1);
    assert!(cache.store(Oid(4), Tid(5), None, &data(4), Tid(10)));
    // Any image that holds the write holds the mark too.
    let image = fs::read(&path).unwrap();
    assert!(place_of(&path, 4).is_some());
    assert_eq!(word(&image, H_CLEAN), 0);
    assert_ne!((word(&image, H_BOOT), word(&image, H_BOOT + 1)), (0, 0));
    let copy = path.with_file_name("image.zoec");
    fs::write(&copy, &image).unwrap();
    // On the same boot, the image keeps its entries.
    let same = open(&copy);
    assert_eq!(same.load(Oid(4)).unwrap().0, data(4));
    same.close();
    // After a restart of the kernel, it is dropped.
    fs::write(&copy, &image).unwrap();
    poke(&copy, H_BOOT, 1);
    let rebooted = Cache::open(&copy, SIZE, STORAGE).unwrap();
    assert_eq!(rebooted.applied(), Tid(0));
    cache.close();
}

#[test]
fn a_failed_mark_disables_the_cache_and_leaves_the_file_clean() {
    let path = dir("failed-mark").join("c-1.zoec");
    fill(&path, 3);
    let before = fs::read(&path).unwrap();
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    cache.fail_next_mark();
    // The verification drops the current entry of the oid 1: a write.
    cache.verify(Tid(12), Some(&[Oid(1)]));
    assert!(cache.is_disabled());
    assert!(!cache.verified());
    assert_eq!(cache.load(Oid(2)), None);
    assert!(!cache.store(Oid(5), Tid(5), None, b"five", Tid(12)));
    cache.close();
    assert!(fs::read(&path).unwrap() == before);
}

#[tokio::test]
async fn a_failed_mark_ends_the_wait_of_ready_at_once() {
    let name = "pool-failed-mark";
    let server = start(name);
    let path = dir(&format!("{name}-var")).join("c-1.zoec");
    let other = Client::connect(server.addr(), "1").await.unwrap();
    let oid = other.new_oids(1).await.unwrap()[0];
    let first = commit_other(&other, oid, Tid(0), b"one").await;
    load_all(open_pool(&server, &path).await, &[oid]).await;
    // A commit while the pool is closed: the verification writes.
    commit_other(&other, oid, first, b"two").await;

    let addr = server.addr();
    let listener = Client::connect(addr, "1").await.unwrap();
    let connect: Connect = Arc::new(move || Box::pin(Client::connect(addr, "1")));
    let cache = Cache::open(&path, SIZE, STORAGE).unwrap();
    cache.fail_next_mark();
    let pool = Pool::with_cache(Arc::new(Tokio), listener, connect, 2, Some(cache));
    tokio::time::timeout(Duration::from_secs(5), pool.ready())
        .await
        .expect("ready ends when the mark fails");
    let cache = pool.cache().unwrap().clone();
    assert!(cache.is_disabled());
    assert_eq!(cache.load(oid), None);
    assert_eq!(pool.load(oid).await.unwrap().0, b"two");
    pool.disconnect();
}
