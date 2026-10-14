//! Checks of the prototype against a model of the server. Not for merge.
//!
//! `sim run`: worker processes share one cache file. A history of
//! transactions comes out at a fixed rate. Each worker reads its own stream
//! of invalidations, applies it to the cache once per node, commits the
//! transactions that it authors, and reads at its snapshot. One worker lags.
//! The parent kills a worker with SIGKILL at intervals and starts a new one.
//! Every hit is checked against the history: a stale or wrong answer counts.
//!
//! `sim damage`: one process fills a cache file, closes it, then the test
//! flips bytes, cuts the file short, and mixes the pages of two images of the
//! file as a power loss does. Every open and every read is checked.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cachex::cache::{Applied, Cache, Config, Mode, OpenError, Session};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The history: the oids of each tid (tid 1 writes every oid), and the tids
/// of each oid.
struct History {
    txs: Vec<Vec<u64>>,
    revs: Vec<Vec<u64>>,
}

impl History {
    fn new(oids: u64, n: u64, seed: u64) -> History {
        let mut rng = Rng(seed);
        let mut txs = vec![Vec::new(), (0..oids).collect()];
        let mut revs: Vec<Vec<u64>> = (0..oids).map(|_| vec![1]).collect();
        for tid in 2..=n {
            let k = 1 + rng.below(6);
            let mut t = Vec::new();
            for _ in 0..k {
                // Skewed: a hot set changes more often.
                let oid = if rng.below(4) == 0 {
                    rng.below(oids)
                } else {
                    rng.below(oids / 20 + 1)
                };
                if !t.contains(&oid) {
                    t.push(oid);
                    revs[oid as usize].push(tid);
                }
            }
            txs.push(t);
        }
        History { txs, revs }
    }

    /// The revision for snapshots before `before`, and the next one.
    fn truth(&self, oid: u64, before: u64) -> (u64, u64) {
        let r = &self.revs[oid as usize];
        let i = r.partition_point(|&t| t < before);
        let start = if i == 0 { 0 } else { r[i - 1] };
        let next = r.get(i).copied().unwrap_or(0);
        (start, next)
    }

    /// The reply of the server to a load at snapshot `before - 1`, when the
    /// stream of the process is at `upto`: the end is known only up to it.
    fn reply(&self, oid: u64, before: u64, upto: u64) -> (u64, u64) {
        let (start, next) = self.truth(oid, before);
        let end = if next != 0 && next <= upto { next } else { 0 };
        (start, end)
    }
}

fn data(oid: u64, tid: u64) -> Vec<u8> {
    let mut r = Rng(oid.wrapping_mul(31).wrapping_add(tid));
    let len = 40 + r.below(1500) as usize;
    let word = r.next().to_le_bytes();
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        v.extend_from_slice(&word);
        if r.below(3) == 0 {
            v.extend_from_slice(&r.next().to_le_bytes());
        }
    }
    v.truncate(len);
    v
}

/// Counters in a shared anonymous mapping.
#[repr(C)]
struct Shared {
    published: AtomicU64,
    stop: AtomicU64,
    hits: AtomicU64,
    current_hits: AtomicU64,
    misses: AtomicU64,
    stale: AtomicU64,
    wrong_data: AtomicU64,
    reads: AtomicU64,
    stored: AtomicU64,
    applied: AtomicU64,
    skipped: AtomicU64,
    dropped: AtomicU64,
    clears: AtomicU64,
    opens: AtomicU64,
    bad_open: AtomicU64,
    refused: AtomicU64,
    lag_max: AtomicU64,
    verify_equal: AtomicU64,
    verify_quick: AtomicU64,
    verify_too_old: AtomicU64,
    verify_ahead: AtomicU64,
    delayed_stores: AtomicU64,
    bad_entries: AtomicU64,
    kills_in_lock: AtomicU64,
    /// One word per slot: 1 while the worker of the slot holds the writer lock.
    in_lock: [AtomicU64; 64],
}

fn shared() -> &'static Shared {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<Shared>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(p != libc::MAP_FAILED);
    unsafe { &*(p as *const Shared) }
}

fn cfg(ring: u64, mode: Mode) -> Config {
    Config {
        ring,
        mode,
        compress_min: Some(256),
        level: 1,
    }
}

/// Check a hit against the history.
fn check(h: &History, oid: u64, before: u64, d: &[u8], start: u64, end: u64, sh: &Shared) -> bool {
    let (t_start, t_next) = h.truth(oid, before);
    let ok = start == t_start && (end == 0 || end == t_next);
    if !ok {
        sh.stale.fetch_add(1, Ordering::Relaxed);
        if sh.stale.load(Ordering::Relaxed) < 5 {
            eprintln!(
                "STALE oid {oid} before {before}: got [{start}, {end}) want [{t_start}, {t_next})"
            );
        }
        return false;
    }
    if d != data(oid, start) {
        sh.wrong_data.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

struct RunArgs {
    path: PathBuf,
    workers: u64,
    oids: u64,
    txs: u64,
    tick_us: u64,
    kill_ms: u64,
    ring: u64,
    seed: u64,
    too_old: u64,
    kill_in_lock: bool,
}

fn worker(slot: u64, a: &RunArgs, h: &History, sh: &Shared) {
    let laggard = slot == 0;
    let mut rng = Rng(a.seed ^ (slot << 32) ^ std::process::id() as u64);
    let cache = match Cache::open(Some(&a.path), &cfg(a.ring, Mode::Shared)) {
        Ok(c) => c,
        Err(OpenError::BadShared) | Err(OpenError::Locked) | Err(OpenError::Io(_)) => {
            sh.bad_open.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    sh.opens.fetch_add(1, Ordering::Relaxed);
    cachex::cache::WRITE_FLAG.store(
        &sh.in_lock[slot as usize] as *const AtomicU64 as *mut AtomicU64,
        Ordering::Relaxed,
    );
    let s = Session::new(cache);
    // Verification at the open: `register` gives `r`. Half of the opens read
    // `applied` first, as a client that opens its file before it connects;
    // the other half read it a moment after `register`, so `a0 > r` happens.
    let (r, a0) = if rng.below(2) == 0 {
        let a0 = s.cache.applied();
        (sh.published.load(Ordering::Acquire), a0)
    } else {
        let r = sh.published.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(2));
        (r, s.cache.applied())
    };
    if a0 < r {
        if a0 == 0 || r - a0 > a.too_old {
            // getInvalidations answers None: the cache is too old. The clear
            // and the move of `applied` are one hold of the writer lock.
            sh.verify_too_old.fetch_add(1, Ordering::Relaxed);
            if std::env::var_os("CACHEX_TWO_HOLDS").is_some() {
                s.clear();
                s.set_last_tid(r);
            } else {
                s.clear_and_apply(r);
            }
        } else {
            sh.verify_quick.fetch_add(1, Ordering::Relaxed);
            for t in a0 + 1..=r {
                for &oid in &h.txs[t as usize] {
                    s.invalidate(oid, None);
                }
            }
            s.set_last_tid(r);
        }
    } else {
        // `a0 >= r`: up to date, or another process is ahead. The model has
        // one history, so `lastTransaction` is at least `a0`.
        if a0 == r {
            sh.verify_equal.fetch_add(1, Ordering::Relaxed);
        } else {
            sh.verify_ahead.fetch_add(1, Ordering::Relaxed);
        }
        s.start_at(r);
    }
    let mut bad_seen = 0;
    let mut x = r;
    let nslots = a.workers + 3;
    while sh.stop.load(Ordering::Relaxed) == 0 {
        let g = sh.published.load(Ordering::Acquire);
        let steps = if laggard {
            u64::from(rng.below(40) == 0)
        } else {
            4
        };
        for _ in 0..steps {
            if x >= g {
                break;
            }
            let t = x + 1;
            let mine = t % nslots == slot;
            for &oid in &h.txs[t as usize] {
                s.invalidate(oid, Some(t));
                if mine {
                    s.store(oid, t, 0, &data(oid, t));
                }
            }
            match s.set_last_tid(t) {
                Applied::Applied => sh.applied.fetch_add(1, Ordering::Relaxed),
                Applied::Skipped => sh.skipped.fetch_add(1, Ordering::Relaxed),
                Applied::Dropped => sh.dropped.fetch_add(1, Ordering::Relaxed),
            };
            x = t;
        }
        sh.lag_max.fetch_max(g.saturating_sub(x), Ordering::Relaxed);
        for _ in 0..8 {
            let back = rng.below(x.min(30));
            let snap = x - back;
            let before = snap + 1;
            let oid = if rng.below(3) == 0 {
                rng.below(a.oids)
            } else {
                rng.below(a.oids / 20 + 1)
            };
            sh.reads.fetch_add(1, Ordering::Relaxed);
            match s.load_before(oid, before) {
                Some((d, hit)) => {
                    sh.hits.fetch_add(1, Ordering::Relaxed);
                    if hit.end == 0 {
                        sh.current_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    check(h, oid, before, &d, hit.start, hit.end, sh);
                }
                None => {
                    sh.misses.fetch_add(1, Ordering::Relaxed);
                    let (start, end) = h.reply(oid, before, x);
                    let seen = x;
                    // One reply in four is stored late, after the process
                    // read more of its stream, as a client whose reply goes
                    // to another task. The store keeps the tid of the reply.
                    let late = !laggard && rng.below(4) == 0;
                    if late {
                        let g = sh.published.load(Ordering::Acquire);
                        for _ in 0..4 {
                            if x >= g {
                                break;
                            }
                            let t = x + 1;
                            let mine = t % nslots == slot;
                            for &o in &h.txs[t as usize] {
                                s.invalidate(o, Some(t));
                                if mine {
                                    s.store(o, t, 0, &data(o, t));
                                }
                            }
                            match s.set_last_tid(t) {
                                Applied::Applied => sh.applied.fetch_add(1, Ordering::Relaxed),
                                Applied::Skipped => sh.skipped.fetch_add(1, Ordering::Relaxed),
                                Applied::Dropped => sh.dropped.fetch_add(1, Ordering::Relaxed),
                            };
                            x = t;
                        }
                        sh.delayed_stores.fetch_add(1, Ordering::Relaxed);
                    }
                    let stored = if late && std::env::var_os("CACHEX_SEEN_AT_STORE").is_some() {
                        s.store(oid, start, end, &data(oid, start))
                    } else {
                        s.store_seen(oid, start, end, &data(oid, start), seen)
                    };
                    if stored {
                        sh.stored.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let bad_now = s.cache.stats.bad.load(Ordering::Relaxed);
        sh.bad_entries.fetch_add(bad_now - bad_seen, Ordering::Relaxed);
        bad_seen = bad_now;
        if rng.below(30_000) == 0 {
            s.clear();
            sh.clears.fetch_add(1, Ordering::Relaxed);
        }
        if laggard {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    sh.refused
        .fetch_add(s.cache.stats.refused.load(Ordering::Relaxed), Ordering::Relaxed);
    s.cache.close();
}

fn spawn(slot: u64, a: &RunArgs, h: &History, sh: &Shared) -> libc::pid_t {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        worker(slot, a, h, sh);
        unsafe { libc::_exit(0) };
    }
    pid
}

fn run(a: RunArgs) {
    let _ = std::fs::remove_file(&a.path);
    let h = History::new(a.oids, a.txs, a.seed);
    let sh = shared();
    sh.published.store(1, Ordering::Release);
    let mut pids: Vec<libc::pid_t> = (0..a.workers).map(|s| spawn(s, &a, &h, sh)).collect();
    let mut rng = Rng(a.seed);
    let t0 = Instant::now();
    let mut last_kill = Instant::now();
    let mut kills = 0u64;
    let mut g = 1;
    while g < a.txs {
        std::thread::sleep(Duration::from_micros(a.tick_us));
        g += 1;
        sh.published.store(g, Ordering::Release);
        if a.kill_ms > 0 && last_kill.elapsed() >= Duration::from_millis(a.kill_ms) {
            // Never the laggard: it must stay behind.
            let slot = 1 + rng.below(a.workers - 1) as usize;
            if a.kill_in_lock {
                // Wait up to 5 ms for the victim to hold the writer lock.
                let t = Instant::now();
                while sh.in_lock[slot].load(Ordering::SeqCst) == 0
                    && t.elapsed() < Duration::from_millis(5)
                {
                    std::hint::spin_loop();
                }
            }
            unsafe {
                libc::kill(pids[slot], libc::SIGKILL);
            }
            if sh.in_lock[slot].swap(0, Ordering::SeqCst) == 1 {
                sh.kills_in_lock.fetch_add(1, Ordering::Relaxed);
            }
            unsafe {
                libc::waitpid(pids[slot], std::ptr::null_mut(), 0);
            }
            kills += 1;
            pids[slot] = spawn(slot as u64, &a, &h, sh);
            last_kill = Instant::now();
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    sh.stop.store(1, Ordering::Release);
    for p in &pids {
        unsafe { libc::waitpid(*p, std::ptr::null_mut(), 0) };
    }
    // The parent reopens the file alone and checks every oid at the last
    // snapshot that the cache applied.
    let c = Cache::open(Some(&a.path), &cfg(a.ring, Mode::Private)).expect("reopen");
    let applied = c.applied();
    let (mut final_hits, mut final_bad) = (0, 0);
    for oid in 0..a.oids {
        for back in [0, 3, 30] {
            let before = applied.saturating_sub(back) + 1;
            if let Some((d, hit)) = c.load_before(oid, before) {
                final_hits += 1;
                if !check(&h, oid, before, &d, hit.start, hit.end, sh) {
                    final_bad += 1;
                }
            }
        }
    }
    let l = |x: &AtomicU64| x.load(Ordering::Relaxed);
    println!(
        "{{\"mode\":\"run\",\"seconds\":{:.1},\"workers\":{},\"published\":{},\"kills\":{},\"opens\":{},\"bad_open\":{},\"reads\":{},\"hits\":{},\"current_hits\":{},\"misses\":{},\"stored\":{},\"refused\":{},\"applied\":{},\"skipped\":{},\"dropped\":{},\"clears\":{},\"lag_max\":{},\"stale\":{},\"wrong_data\":{},\"verify_equal\":{},\"verify_quick\":{},\"verify_too_old\":{},\"verify_ahead\":{},\"delayed_stores\":{},\"bad_entries\":{},\"kills_in_lock\":{},\"reopen\":\"{:?}\",\"final_applied\":{},\"final_hits\":{},\"final_bad\":{}}}",
        t0.elapsed().as_secs_f64(),
        a.workers,
        l(&sh.published),
        kills,
        l(&sh.opens),
        l(&sh.bad_open),
        l(&sh.reads),
        l(&sh.hits),
        l(&sh.current_hits),
        l(&sh.misses),
        l(&sh.stored),
        l(&sh.refused),
        l(&sh.applied),
        l(&sh.skipped),
        l(&sh.dropped),
        l(&sh.clears),
        l(&sh.lag_max),
        l(&sh.stale),
        l(&sh.wrong_data),
        l(&sh.verify_equal),
        l(&sh.verify_quick),
        l(&sh.verify_too_old),
        l(&sh.verify_ahead),
        l(&sh.delayed_stores),
        l(&sh.bad_entries),
        l(&sh.kills_in_lock),
        c.opened,
        applied,
        final_hits,
        final_bad,
    );
}

/// Fill a private cache file from the history up to `upto`, with the loads
/// of one reader.
fn fill(s: &Session, h: &History, from: u64, upto: u64, oids: u64, rng: &mut Rng) {
    for t in from..=upto {
        if t > 1 {
            for &oid in &h.txs[t as usize] {
                s.invalidate(oid, Some(t));
            }
        }
        s.set_last_tid(t);
        for _ in 0..4 {
            let oid = rng.below(oids);
            let before = t + 1 - rng.below(t.min(20));
            if s.load_before(oid, before).is_none() {
                let (start, end) = h.reply(oid, before, t);
                s.store(oid, start, end, &data(oid, start));
            }
        }
    }
}

/// Read every oid at a few snapshots; count hits and bad answers.
fn audit(c: &Cache, h: &History, oids: u64, sh: &Shared) -> (u64, u64) {
    let applied = c.applied();
    let (mut hits, mut bad) = (0, 0);
    for oid in 0..oids {
        for back in [0, 1, 5, 40] {
            let before = applied.saturating_sub(back) + 1;
            if let Some((d, hit)) = c.load_before(oid, before) {
                hits += 1;
                if !check(h, oid, before, &d, hit.start, hit.end, sh) {
                    bad += 1;
                }
            }
        }
    }
    (hits, bad)
}

fn copy(from: &Path, to: &Path) {
    std::fs::copy(from, to).expect("copy");
}

fn damage(dir: &Path, trials: u64, seed: u64) {
    let oids = 3000;
    let ring = 1 << 20;
    let h = History::new(oids, 4000, seed);
    let sh = shared();
    let mut rng = Rng(seed);
    let base = dir.join("base.zoec");
    let before_img = dir.join("before.img");
    let after_img = dir.join("after.img");
    let _ = std::fs::remove_file(&base);
    // Two images of a file that a process has open (not clean): the second
    // after more transactions. Then a clean close.
    let s = Session::new(Cache::open(Some(&base), &cfg(ring, Mode::Private)).unwrap());
    fill(&s, &h, 1, 2500, oids, &mut rng);
    copy(&base, &before_img);
    fill(&s, &h, 2501, 3000, oids, &mut rng);
    copy(&base, &after_img);
    s.cache.close();
    drop(s);
    let work = dir.join("work.zoec");

    // 0. The clean file opens and answers.
    copy(&base, &work);
    let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
    let (clean_hits, clean_bad) = audit(&c, &h, oids, sh);
    let clean_opened = c.opened;
    drop(c);

    // 1. Byte flips anywhere in the clean file.
    let len = std::fs::metadata(&base).unwrap().len();
    let (mut f_dropped, mut f_hits, mut f_bad) = (0, 0, 0);
    for _ in 0..trials {
        copy(&base, &work);
        let mut bytes = std::fs::read(&work).unwrap();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(len) as usize;
            bytes[at] ^= 1 << rng.below(8);
        }
        std::fs::write(&work, &bytes).unwrap();
        let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
        if matches!(c.opened, cachex::cache::Opened::Dropped(_)) {
            f_dropped += 1;
        }
        let (hh, bb) = audit(&c, &h, oids, sh);
        f_hits += hh;
        f_bad += bb;
    }

    // 1b. One bit flip in the first 88 bytes of the header.
    let (mut hf_dropped, mut hf_hits, mut hf_bad) = (0, 0, 0);
    for _ in 0..trials.min(200) {
        copy(&base, &work);
        let mut bytes = std::fs::read(&work).unwrap();
        let at = rng.below(88) as usize;
        bytes[at] ^= 1 << rng.below(8);
        std::fs::write(&work, &bytes).unwrap();
        let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
        if matches!(c.opened, cachex::cache::Opened::Dropped(_)) {
            hf_dropped += 1;
        }
        let (hh, bb) = audit(&c, &h, oids, sh);
        hf_hits += hh;
        hf_bad += bb;
    }

    // 2. A file cut short.
    let (mut t_dropped, mut t_hits) = (0, 0);
    for _ in 0..trials.min(200) {
        copy(&base, &work);
        let cut = rng.below(len);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&work)
            .unwrap()
            .set_len(cut)
            .unwrap();
        let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
        if matches!(c.opened, cachex::cache::Opened::Dropped(_) | cachex::cache::Opened::Created) {
            t_dropped += 1;
        }
        t_hits += audit(&c, &h, oids, sh).0;
    }

    // 3. A power loss: pages of the two images mixed, a new boot id.
    let b = std::fs::read(&before_img).unwrap();
    let a = std::fs::read(&after_img).unwrap();
    let torn = |rng: &mut Rng| -> Vec<u8> {
        let mut out = a.clone();
        for (i, page) in out.chunks_mut(4096).enumerate() {
            if rng.below(2) == 0 {
                page.copy_from_slice(&b[i * 4096..i * 4096 + page.len()]);
            }
        }
        out
    };
    let mut p_dropped = 0;
    let mut p_hits = 0;
    unsafe { std::env::set_var("CACHEX_BOOT_ID", "00000000000000000000000000000001") };
    for _ in 0..trials.min(200) {
        std::fs::write(&work, torn(&mut rng)).unwrap();
        let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
        if matches!(c.opened, cachex::cache::Opened::Dropped(_)) {
            p_dropped += 1;
        }
        p_hits += audit(&c, &h, oids, sh).0;
    }
    // The canary: the same torn files without the boot rule.
    unsafe { std::env::set_var("CACHEX_NO_BOOT_RULE", "1") };
    let stale_before = sh.stale.load(Ordering::Relaxed);
    let (mut c_hits, mut c_bad) = (0, 0);
    for _ in 0..trials.min(200) {
        std::fs::write(&work, torn(&mut rng)).unwrap();
        let c = Cache::open(Some(&work), &cfg(ring, Mode::Private)).unwrap();
        let (hh, bb) = audit(&c, &h, oids, sh);
        c_hits += hh;
        c_bad += bb;
    }
    unsafe {
        std::env::remove_var("CACHEX_NO_BOOT_RULE");
        std::env::remove_var("CACHEX_BOOT_ID");
    }
    let _ = stale_before;
    println!(
        "{{\"mode\":\"damage\",\"trials\":{trials},\"file_bytes\":{len},\"clean_opened\":\"{clean_opened:?}\",\"clean_hits\":{clean_hits},\"clean_bad\":{clean_bad},\"flip_dropped\":{f_dropped},\"flip_hits\":{f_hits},\"flip_bad\":{f_bad},\"header_flip_trials\":{},\"header_flip_dropped\":{hf_dropped},\"header_flip_hits\":{hf_hits},\"header_flip_bad\":{hf_bad},\"cut_dropped\":{t_dropped},\"cut_hits\":{t_hits},\"power_loss_dropped\":{p_dropped},\"power_loss_hits\":{p_hits},\"canary_no_boot_rule_hits\":{c_hits},\"canary_no_boot_rule_bad\":{c_bad}}}",
        trials.min(200)
    );
}

/// Two races in a fixed order, each with the rule and without it. Oid 7 has
/// revisions at tids 1 and 5. A reader at snapshot 5 or 6 must get tid 5.
///
/// Race 1, the verification of a cache too old: a process P, whose stream is
/// at tid 3, stores the reply of a load of oid 7 (start 1, current up to 3)
/// while a joiner J clears the cache and moves `applied` to its `register`
/// tid 6. With two holds of the lock, the store can come between them.
///
/// Race 2, a reply stored late: P gets the reply (start 1, current up to 3),
/// then reads and applies the invalidation of tid 5 for oid 7, then stores
/// the reply. The rule takes the tid of the reply (3), not the tid of the
/// store (5).
fn race(dir: &Path) {
    let path = dir.join("race.zoec");
    let open = || Session::new(Cache::open(Some(&path), &cfg(1 << 20, Mode::Shared)).unwrap());
    let rev = |t: u64| format!("revision {t}").into_bytes();
    let line = |name: &str, rule: bool, stored: bool, hit: Option<u64>| {
        println!(
            "{{\"mode\":\"race\",\"race\":\"{name}\",\"rule\":{rule},\"reply_stored\":{stored},\"hit_start\":{},\"stale\":{}}}",
            hit.map_or("null".to_string(), |t| t.to_string()),
            hit == Some(1)
        );
    };
    for (name, one_hold, store_first) in [
        ("too-old verification, store between the holds", false, false),
        ("too-old verification, store before the hold", true, true),
        ("too-old verification, store after the hold", true, false),
    ] {
        let _ = std::fs::remove_file(&path);
        let p = open();
        p.set_last_tid(3);
        let j = open();
        let mut stored = false;
        if store_first {
            stored = p.store_seen(7, 1, 0, &rev(1), 3);
        }
        if one_hold {
            j.clear_and_apply(6);
        } else {
            j.clear();
        }
        if !store_first {
            stored = p.store_seen(7, 1, 0, &rev(1), 3);
        }
        if !one_hold {
            j.set_last_tid(6);
        }
        let hit = j.load_before(7, 7).map(|(_, h)| h.start);
        line(name, one_hold, stored, hit);
    }
    for seen_at_reply in [false, true] {
        let _ = std::fs::remove_file(&path);
        let p = open();
        p.set_last_tid(3);
        p.invalidate(7, Some(5));
        p.set_last_tid(5);
        let stored = if seen_at_reply {
            p.store_seen(7, 1, 0, &rev(1), 3)
        } else {
            p.store(7, 1, 0, &rev(1))
        };
        // A reader at snapshot 5, which is `applied`: the truth is tid 5.
        let hit = p.load_before(7, 6).map(|(_, h)| h.start);
        line("a reply stored after the invalidation of its oid", seen_at_reply, stored, hit);
    }
}

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().expect("a number"))
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(
        args.iter()
            .position(|a| a == "--dir")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "/tmp".into()),
    );
    std::fs::create_dir_all(&dir).unwrap();
    match args.get(1).map(String::as_str) {
        Some("run") => run(RunArgs {
            path: dir.join("shared.zoec"),
            workers: arg(&args, "--workers", 6),
            oids: arg(&args, "--oids", 5000),
            txs: arg(&args, "--txs", 40_000),
            tick_us: arg(&args, "--tick-us", 250),
            kill_ms: arg(&args, "--kill-ms", 200),
            ring: arg(&args, "--ring", 4 << 20),
            seed: arg(&args, "--seed", 1),
            too_old: arg(&args, "--too-old", 300),
            kill_in_lock: arg(&args, "--kill-in-lock", 0) == 1,
        }),
        Some("race") => race(&dir),
        Some("damage") => damage(&dir, arg(&args, "--trials", 500), arg(&args, "--seed", 1)),
        _ => eprintln!("usage: sim run|damage [--dir DIR] [options]"),
    }
}
