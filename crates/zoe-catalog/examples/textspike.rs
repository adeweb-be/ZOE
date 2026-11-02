//! The measures of the text index spike. See `spike/README.md`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use roaring::RoaringBitmap;
use tantivy::collector::{Count, FilterCollector, TopDocs};
use tantivy::{Order, Searcher};
use zoe_catalog::text::compat::{self, Compat, Node, Stats};
use zoe_catalog::text::enhanced::{Options, Translator};
use zoe_catalog::text::index::{self, Doc, INDEXES, Mode};
use zoe_catalog::text::overlay::{self, Counts, Overlay};
use zoe_core::Compression;
use zoe_core::text::{Delta, TextDeltas};

fn mode_of(s: &str) -> Mode {
    match s {
        "compat" => Mode::Compat,
        "enhanced" => Mode::Enhanced,
        _ => panic!("mode: compat or enhanced"),
    }
}

fn status(key: &str) -> String {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with(key))
        .map(|l| l[key.len()..].trim().to_owned())
        .unwrap_or_default()
}

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0)
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort();
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i].as_secs_f64() * 1000.0
}

fn lines(path: &Path) -> impl Iterator<Item = String> {
    BufReader::with_capacity(1 << 20, File::open(path).expect("open corpus"))
        .lines()
        .map(|l| l.expect("read line"))
}

fn parse(line: &str) -> (Doc, serde_json::Value) {
    // An empty text is an entry of no word, as in ZCTextIndex.
    index::parse_line(line).expect("a corpus line")
}

/// build MODE CORPUS DIR THREADS [LIMIT]
fn build(args: &[String]) {
    let mode = mode_of(&args[0]);
    let corpus = PathBuf::from(&args[1]);
    let dir = PathBuf::from(&args[2]);
    let nthreads: usize = args[3].parse().unwrap();
    let limit: usize = args.get(4).map_or(usize::MAX, |s| s.parse().unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let t0 = Instant::now();
    let th0 = threads();
    let (idx, fields) = index::create(mode, Some(&dir)).unwrap();
    let writer = idx.writer_with_num_threads(nthreads, 256_000_000 * nthreads).unwrap();
    let th_writer = threads();
    let (tx, rx) = mpsc::sync_channel::<Vec<String>>(8);
    let rx = Arc::new(Mutex::new(rx));
    let listing = Mutex::new(RoaringBitmap::new());
    let editor = Mutex::new(RoaringBitmap::new());
    let mut n = 0usize;
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            let rx = rx.clone();
            let (writer, fields, listing, editor) = (&writer, &fields, &listing, &editor);
            s.spawn(move || {
                loop {
                    let batch = match rx.lock().unwrap().recv() {
                        Ok(b) => b,
                        Err(_) => break,
                    };
                    let (mut l, mut e) = (Vec::new(), Vec::new());
                    for line in batch {
                        let (doc, v) = parse(&line);
                        let rid = doc.rid as u32;
                        if v["type"] == "Document" && v["state"] == "published" {
                            l.push(rid);
                        }
                        if v["creator"] == "editor-000" {
                            e.push(rid);
                        }
                        if doc.texts.iter().any(Option::is_some) {
                            writer.add_document(index::document(mode, fields, &doc)).unwrap();
                        }
                    }
                    listing.lock().unwrap().extend(l);
                    editor.lock().unwrap().extend(e);
                }
            });
        }
        let mut batch = Vec::with_capacity(1000);
        for line in lines(&corpus).take(limit) {
            batch.push(line);
            n += 1;
            if batch.len() == 1000 {
                tx.send(std::mem::replace(&mut batch, Vec::with_capacity(1000))).unwrap();
            }
        }
        tx.send(batch).unwrap();
        drop(tx);
    });
    let th_during = threads();
    let mut writer = writer;
    writer.commit().unwrap();
    let t_commit = t0.elapsed();
    let reader = index::reader(&idx).unwrap();
    let segs = reader.searcher().segment_readers().len();
    let ids = idx.searchable_segment_ids().unwrap();
    let t1 = Instant::now();
    if ids.len() > 1 {
        let _ = writer.merge(&ids).wait().unwrap();
    }
    writer.wait_merging_threads().unwrap();
    let t_merge = t1.elapsed();
    let mut f = File::create(dir.with_extension("listing.roar")).unwrap();
    listing.lock().unwrap().serialize_into(&mut f).unwrap();
    let mut f = File::create(dir.with_extension("editor.roar")).unwrap();
    editor.lock().unwrap().serialize_into(&mut f).unwrap();
    println!(
        "{{\"mode\":\"{:?}\",\"docs\":{n},\"threads\":{nthreads},\"build_s\":{:.1},\"segments_after_commit\":{segs},\"merge_s\":{:.1},\"size_bytes\":{},\"peak_rss\":\"{}\",\"threads_before\":{th0},\"threads_with_writer\":{th_writer},\"threads_after_feed\":{th_during},\"listing\":{},\"editor\":{}}}",
        mode,
        t_commit.as_secs_f64(),
        t_merge.as_secs_f64(),
        index::dir_size(&dir),
        status("VmHWM:"),
        listing.lock().unwrap().len(),
        editor.lock().unwrap().len(),
    );
}

fn load_bitmap(path: &Path) -> Arc<RoaringBitmap> {
    Arc::new(RoaringBitmap::deserialize_from(File::open(path).unwrap()).unwrap())
}

#[derive(Clone, Copy, Debug)]
enum Variant {
    Alone,
    Filter,
    Selective,
    Sort,
    FilterSort,
}

const VARIANTS: [Variant; 5] =
    [Variant::Alone, Variant::Filter, Variant::Selective, Variant::Sort, Variant::FilterSort];

struct Bench {
    mode: Mode,
    searcher: Searcher,
    fields: index::Fields,
    listing: Arc<RoaringBitmap>,
    editor: Arc<RoaringBitmap>,
    stats: Vec<Stats>,
    translator: Translator,
}

impl Bench {
    fn run(&mut self, index_name: &str, tree: &Node, v: Variant, limit: usize) -> usize {
        let i = INDEXES.iter().position(|n| *n == index_name).unwrap();
        let bm = match v {
            Variant::Filter | Variant::FilterSort => Some(self.listing.clone()),
            Variant::Selective => Some(self.editor.clone()),
            _ => None,
        };
        let sort = matches!(v, Variant::Sort | Variant::FilterSort);
        match self.mode {
            Mode::Compat => {
                let len_name = format!("n_{index_name}");
                let mut c = Compat::new(&self.searcher, self.fields.text[i], &len_name, self.stats[i]);
                let Some(hits) = c.run(tree).unwrap() else { return 0 };
                if sort {
                    // The mirror sorts: here, the effective date of each hit.
                    let mut all = Vec::new();
                    for (ord, h) in hits.iter().enumerate() {
                        let seg = &self.searcher.segment_readers()[ord];
                        let rids = seg.fast_fields().u64("rid").unwrap();
                        let eff = seg.fast_fields().i64("effective").unwrap();
                        for &(d, _) in h {
                            let rid = rids.first(d).unwrap_or(0);
                            if bm.as_ref().is_none_or(|b| b.contains(rid as u32)) {
                                all.push((eff.first(d).unwrap_or(0), rid));
                            }
                        }
                    }
                    let n = all.len();
                    let cmp = |a: &(i64, u64), b: &(i64, u64)| b.cmp(a);
                    if limit < all.len() {
                        all.select_nth_unstable_by(limit, cmp);
                        all.truncate(limit);
                    }
                    all.sort_by(cmp);
                    n
                } else {
                    let (n, _) = compat::rank(
                        &self.searcher,
                        &hits,
                        |rid| bm.as_ref().is_none_or(|b| b.contains(rid as u32)),
                        limit,
                    )
                    .unwrap();
                    n
                }
            }
            Mode::Enhanced => {
                let Some(q) = self.translator.translate(tree) else { return 0 };
                let keep = move |rid: u64| bm.as_ref().is_none_or(|b| b.contains(rid as u32));
                if sort {
                    let col = (
                        Count,
                        TopDocs::with_limit(limit).order_by_fast_field::<i64>("effective", Order::Desc),
                    );
                    let (n, _) = self
                        .searcher
                        .search(&q, &FilterCollector::new("rid".to_string(), keep, col))
                        .unwrap();
                    n
                } else {
                    let (n, _) = overlay::enhanced(&self.searcher, None, &*q, keep, limit).unwrap();
                    n
                }
            }
        }
    }
}

fn open_bench(mode: Mode, dir: &Path, fuzzy: u8, boosts: bool) -> Bench {
    let (idx, fields) = index::open(mode, dir).unwrap();
    let searcher = index::reader(&idx).unwrap().searcher();
    let stats = INDEXES
        .iter()
        .map(|n| Stats::count(&searcher, &format!("n_{n}")).unwrap())
        .collect();
    let opts = Options {
        fields: if boosts {
            vec![(fields.text[1], 3.0), (fields.text[2], 1.5), (fields.text[0], 1.0)]
        } else {
            vec![(fields.text[0], 1.0)]
        },
        fuzzy,
    };
    let translator = Translator::new(&idx, opts);
    Bench {
        mode,
        searcher,
        fields,
        listing: load_bitmap(&dir.with_extension("listing.roar")),
        editor: load_bitmap(&dir.with_extension("editor.roar")),
        stats,
        translator,
    }
}

/// bench MODE DIR QUERIES ROUNDS [fuzzy] [boosts]
fn bench(args: &[String]) {
    let mode = mode_of(&args[0]);
    let dir = PathBuf::from(&args[1]);
    let queries: Vec<serde_json::Value> =
        lines(Path::new(&args[2])).map(|l| serde_json::from_str(&l).unwrap()).collect();
    let rounds: usize = args[3].parse().unwrap();
    let fuzzy: u8 = args.get(4).map_or(0, |s| s.parse().unwrap());
    let boosts = args.get(5).is_some_and(|s| s == "boosts");
    let t = Instant::now();
    let mut b = open_bench(mode, &dir, fuzzy, boosts);
    let open_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut by_variant: Vec<Vec<Duration>> = vec![Vec::new(); VARIANTS.len()];
    let mut by_kind: std::collections::BTreeMap<(String, usize), Vec<Duration>> = Default::default();
    // One warm pass.
    for q in &queries {
        let tree: Node = serde_json::from_value(q["tree"].clone()).unwrap();
        for v in VARIANTS {
            b.run(q["index"].as_str().unwrap(), &tree, v, 20);
        }
    }
    let mut counts = Vec::new();
    for _ in 0..rounds {
        for q in &queries {
            let tree: Node = serde_json::from_value(q["tree"].clone()).unwrap();
            for (vi, v) in VARIANTS.iter().enumerate() {
                let t = Instant::now();
                let n = b.run(q["index"].as_str().unwrap(), &tree, *v, 20);
                let d = t.elapsed();
                by_variant[vi].push(d);
                by_kind.entry((q["kind"].as_str().unwrap().to_owned(), vi)).or_default().push(d);
                if vi == 0 {
                    counts.push(n);
                }
            }
        }
    }
    let mut out = format!(
        "{{\"mode\":\"{mode:?}\",\"fuzzy\":{fuzzy},\"boosts\":{boosts},\"queries\":{},\"rounds\":{rounds},\"open_ms\":{open_ms:.1},\"rss\":\"{}\",\"threads\":{},\"variants\":{{",
        queries.len(),
        status("VmRSS:"),
        threads()
    );
    for (vi, v) in VARIANTS.iter().enumerate() {
        let d = &mut by_variant[vi];
        out += &format!(
            "{}\"{:?}\":{{\"p50\":{:.2},\"p99\":{:.2},\"max\":{:.2}}}",
            if vi > 0 { "," } else { "" },
            v,
            pct(d, 0.5),
            pct(d, 0.99),
            pct(d, 1.0)
        );
    }
    out += "},\"kinds\":{";
    let mut first = true;
    for ((kind, vi), d) in &mut by_kind {
        out += &format!(
            "{}\"{}/{:?}\":{{\"p50\":{:.2},\"p99\":{:.2}}}",
            if first { "" } else { "," },
            kind,
            VARIANTS[*vi],
            pct(d, 0.5),
            pct(d, 0.99)
        );
        first = false;
    }
    out += "}}";
    println!("{out}");
    let total: usize = counts.iter().sum();
    eprintln!("mean count {}", total / counts.len().max(1));
}

/// compat DIR CASES
fn compat_check(args: &[String]) {
    let dir = PathBuf::from(&args[0]);
    let (idx, fields) = index::open(Mode::Compat, &dir).unwrap();
    let searcher = index::reader(&idx).unwrap().searcher();
    let stats: Vec<Stats> = INDEXES
        .iter()
        .map(|n| Stats::count(&searcher, &format!("n_{n}")).unwrap())
        .collect();
    let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
    let mut examples: Vec<String> = Vec::new();
    let mut total = 0;
    for line in lines(Path::new(&args[1])) {
        let case: serde_json::Value = serde_json::from_str(&line).unwrap();
        if case.get("error").is_some() {
            *kinds.entry("parser error (same parser)".into()).or_default() += 1;
            total += 1;
            continue;
        }
        total += 1;
        let name = case["index"].as_str().unwrap();
        let i = INDEXES.iter().position(|n| *n == name).unwrap();
        let tree: Node = serde_json::from_value(case["tree"].clone()).unwrap();
        let expected: Vec<(u64, i64)> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_u64().unwrap(), p[1].as_i64().unwrap()))
            .collect();
        let len_name = format!("n_{name}");
        let mut c = Compat::new(&searcher, fields.text[i], &len_name, stats[i]);
        let got = match c.run(&tree).unwrap() {
            Some(h) => compat::rank(&searcher, &h, |_| true, usize::MAX).unwrap().1,
            None => Vec::new(),
        };
        let kind = if got == expected {
            "identical".to_owned()
        } else {
            let gs: HashSet<u64> = got.iter().map(|p| p.0).collect();
            let es: HashSet<u64> = expected.iter().map(|p| p.0).collect();
            if gs != es {
                format!("rids differ ({})", case["family"].as_str().unwrap_or("?"))
            } else if got.iter().map(|p| p.0).collect::<Vec<_>>()
                == expected.iter().map(|p| p.0).collect::<Vec<_>>()
            {
                format!("scores differ ({})", case["family"].as_str().unwrap_or("?"))
            } else {
                format!("order differs ({})", case["family"].as_str().unwrap_or("?"))
            }
        };
        if kind != "identical" && examples.len() < 40 {
            let gs: HashSet<u64> = got.iter().map(|p| p.0).collect();
            let es: HashSet<u64> = expected.iter().map(|p| p.0).collect();
            let only_got: Vec<_> = gs.difference(&es).take(5).collect();
            let only_exp: Vec<_> = es.difference(&gs).take(5).collect();
            examples.push(format!(
                "{kind}: {} {:?} got {} exp {} only_got {:?} only_exp {:?} first got {:?} exp {:?}",
                name,
                case["query"],
                got.len(),
                expected.len(),
                only_got,
                only_exp,
                &got[..got.len().min(3)],
                &expected[..expected.len().min(3)]
            ));
        }
        *kinds.entry(kind).or_default() += 1;
    }
    println!("{{\"cases\":{total},\"kinds\":{}}}", serde_json::to_string(&kinds).unwrap());
    for e in examples {
        eprintln!("{e}");
    }
}

/// deltas CORPUS STORE [LIMIT]
fn deltas(args: &[String]) {
    let corpus = PathBuf::from(&args[0]);
    let path = PathBuf::from(&args[1]);
    let limit: usize = args.get(2).map_or(usize::MAX, |s| s.parse().unwrap());
    let store = TextDeltas::create(&path, Compression::None).unwrap();
    let catalog = 0x1234u64;
    let t0 = Instant::now();
    let mut tid = 1u64 << 56;
    let mut batch = Vec::new();
    let (mut rows, mut dbytes, mut ibytes, mut docs) = (0u64, 0u64, 0u64, 0u64);
    let mut zbytes = 0u64;
    let mut sample = 0u64;
    for line in lines(&corpus).take(limit) {
        let (doc, _) = parse(&line);
        let mut list = Vec::new();
        for (i, t) in doc.texts.iter().enumerate() {
            if let Some(t) = t {
                list.push(Delta {
                    catalog,
                    index: INDEXES[i].to_owned(),
                    rid: doc.rid as i32,
                    text: Some(("en".to_owned(), t.clone())),
                });
                if docs % 100 == 0 {
                    // A sample with zstd at level 3: the store setting `zstd`.
                    let payload = zoe_core::text::encode_payload_for_measure("en", t);
                    zbytes += zstd_len(&payload) as u64;
                    sample += payload.len() as u64;
                }
            }
        }
        docs += 1;
        tid += 1;
        batch.push((tid, list));
        if batch.len() == 1000 {
            let s = store.commit(&batch).unwrap();
            rows += s.rows;
            dbytes += s.delta_bytes;
            ibytes += s.index_bytes;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let s = store.commit(&batch).unwrap();
        rows += s.rows;
        dbytes += s.delta_bytes;
        ibytes += s.index_bytes;
    }
    let load = t0.elapsed();
    let size = std::fs::metadata(&path).unwrap().len();
    // Reads at a snapshot.
    let mut reads = Vec::new();
    let mut x = 88172645463325252u64;
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let rid = (x % docs) as i32;
        let t = Instant::now();
        let _ = store.text_at(catalog, "SearchableText", rid, u64::MAX).unwrap();
        reads.push(t.elapsed());
    }
    // A scan of the deltas of 1,000 commits.
    let t = Instant::now();
    let changed = store.changed(catalog, (1u64 << 56) + 1000, (1u64 << 56) + 2000).unwrap();
    let scan = t.elapsed();
    let t = Instant::now();
    let (vrows, vbytes) = store.verify().unwrap();
    let verify = t.elapsed();
    println!(
        "{{\"docs\":{docs},\"rows\":{rows},\"load_s\":{:.1},\"file_bytes\":{size},\"delta_bytes\":{dbytes},\"index_bytes\":{ibytes},\"bytes_per_doc\":{:.0},\"payload_zstd_ratio\":{:.3},\"read_p50_us\":{:.1},\"read_p99_us\":{:.1},\"scan_1000_commits_ms\":{:.2},\"scan_rows\":{},\"verify_s\":{:.1},\"verify_rows\":{vrows},\"verify_bytes\":{vbytes}}}",
        load.as_secs_f64(),
        (dbytes + ibytes) as f64 / docs as f64,
        zbytes as f64 / sample.max(1) as f64,
        pct(&mut reads, 0.5) * 1000.0,
        pct(&mut reads, 0.99) * 1000.0,
        scan.as_secs_f64() * 1000.0,
        changed.len(),
        verify.as_secs_f64(),
    );
}

fn zstd_len(data: &[u8]) -> usize {
    zoe_core::text::zstd_len_for_measure(data)
}

/// window MODE DIR STORE CORPUS QUERIES
fn window(args: &[String]) {
    let mode = mode_of(&args[0]);
    let dir = PathBuf::from(&args[1]);
    let store_path = PathBuf::from(&args[2]);
    let corpus = PathBuf::from(&args[3]);
    let queries: Vec<serde_json::Value> =
        lines(Path::new(&args[4])).map(|l| serde_json::from_str(&l).unwrap()).collect();
    let catalog = 0x1234u64;
    let store = TextDeltas::create(&store_path, Compression::None).unwrap();
    let (idx, fields) = index::open(mode, &dir).unwrap();
    let base_tid = (1u64 << 56) + 2_000_000;
    // The documents that change: 10,000 rids, one in 100.
    let docs: Vec<Doc> = lines(&corpus)
        .step_by(100)
        .take(10_000)
        .map(|l| parse(&l).0)
        .collect();
    // Each changes at its own tid after `base_tid`: a new text, or every
    // tenth an unindex.
    let mut writer = idx.writer_with_num_threads(1, 100_000_000).unwrap();
    let mut commits = Vec::new();
    for (k, d) in docs.iter().enumerate() {
        let new = if k % 10 == 9 {
            Doc { rid: d.rid, texts: [None, None, None], effective: d.effective }
        } else {
            let o = &docs[(k + 1) % docs.len()];
            Doc { rid: d.rid, texts: o.texts.clone(), effective: d.effective }
        };
        index::upsert(&writer, mode, &fields, &new).unwrap();
        let list = (0..3)
            .map(|i| Delta {
                catalog,
                index: INDEXES[i].to_owned(),
                rid: d.rid as i32,
                text: new.texts[i].clone().map(|t| ("en".to_owned(), t)),
            })
            .collect();
        commits.push((base_tid + 1 + k as u64, list));
    }
    store.commit(&commits).unwrap();
    writer.commit().unwrap();
    let searcher = index::reader(&idx).unwrap().searcher();
    let last = base_tid + docs.len() as u64;
    let main_stats = Stats::count(&searcher, "n_SearchableText").unwrap();
    let tree: Node = serde_json::from_value(queries[0]["tree"].clone()).unwrap();
    let mut translator = Translator::new(
        &idx,
        Options { fields: vec![(fields.text[0], 1.0)], fuzzy: 0 },
    );
    let q = translator.translate(&tree).unwrap();
    println!("{{\"query\":{},\"mode\":\"{mode:?}\",\"rows\":[", queries[0]["query"]);
    for (ki, k) in [1usize, 10, 100, 1000, 10_000].into_iter().enumerate() {
        let snapshot = last - k as u64;
        let mut best = [f64::MAX; 6];
        let mut found_deleted = false;
        for _ in 0..5 {
            // Outside the window: the changed documents, their texts at the
            // snapshot, the overlay, the query.
            let t = Instant::now();
            let changed = store.changed(catalog, snapshot, u64::MAX).unwrap();
            let t_scan = t.elapsed();
            let t = Instant::now();
            let mut rids: Vec<i32> = changed.iter().map(|c| c.1).collect();
            rids.sort_unstable();
            rids.dedup();
            let mut odocs = Vec::new();
            for rid in &rids {
                let mut texts = [None, None, None];
                for (i, name) in INDEXES.iter().enumerate() {
                    texts[i] = store.text_at(catalog, name, *rid, snapshot).unwrap().map(|p| p.1);
                }
                // The base deltas are not in this store: the text at the
                // snapshot is the corpus text.
                if texts.iter().all(Option::is_none) {
                    let d = docs.iter().find(|d| d.rid as i32 == *rid).unwrap();
                    texts = d.texts.clone();
                }
                odocs.push(Doc { rid: *rid as u64, texts, effective: 0 });
            }
            let t_read = t.elapsed();
            let t = Instant::now();
            let ov = Overlay::build(mode, &odocs).unwrap();
            let t_build = t.elapsed();
            let t = Instant::now();
            let res = match mode {
                Mode::Enhanced => overlay::enhanced(&searcher, Some(&ov), &*q, |_| true, 20).unwrap(),
                Mode::Compat => overlay::compat(
                    &searcher, &fields, 0, main_stats, Some(&ov), &tree, Counts::Current, |_| true, 20,
                )
                .unwrap(),
            };
            let t_query = t.elapsed();
            // The transaction overlay: the staged texts come with the query.
            let t = Instant::now();
            let ov2 = Overlay::build(mode, &odocs).unwrap();
            let _ = match mode {
                Mode::Enhanced => overlay::enhanced(&searcher, Some(&ov2), &*q, |_| true, 20).unwrap(),
                Mode::Compat => overlay::compat(
                    &searcher, &fields, 0, main_stats, Some(&ov2), &tree, Counts::Exact, |_| true, 20,
                )
                .unwrap(),
            };
            let t_txn = t.elapsed();
            let t = Instant::now();
            let _ = match mode {
                Mode::Enhanced => overlay::enhanced(&searcher, None, &*q, |_| true, 20).unwrap(),
                Mode::Compat => overlay::compat(
                    &searcher, &fields, 0, main_stats, None, &tree, Counts::Current, |_| true, 20,
                )
                .unwrap(),
            };
            let t_plain = t.elapsed();
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let row = [ms(t_scan), ms(t_read), ms(t_build), ms(t_query), ms(t_txn), ms(t_plain)];
            for (b, r) in best.iter_mut().zip(row) {
                *b = b.min(r);
            }
            // The count at the snapshot, from the texts: the count of the
            // index now, minus the changed documents that match now, plus
            // those that match at the snapshot.
            if let Node::Atom { words } = &tree {
                let w = &words[0];
                let has = |t: &Option<String>| {
                    t.as_deref().is_some_and(|t| match mode {
                        Mode::Compat => zoe_catalog::text::analysis::plone_words(t).contains(w),
                        Mode::Enhanced => t.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|x| x == w),
                    })
                };
                let now_count = match mode {
                    Mode::Enhanced => overlay::enhanced(&searcher, None, &*q, |_| true, 1).unwrap().0,
                    Mode::Compat => overlay::compat(&searcher, &fields, 0, main_stats, None, &tree, Counts::Current, |_| true, 1).unwrap().0,
                };
                let changed_set: HashSet<u64> = rids.iter().map(|r| *r as u64).collect();
                let mut expected = now_count as i64;
                for (k2, d) in docs.iter().enumerate() {
                    if !changed_set.contains(&d.rid) {
                        continue;
                    }
                    let now_text = if k2 % 10 == 9 { None } else { docs[(k2 + 1) % docs.len()].texts[0].clone() };
                    expected -= i64::from(has(&now_text));
                    expected += i64::from(has(&d.texts[0]));
                }
                found_deleted = expected == res.0 as i64;
            }
        }
        println!(
            "{}{{\"changed\":{k},\"scan_ms\":{:.2},\"read_ms\":{:.2},\"overlay_build_ms\":{:.2},\"query_with_overlay_ms\":{:.2},\"txn_overlay_total_ms\":{:.2},\"query_alone_ms\":{:.2},\"count_exact\":{found_deleted}}}",
            if ki > 0 { "," } else { "" },
            best[0], best[1], best[2], best[3], best[4], best[5]
        );
    }
    println!("]}}");
    std::io::stdout().flush().unwrap();
}

/// threads DIR MODE
fn threads_cmd(args: &[String]) {
    let mode = mode_of(&args[1]);
    let dir = PathBuf::from(&args[0]);
    let t0 = threads();
    let (idx, fields) = index::open(mode, &dir).unwrap();
    let t_open = threads();
    let reader = index::reader(&idx).unwrap();
    let _s = reader.searcher();
    let t_reader = threads();
    let writer = idx.writer_with_num_threads(1, 50_000_000).unwrap();
    let t_writer = threads();
    let mut writer = writer;
    index::upsert(&writer, mode, &fields, &Doc { rid: 1, texts: [Some("x".into()), None, None], effective: 0 }).unwrap();
    writer.commit().unwrap();
    let t_commit = threads();
    drop(writer);
    std::thread::sleep(Duration::from_millis(200));
    let t_drop = threads();
    println!(
        "{{\"start\":{t0},\"open\":{t_open},\"reader_manual\":{t_reader},\"writer_1\":{t_writer},\"after_commit\":{t_commit},\"after_drop\":{t_drop}}}"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rest = &args[2..];
    match args[1].as_str() {
        "build" => build(rest),
        "bench" => bench(rest),
        "compat" => compat_check(rest),
        "deltas" => deltas(rest),
        "window" => window(rest),
        "threads" => threads_cmd(rest),
        "overlaycost" => {
            // overlaycost MODE CORPUS: the build of an overlay of 0, 1 and 10
            // documents, the mean of 200 builds.
            let mode = mode_of(&rest[0]);
            let docs: Vec<Doc> = lines(Path::new(&rest[1])).skip(5000).take(10).map(|l| parse(&l).0).collect();
            for k in [0usize, 1, 10] {
                let t = Instant::now();
                for _ in 0..200 {
                    let _ = Overlay::build(mode, &docs[..k]).unwrap();
                }
                println!("{k} docs: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / 200.0);
            }
            let t = Instant::now();
            for _ in 0..200 {
                for d in &docs {
                    let _ = index::document(mode, &index::schema(mode).1, d);
                }
            }
            println!("10 documents, analysis only: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / 200.0);
        }
        "words" => {
            // words CORPUS: the words of `plone_words` for each SearchableText.
            for line in lines(Path::new(&rest[0])) {
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                let words = zoe_catalog::text::analysis::plone_words(v["SearchableText"].as_str().unwrap_or(""));
                println!("{}", serde_json::json!({"rid": v["rid"], "words": words}));
            }
        }
        other => panic!("unknown command {other}"),
    }
}
