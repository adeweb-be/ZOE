//! The layout of the client cache: one mapping that holds a header, an index
//! and a ring of entries. Not for merge.
//!
//! The same code serves three stages: an anonymous mapping in one process, a
//! file that one process maps, and a file that the processes of a node map
//! together.
//!
//! Rules that keep a stale entry out of every answer:
//!
//! - An entry `(oid, start, end)` with `end != 0` says that the revision of
//!   `start` is valid for the snapshots in `[start, end)`. Such an entry is
//!   true for ever.
//! - An entry with `end == 0` is current. It is true for the snapshots in
//!   `[start, applied]`, where `applied` is the tid up to which the cache
//!   applied the invalidations.
//! - `apply` ends the current entries of the oids of a transaction before it
//!   moves `applied`.
//! - A process stores a current entry only when `applied` is not above the
//!   tid up to which the process read its stream.
//! - A reader loads `applied` first, then the index, then the entry. It copies
//!   the entry, checks its checksums, then checks that the ring did not
//!   overwrite it during the copy.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Mutex, MutexGuard};

use xxhash_rust::xxh3::xxh3_64;

pub const HEADER: usize = 4096;
pub const ENTRY_HEAD: usize = 72;
const MAGIC: &[u8; 8] = b"ZOECACH\x01";
const SLOT: usize = 16;
const MAX_PROBE: usize = 32;
pub const MAX_CHAIN: usize = 8;
pub const CODEC_RAW: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;

// The header. The static part (magic, ring, slots) has a checksum.
const H_RING: usize = 8;
const H_SLOTS: usize = 16;
const H_STATIC_SUM: usize = 24;
const H_BOOT: usize = 32;
const H_CLEAN: usize = 48;
const H_APPLIED: usize = 56;
const H_HEAD: usize = 64;
const H_STATE_SUM: usize = 72;

// The head of an entry. `end` is the only field that changes in place. Its
// change rewrites the head checksum too.
const E_POS: usize = 0;
const E_OID: usize = 8;
const E_START: usize = 16;
const E_END: usize = 24;
const E_PREV: usize = 32;
const E_LEN: usize = 40;
const E_RAW: usize = 44;
const E_CODEC: usize = 48;
const E_TOTAL: usize = 52;
const E_DSUM: usize = 56;
const E_HSUM: usize = 64;

#[derive(Debug)]
pub enum OpenError {
    Io(io::Error),
    /// Another process holds the file. This is the `LockError` of ZEO.
    Locked,
    /// Other processes map the file, and its header is bad.
    BadShared,
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        OpenError::Io(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// An anonymous mapping, for one process.
    Memory,
    /// A file that one process maps. A second process fails with `Locked`.
    Private,
    /// A file that the processes of a node map together.
    Shared,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub ring: u64,
    pub mode: Mode,
    /// Compress an entry when its data has at least this many bytes.
    pub compress_min: Option<usize>,
    pub level: i32,
}

/// What `open` found in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    Created,
    Reused,
    /// The file was bad, or the machine restarted and the file was not closed
    /// cleanly. The cache dropped its content.
    Dropped(&'static str),
    Joined,
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

fn map(fd: Option<&File>, len: usize) -> io::Result<Mapping> {
    let (flags, raw) = match fd {
        Some(f) => (libc::MAP_SHARED, f.as_raw_fd()),
        None => (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1),
    };
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            raw,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Mapping {
        ptr: ptr as *mut u8,
        len,
    })
}

fn flock(f: &File, op: libc::c_int) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), op) == 0 }
}

/// An open-file-description lock on byte 0 of the file. The kernel drops it
/// when the process dies.
fn ofd(f: &File, cmd: libc::c_int, kind: libc::c_short) -> bool {
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = kind;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = 0;
    fl.l_len = 1;
    loop {
        let r = unsafe { libc::fcntl(f.as_raw_fd(), cmd, &mut fl) };
        if r == 0 {
            return true;
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return false;
    }
}

/// The boot id of the kernel. `CACHEX_BOOT_ID` replaces it in the tests, to
/// act as a restart of the machine.
pub fn boot_id() -> [u8; 16] {
    let text = std::env::var("CACHEX_BOOT_ID").unwrap_or_else(|_| {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default()
    });
    let hex: Vec<u8> = text.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).take(16).enumerate() {
        let s = std::str::from_utf8(pair).unwrap_or("0");
        out[i] = u8::from_str_radix(s, 16).unwrap_or(0);
    }
    out
}

/// `CACHEX_NO_BOOT_RULE` turns the boot rule off: a canary for the tests.
fn no_boot_rule() -> bool {
    std::env::var_os("CACHEX_NO_BOOT_RULE").is_some()
}

/// `CACHEX_NO_SEEN_RULE` turns the rule of a current store off: a canary.
fn no_seen_rule() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("CACHEX_NO_SEEN_RULE").is_some())
}

pub fn slots_for(ring: u64) -> u64 {
    (ring / 256).max(1024).next_power_of_two()
}

pub fn file_len(ring: u64, slots: u64) -> usize {
    HEADER + slots as usize * SLOT + ring as usize
}

#[derive(Clone, Copy, Debug)]
pub struct EntryHead {
    pub pos: u64,
    pub oid: u64,
    pub start: u64,
    pub end: u64,
    pub prev: u64,
    pub len: u32,
    pub raw: u32,
    pub codec: u8,
    pub total: u32,
    pub dsum: u64,
    bytes: [u8; ENTRY_HEAD],
}

#[derive(Default, Debug)]
pub struct Stats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub bad: AtomicU64,
    pub stores: AtomicU64,
    pub applied: AtomicU64,
    pub skipped: AtomicU64,
    pub refused: AtomicU64,
    pub rearranged: AtomicU64,
}

pub struct Cache {
    map: Mapping,
    file: Option<File>,
    pub ring: u64,
    slots: u64,
    index_off: usize,
    ring_off: usize,
    pub mode: Mode,
    wlock: Mutex<()>,
    compress_min: Option<usize>,
    level: i32,
    pub opened: Opened,
    pub stats: Stats,
}

/// When set, the writer lock stores 1 in this word while a process holds
/// it, and 0 before it releases it. The kill loop of `sim` reads it.
pub static WRITE_FLAG: std::sync::atomic::AtomicPtr<AtomicU64> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

fn write_flag(v: u64) {
    let p = WRITE_FLAG.load(Ordering::Relaxed);
    if !p.is_null() {
        unsafe { (*p).store(v, Ordering::SeqCst) };
    }
}

pub struct WriteGuard<'a> {
    cache: &'a Cache,
    _inner: MutexGuard<'a, ()>,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        write_flag(0);
        if let Some(f) = &self.cache.file
            && self.cache.mode == Mode::Shared
        {
            ofd(f, libc::F_OFD_SETLK, libc::F_UNLCK as libc::c_short);
        }
    }
}

thread_local! {
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// A hit: the data, its start tid and its end tid (0 for a current entry).
pub struct Hit {
    pub start: u64,
    pub end: u64,
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

impl Cache {
    pub fn open(path: Option<&Path>, cfg: &Config) -> Result<Cache, OpenError> {
        let want_slots = slots_for(cfg.ring);
        let Some(path) = path else {
            let map = map(None, file_len(cfg.ring, want_slots))?;
            let c = Cache::from_parts(map, None, cfg.ring, want_slots, cfg, Opened::Created);
            c.init_header();
            return Ok(c);
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        if cfg.mode == Mode::Private && !flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            return Err(OpenError::Locked);
        }
        // The open runs under the writer lock, so two opens never race.
        ofd(&file, libc::F_OFD_SETLKW, libc::F_WRLCK as libc::c_short);
        let result = Self::open_locked(file, cfg, want_slots);
        if let Ok(c) = &result {
            ofd(
                c.file.as_ref().unwrap(),
                libc::F_OFD_SETLK,
                libc::F_UNLCK as libc::c_short,
            );
        }
        result
    }

    fn open_locked(file: File, cfg: &Config, want_slots: u64) -> Result<Cache, OpenError> {
        let alone = match cfg.mode {
            Mode::Shared => {
                if flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
                    true
                } else if flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
                    false
                } else {
                    // A private process holds the file.
                    return Err(OpenError::Locked);
                }
            }
            _ => true,
        };
        let size = file.metadata()?.len() as usize;
        let header = read_header(&file, size);
        let boot = boot_id();
        if !alone {
            // Other processes map the file: it must be good, and its size wins.
            let Some(h) = header else {
                return Err(OpenError::BadShared);
            };
            let map = map(Some(&file), file_len(h.ring, h.slots))?;
            return Ok(Cache::from_parts(
                map,
                Some(file),
                h.ring,
                h.slots,
                cfg,
                Opened::Joined,
            ));
        }
        let mut reason = None;
        match &header {
            None if size == 0 => {}
            None => reason = Some("bad header or bad length"),
            Some(h) if h.ring != cfg.ring => reason = Some("another size"),
            Some(h) if h.boot != boot && h.clean != 1 && !no_boot_rule() => {
                reason = Some("the machine restarted and the file was not closed cleanly")
            }
            Some(h) if h.clean == 1 && !h.state_ok => reason = Some("bad state checksum"),
            Some(_) => {}
        }
        let fresh = header.is_none() || reason.is_some();
        if fresh {
            file.set_len(0)?;
            file.set_len(file_len(cfg.ring, want_slots) as u64)?;
        }
        let map = map(Some(&file), file_len(cfg.ring, want_slots))?;
        let opened = match (fresh, reason) {
            (true, Some(r)) => Opened::Dropped(r),
            (true, None) => Opened::Created,
            _ => Opened::Reused,
        };
        let c = Cache::from_parts(map, Some(file), cfg.ring, want_slots, cfg, opened);
        if fresh {
            c.init_header();
        }
        c.write_bytes(H_BOOT, &boot);
        c.word(H_CLEAN).store(0, Ordering::Release);
        if cfg.mode == Mode::Shared {
            flock(c.file.as_ref().unwrap(), libc::LOCK_SH);
        }
        Ok(c)
    }

    fn from_parts(
        map: Mapping,
        file: Option<File>,
        ring: u64,
        slots: u64,
        cfg: &Config,
        opened: Opened,
    ) -> Cache {
        let index_off = HEADER;
        let ring_off = HEADER + slots as usize * SLOT;
        Cache {
            map,
            file,
            ring,
            slots,
            index_off,
            ring_off,
            mode: cfg.mode,
            wlock: Mutex::new(()),
            compress_min: cfg.compress_min,
            level: cfg.level,
            opened,
            stats: Stats::default(),
        }
    }

    fn init_header(&self) {
        self.write_bytes(0, MAGIC);
        self.word(H_RING).store(self.ring, Ordering::Relaxed);
        self.word(H_SLOTS).store(self.slots, Ordering::Relaxed);
        let sum = xxh3_64(self.bytes(0, H_STATIC_SUM));
        self.word(H_STATIC_SUM).store(sum, Ordering::Relaxed);
        self.word(H_APPLIED).store(0, Ordering::Relaxed);
        // Positions start at one ring, so 0 means "no entry".
        self.word(H_HEAD).store(self.ring, Ordering::Release);
    }

    fn word(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off % 8 == 0 && off + 8 <= self.map.len);
        unsafe { &*(self.map.ptr.add(off) as *const AtomicU64) }
    }

    fn bytes(&self, off: usize, len: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map.ptr.add(off), len) }
    }

    fn write_bytes(&self, off: usize, data: &[u8]) {
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.map.ptr.add(off), data.len());
        }
    }

    fn phys(&self, pos: u64) -> usize {
        self.ring_off + (pos % self.ring) as usize
    }

    pub fn applied(&self) -> u64 {
        self.word(H_APPLIED).load(Ordering::Acquire)
    }

    fn head(&self) -> u64 {
        self.word(H_HEAD).load(Ordering::Acquire)
    }

    fn slot_key(&self, i: u64) -> &AtomicU64 {
        self.word(self.index_off + i as usize * SLOT)
    }

    fn slot_pos(&self, i: u64) -> &AtomicU64 {
        self.word(self.index_off + i as usize * SLOT + 8)
    }

    fn hash(&self, oid: u64) -> u64 {
        oid.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) & (self.slots - 1)
    }

    /// The bytes of the entry at `pos` are intact while the head of the ring
    /// stays within one ring of `pos`.
    fn intact(&self, pos: u64) -> bool {
        fence(Ordering::Acquire);
        let h = self.word(H_HEAD).load(Ordering::Relaxed);
        pos != 0 && pos < h && h <= pos + self.ring
    }

    /// Read and check the head of the entry at `pos`.
    pub fn entry_head(&self, pos: u64) -> Option<EntryHead> {
        if !self.intact(pos) {
            return None;
        }
        let off = self.phys(pos);
        if off + ENTRY_HEAD > self.map.len {
            return None;
        }
        let mut b = [0u8; ENTRY_HEAD];
        unsafe { std::ptr::copy_nonoverlapping(self.map.ptr.add(off), b.as_mut_ptr(), ENTRY_HEAD) };
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let e = EntryHead {
            pos: u64_at(E_POS),
            oid: u64_at(E_OID),
            start: u64_at(E_START),
            end: u64_at(E_END),
            prev: u64_at(E_PREV),
            len: u32_at(E_LEN),
            raw: u32_at(E_RAW),
            codec: b[E_CODEC],
            total: u32_at(E_TOTAL),
            dsum: u64_at(E_DSUM),
            bytes: b,
        };
        let hsum = u64_at(E_HSUM);
        if e.pos != pos
            || e.total as usize != align8(ENTRY_HEAD + e.len as usize)
            || (pos % self.ring) as usize + e.total as usize > self.ring as usize
            || hsum != head_sum(&b)
        {
            self.stats.bad.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(e)
    }

    fn find_slot(&self, oid: u64) -> Option<(u64, u64)> {
        let key = oid.wrapping_add(1);
        let mut i = self.hash(oid);
        for _ in 0..MAX_PROBE {
            let k = self.slot_key(i).load(Ordering::Acquire);
            if k == 0 {
                return None;
            }
            if k == key {
                return Some((i, self.slot_pos(i).load(Ordering::Acquire)));
            }
            i = (i + 1) & (self.slots - 1);
        }
        None
    }

    /// The entries of `oid`, newest insert first, at most `MAX_CHAIN`.
    fn chain(&self, oid: u64) -> impl Iterator<Item = EntryHead> + '_ {
        let mut pos = self.find_slot(oid).map(|(_, p)| p).unwrap_or(0);
        let mut n = 0;
        std::iter::from_fn(move || {
            if pos == 0 || n == MAX_CHAIN {
                return None;
            }
            n += 1;
            let e = self.entry_head(pos)?;
            if e.oid != oid {
                return None;
            }
            pos = e.prev;
            Some(e)
        })
    }

    /// Find the entry of `oid` for the snapshots before `before`. `out` gets
    /// the data. A current entry serves only snapshots up to `applied`.
    pub fn load_before_with<R>(
        &self,
        oid: u64,
        before: u64,
        out: impl FnOnce(usize, &mut dyn FnMut(&mut [u8]) -> bool) -> Option<R>,
    ) -> Option<(R, Hit)> {
        let applied = self.applied();
        let mut found = None;
        for e in self.chain(oid) {
            if e.start >= before {
                continue;
            }
            let end = e.end;
            let valid = if end == 0 {
                before <= applied.saturating_add(1)
            } else {
                end != e.start && end >= before
            };
            if valid {
                found = Some((e, end));
                break;
            }
        }
        let Some((e, end)) = found else {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let src = self.phys(e.pos) + ENTRY_HEAD;
        let mut fill = |buf: &mut [u8]| -> bool { self.copy_data(&e, src, buf) };
        match out(e.raw as usize, &mut fill) {
            Some(r) => {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                if end == 0 {
                    self.maybe_rearrange(&e);
                }
                Some((
                    r,
                    Hit {
                        start: e.start,
                        end,
                    },
                ))
            }
            None => {
                self.stats.bad.fetch_add(1, Ordering::Relaxed);
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Copy (and decompress) the data of `e` into `buf`, then check it.
    fn copy_data(&self, e: &EntryHead, src: usize, buf: &mut [u8]) -> bool {
        if buf.len() != e.raw as usize {
            return false;
        }
        let ok = if e.codec == CODEC_RAW {
            unsafe {
                std::ptr::copy_nonoverlapping(self.map.ptr.add(src), buf.as_mut_ptr(), buf.len())
            };
            xxh3_64(buf) == e.dsum
        } else {
            SCRATCH.with(|s| {
                let mut s = s.borrow_mut();
                s.clear();
                s.extend_from_slice(self.bytes(src, e.len as usize));
                if xxh3_64(&s) != e.dsum {
                    return false;
                }
                DCTX.with(|d| {
                    let mut d = d.borrow_mut();
                    let d = d.get_or_insert_with(|| zstd::bulk::Decompressor::new().unwrap());
                    matches!(d.decompress_to_buffer(&s[..], buf), Ok(n) if n == buf.len())
                })
            })
        };
        ok && self.intact(e.pos)
    }

    pub fn load_before(&self, oid: u64, before: u64) -> Option<(Vec<u8>, Hit)> {
        self.load_before_with(oid, before, |n, fill| {
            let mut v = vec![0u8; n];
            if fill(&mut v) { Some(v) } else { None }
        })
    }

    pub fn lock(&self) -> WriteGuard<'_> {
        let inner = self.wlock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = &self.file
            && self.mode == Mode::Shared
        {
            ofd(f, libc::F_OFD_SETLKW, libc::F_WRLCK as libc::c_short);
        }
        write_flag(1);
        WriteGuard {
            cache: self,
            _inner: inner,
        }
    }

    fn try_lock(&self) -> Option<WriteGuard<'_>> {
        let inner = self.wlock.try_lock().ok()?;
        if let Some(f) = &self.file
            && self.mode == Mode::Shared
            && !ofd(f, libc::F_OFD_SETLK, libc::F_WRLCK as libc::c_short)
        {
            return None;
        }
        write_flag(1);
        Some(WriteGuard {
            cache: self,
            _inner: inner,
        })
    }

    /// Write an entry at the head of the ring. The caller holds the lock.
    fn append(&self, oid: u64, start: u64, end: u64, prev: u64, data: &[u8]) -> Option<u64> {
        let (codec, stored): (u8, std::borrow::Cow<[u8]>) = match self.compress_min {
            Some(min) if data.len() >= min => {
                let z = zstd::bulk::compress(data, self.level).ok()?;
                if z.len() + z.len() / 8 < data.len() {
                    (CODEC_ZSTD, z.into())
                } else {
                    (CODEC_RAW, data.into())
                }
            }
            _ => (CODEC_RAW, data.into()),
        };
        let total = align8(ENTRY_HEAD + stored.len());
        if total as u64 > self.ring / 2 {
            return None;
        }
        let h = self.word(H_HEAD).load(Ordering::Relaxed);
        let mut pos = h;
        if (pos % self.ring) + total as u64 > self.ring {
            pos = (pos / self.ring + 1) * self.ring;
        }
        // Reserve first: a reader of an older entry in this space then sees
        // that the ring moved past it.
        self.word(H_HEAD).store(pos + total as u64, Ordering::Relaxed);
        fence(Ordering::Release);
        let mut b = [0u8; ENTRY_HEAD];
        b[E_POS..E_POS + 8].copy_from_slice(&pos.to_le_bytes());
        b[E_OID..E_OID + 8].copy_from_slice(&oid.to_le_bytes());
        b[E_START..E_START + 8].copy_from_slice(&start.to_le_bytes());
        b[E_END..E_END + 8].copy_from_slice(&end.to_le_bytes());
        b[E_PREV..E_PREV + 8].copy_from_slice(&prev.to_le_bytes());
        b[E_LEN..E_LEN + 4].copy_from_slice(&(stored.len() as u32).to_le_bytes());
        b[E_RAW..E_RAW + 4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        b[E_CODEC] = codec;
        b[E_TOTAL..E_TOTAL + 4].copy_from_slice(&(total as u32).to_le_bytes());
        b[E_DSUM..E_DSUM + 8].copy_from_slice(&xxh3_64(&stored).to_le_bytes());
        let hsum = head_sum(&b);
        b[E_HSUM..E_HSUM + 8].copy_from_slice(&hsum.to_le_bytes());
        let off = self.phys(pos);
        self.write_bytes(off, &b);
        self.write_bytes(off + ENTRY_HEAD, &stored);
        self.stats.stores.fetch_add(1, Ordering::Relaxed);
        Some(pos)
    }

    /// Point the index at `pos` for `oid`. The caller holds the lock.
    fn link(&self, oid: u64, pos: u64) {
        let key = oid.wrapping_add(1);
        let mut i = self.hash(oid);
        let mut reuse = None;
        for _ in 0..MAX_PROBE {
            let k = self.slot_key(i).load(Ordering::Acquire);
            if k == key {
                reuse = Some(i);
                break;
            }
            if k == 0 {
                reuse.get_or_insert(i);
                break;
            }
            if reuse.is_none() {
                let p = self.slot_pos(i).load(Ordering::Acquire);
                let live = self.entry_head(p).is_some_and(|e| e.oid == k.wrapping_sub(1));
                if !live {
                    reuse = Some(i);
                }
            }
            i = (i + 1) & (self.slots - 1);
        }
        if let Some(i) = reuse {
            self.slot_pos(i).store(pos, Ordering::Release);
            self.slot_key(i).store(key, Ordering::Release);
        }
    }

    /// Store an entry that a load from the server gave. A current entry
    /// (`end == 0`) comes from a reply that is current up to the tid `seen`.
    /// The caller holds the lock. Returns whether the cache took it.
    pub fn store_locked(&self, oid: u64, start: u64, end: u64, data: &[u8], seen: u64) -> bool {
        if end == 0 && self.applied() > seen && !no_seen_rule() {
            // The cache applied invalidations that this reply may not show.
            self.stats.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let mut prev = 0;
        let mut first = true;
        let mut newer_current = false;
        for e in self.chain(oid) {
            if first {
                prev = e.pos;
                first = false;
            }
            if e.start == start {
                return false;
            }
            if end == 0 && e.end == 0 {
                if e.start < start {
                    self.set_end(&e, start);
                } else {
                    newer_current = true;
                }
            }
        }
        if newer_current {
            return false;
        }
        match self.append(oid, start, end, prev, data) {
            Some(pos) => {
                self.link(oid, pos);
                true
            }
            None => false,
        }
    }

    pub fn store(&self, oid: u64, start: u64, end: u64, data: &[u8], seen: u64) -> bool {
        let _g = self.lock();
        self.store_locked(oid, start, end, data, seen)
    }

    /// End the current entries of `oid` at `tid`, or drop them when `tid` is
    /// `None`. The caller holds the lock.
    pub fn invalidate_locked(&self, oid: u64, tid: Option<u64>) {
        for e in self.chain(oid) {
            if e.end == 0 {
                let new = match tid {
                    Some(t) if t == e.start => continue,
                    Some(t) => t,
                    None => e.start,
                };
                self.set_end(&e, new);
            }
        }
    }

    /// Change the end tid of an entry, then its head checksum. A reader that
    /// sees one without the other finds a bad checksum and misses. The caller
    /// holds the lock.
    fn set_end(&self, e: &EntryHead, end: u64) {
        let mut b = e.bytes;
        b[E_END..E_END + 8].copy_from_slice(&end.to_le_bytes());
        let off = self.phys(e.pos);
        self.word(off + E_END).store(end, Ordering::Relaxed);
        self.word(off + E_HSUM).store(head_sum(&b), Ordering::Release);
    }

    pub fn set_applied_locked(&self, tid: u64) {
        self.word(H_APPLIED).store(tid, Ordering::Release);
    }

    /// Drop every entry: move the head one ring forward. `applied` stays.
    pub fn clear_locked(&self) {
        let h = self.word(H_HEAD).load(Ordering::Relaxed);
        let next = (h / self.ring + 2) * self.ring;
        self.word(H_HEAD).store(next, Ordering::Release);
    }

    pub fn clear(&self) {
        let _g = self.lock();
        self.clear_locked();
    }

    /// Copy a current entry forward when it is in the oldest fifth of the
    /// ring, as ZEO's `rearrange` does. It never waits for the lock.
    fn maybe_rearrange(&self, e: &EntryHead) {
        let h = self.head();
        if e.pos + self.ring / 5 * 4 > h || e.total as u64 > self.ring / 10 {
            return;
        }
        let Some(_g) = self.try_lock() else { return };
        let Some(e2) = self.entry_head(e.pos) else {
            return;
        };
        if e2.end != 0 {
            return;
        }
        let Some((_, head_pos)) = self.find_slot(e.oid) else {
            return;
        };
        if head_pos != e.pos {
            return;
        }
        let mut raw = vec![0u8; e2.raw as usize];
        if !self.copy_data(&e2, self.phys(e2.pos) + ENTRY_HEAD, &mut raw) {
            return;
        }
        if let Some(pos) = self.append(e2.oid, e2.start, 0, e2.prev, &raw) {
            self.link(e2.oid, pos);
            self.stats.rearranged.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The current entries: `(oid, start)`.
    pub fn contents(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for i in 0..self.slots {
            let k = self.slot_key(i).load(Ordering::Acquire);
            if k == 0 {
                continue;
            }
            let oid = k.wrapping_sub(1);
            for e in self.chain(oid) {
                if e.end == 0 {
                    out.push((oid, e.start));
                    break;
                }
            }
        }
        out
    }

    /// The number of entries that the index reaches.
    pub fn len(&self) -> usize {
        let mut n = 0;
        for i in 0..self.slots {
            let k = self.slot_key(i).load(Ordering::Acquire);
            if k != 0 {
                n += self.chain(k.wrapping_sub(1)).count();
            }
        }
        n
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Close: the last process of the file syncs it and marks it clean.
    pub fn close(&self) {
        let Some(f) = &self.file else { return };
        let _g = self.lock();
        let last = match self.mode {
            Mode::Shared => flock(f, libc::LOCK_EX | libc::LOCK_NB),
            _ => true,
        };
        if last {
            unsafe {
                libc::msync(self.map.ptr as *mut libc::c_void, self.map.len, libc::MS_SYNC);
            }
            self.word(H_CLEAN).store(1, Ordering::Release);
            let sum = self.state_sum();
            self.word(H_STATE_SUM).store(sum, Ordering::Release);
            unsafe {
                libc::msync(self.map.ptr as *mut libc::c_void, HEADER, libc::MS_SYNC);
            }
        }
        // Release the file, as ZEO closes its lock file. The caller uses the
        // cache no more.
        flock(f, libc::LOCK_UN);
    }

    /// The checksum of the words that change: clean, applied, head. A clean
    /// close writes it; an open of a clean file checks it.
    fn state_sum(&self) -> u64 {
        xxh3_64(self.bytes(H_CLEAN, H_STATE_SUM - H_CLEAN))
    }

    /// The raw file offset of the ring, for the tests that damage a file.
    pub fn layout(&self) -> (usize, usize, u64) {
        (self.index_off, self.ring_off, self.slots)
    }
}

fn head_sum(b: &[u8; ENTRY_HEAD]) -> u64 {
    xxh3_64(&b[..E_HSUM])
}

struct HeaderInfo {
    ring: u64,
    slots: u64,
    boot: [u8; 16],
    clean: u64,
    state_ok: bool,
}

fn read_header(f: &File, size: usize) -> Option<HeaderInfo> {
    use std::os::unix::fs::FileExt;
    if size < HEADER {
        return None;
    }
    let mut b = [0u8; 80];
    f.read_exact_at(&mut b, 0).ok()?;
    let u = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    if &b[..8] != MAGIC || u(H_STATIC_SUM) != xxh3_64(&b[..H_STATIC_SUM]) {
        return None;
    }
    let (ring, slots) = (u(H_RING), u(H_SLOTS));
    if !slots.is_power_of_two() || size != file_len(ring, slots) {
        return None;
    }
    Some(HeaderInfo {
        ring,
        slots,
        boot: b[H_BOOT..H_BOOT + 16].try_into().unwrap(),
        clean: u(H_CLEAN),
        state_ok: u(H_STATE_SUM) == xxh3_64(&b[H_CLEAN..H_STATE_SUM]),
    })
}

/// The view of one process: the tid up to which it read its stream, and the
/// invalidations and stores of the transaction that it reads now.
pub struct Session {
    pub cache: Cache,
    local: Mutex<Local>,
}

#[derive(Default)]
struct Local {
    last: u64,
    pending: Vec<Pending>,
}

enum Pending {
    Invalidate(u64, Option<u64>),
    Store(u64, u64, Vec<u8>),
}

/// What `set_last_tid` did with the shared file.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// This process applied the transaction.
    Applied,
    /// Another process applied it first.
    Skipped,
    /// The cache missed transactions: it dropped every entry.
    Dropped,
}

impl Session {
    pub fn new(cache: Cache) -> Session {
        let last = cache.applied();
        Session {
            cache,
            local: Mutex::new(Local {
                last,
                pending: Vec::new(),
            }),
        }
    }

    pub fn last_tid(&self) -> u64 {
        self.local.lock().unwrap().last
    }

    /// Start the stream of a process at `tid` (its `register`), after its
    /// verification.
    pub fn start_at(&self, tid: u64) {
        self.local.lock().unwrap().last = tid;
    }

    pub fn load_before(&self, oid: u64, before: u64) -> Option<(Vec<u8>, Hit)> {
        self.cache.load_before(oid, before)
    }

    /// ZEO's `invalidate`. It waits for `set_last_tid`.
    pub fn invalidate(&self, oid: u64, tid: Option<u64>) {
        self.local
            .lock()
            .unwrap()
            .pending
            .push(Pending::Invalidate(oid, tid));
    }

    /// ZEO's `store`. The data of a commit of the process (after an
    /// `invalidate` at the same tid) waits for `set_last_tid`; the data of a
    /// load goes in at once, current up to the stream of the process.
    pub fn store(&self, oid: u64, start: u64, end: u64, data: &[u8]) -> bool {
        let mut l = self.local.lock().unwrap();
        let of_commit = end == 0
            && l.pending
                .iter()
                .any(|p| matches!(p, Pending::Invalidate(o, Some(t)) if *o == oid && *t == start));
        if of_commit {
            l.pending.push(Pending::Store(oid, start, data.to_vec()));
            return true;
        }
        let seen = l.last;
        drop(l);
        self.cache.store(oid, start, end, data, seen)
    }

    /// Store the reply of a load with `seen`, the tid up to which the
    /// process had read its stream when the reply arrived. A store that runs
    /// later must not take the tid of that later time.
    pub fn store_seen(&self, oid: u64, start: u64, end: u64, data: &[u8], seen: u64) -> bool {
        self.cache.store(oid, start, end, data, seen)
    }

    /// The verification found a cache too old: drop every entry and move
    /// `applied` to `tid` in one hold of the writer lock. Between two holds,
    /// another process could store an entry that is current only up to the
    /// old `applied`.
    pub fn clear_and_apply(&self, tid: u64) {
        let mut l = self.local.lock().unwrap();
        l.pending.clear();
        let c = &self.cache;
        let g = c.lock();
        c.clear_locked();
        if c.applied() < tid {
            c.set_applied_locked(tid);
        }
        drop(g);
        l.last = tid.max(l.last);
    }

    /// ZEO's `setLastTid`: apply the pending transaction once per node, then
    /// move the stream of the process.
    pub fn set_last_tid(&self, tid: u64) -> Applied {
        let mut l = self.local.lock().unwrap();
        let pending = std::mem::take(&mut l.pending);
        let prev = l.last;
        let c = &self.cache;
        let g = c.lock();
        let applied = c.applied();
        let outcome = if applied >= tid {
            if applied == tid {
                for p in &pending {
                    if let Pending::Store(oid, start, data) = p {
                        c.store_locked(*oid, *start, 0, data, tid);
                    }
                }
            }
            c.stats.skipped.fetch_add(1, Ordering::Relaxed);
            Applied::Skipped
        } else {
            let outcome = if applied < prev {
                c.clear_locked();
                Applied::Dropped
            } else {
                for p in &pending {
                    if let Pending::Invalidate(oid, t) = p {
                        c.invalidate_locked(*oid, *t);
                    }
                }
                Applied::Applied
            };
            for p in &pending {
                if let Pending::Store(oid, start, data) = p {
                    // Not refused: `applied` is still below `tid`.
                    c.store_locked(*oid, *start, 0, data, tid);
                }
            }
            c.set_applied_locked(tid);
            c.stats.applied.fetch_add(1, Ordering::Relaxed);
            outcome
        };
        drop(g);
        l.last = tid.max(l.last);
        outcome
    }

    pub fn clear(&self) {
        self.cache.clear();
    }
}
