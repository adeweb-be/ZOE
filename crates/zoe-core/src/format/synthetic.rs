//! A synthetic record set with the shape of a Plone store, and the
//! measurement of its shrink ratio under each zstd level.
//!
//! Each record is a ZODB record: a class pickle, then a state pickle, in
//! pickle protocol 3. The set holds content objects with rich text, the
//! buckets of folders and of catalog indexes, catalog metadata, workflow
//! histories, annotations, file records and `Length` counters. The words
//! are made from random syllables, so the text is generic. A fixed seed
//! makes the set the same on each run.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zstd::bulk::{Compressor, Decompressor};

use super::codec::{self, Codec, Compression};

/// A generator of random numbers: xorshift64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Return a number from `low` to `high`, both included.
    fn range(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }

    /// Return true with the probability `percent` in 100.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.range(0, items.len() - 1)]
    }
}

const SYLLABLES: [&str; 24] = [
    "ka", "lo", "mi", "ne", "ru", "ta", "vi", "so", "pe", "du", "ra", "li", "mo", "sa", "te", "ko",
    "ba", "ri", "fu", "zen", "dor", "mal", "tes", "quo",
];

const USERS: [&str; 6] = [
    "admin", "editor1", "editor2", "author1", "author2", "reviewer",
];

const ROLES: [&str; 7] = [
    "Manager",
    "Site Administrator",
    "Owner",
    "Editor",
    "Reader",
    "Contributor",
    "Reviewer",
];

const CONTENT: [(&str, &str); 7] = [
    ("Document", "Page"),
    ("NewsItem", "News Item"),
    ("Event", "Event"),
    ("Folder", "Folder"),
    ("File", "File"),
    ("Image", "Image"),
    ("Link", "Link"),
];

const STATES: [&str; 3] = ["private", "pending", "published"];

/// The writer of the pickles of one record. The memo spans the class
/// pickle and the state pickle, as in ZODB.
struct Pickler<'r> {
    out: Vec<u8>,
    memo: u32,
    globals: HashMap<(&'static str, &'static str), u32>,
    shared: HashMap<String, u32>,
    rng: &'r mut Rng,
    vocabulary: &'r [String],
}

impl<'r> Pickler<'r> {
    fn new(rng: &'r mut Rng, vocabulary: &'r [String]) -> Self {
        Self {
            out: Vec::new(),
            memo: 0,
            globals: HashMap::new(),
            shared: HashMap::new(),
            rng,
            vocabulary,
        }
    }

    fn op(&mut self, ops: &[u8]) {
        self.out.extend_from_slice(ops);
    }

    fn put(&mut self) -> u32 {
        let index = self.memo;
        if index < 256 {
            self.op(&[b'q', index as u8]);
        } else {
            self.op(b"r");
            self.op(&index.to_le_bytes());
        }
        self.memo += 1;
        index
    }

    fn get(&mut self, index: u32) {
        if index < 256 {
            self.op(&[b'h', index as u8]);
        } else {
            self.op(b"j");
            self.op(&index.to_le_bytes());
        }
    }

    fn global(&mut self, module: &'static str, name: &'static str) {
        if let Some(&index) = self.globals.get(&(module, name)) {
            return self.get(index);
        }
        self.op(b"c");
        self.op(module.as_bytes());
        self.op(b"\n");
        self.op(name.as_bytes());
        self.op(b"\n");
        let index = self.put();
        self.globals.insert((module, name), index);
    }

    fn text(&mut self, text: &str) {
        self.op(b"X");
        self.op(&(text.len() as u32).to_le_bytes());
        self.op(text.as_bytes());
        self.put();
    }

    /// Write a string that one object of the program holds, such as a role
    /// name. A second use refers to the memo.
    fn shared_text(&mut self, text: &str) {
        if let Some(&index) = self.shared.get(text) {
            return self.get(index);
        }
        self.text(text);
        self.shared.insert(text.to_owned(), self.memo - 1);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.op(&[b'C', bytes.len() as u8]);
        self.op(bytes);
        self.put();
    }

    fn int(&mut self, n: u64) {
        match n {
            0..=0xff => self.op(&[b'K', n as u8]),
            0x100..=0xffff => {
                self.op(b"M");
                self.op(&(n as u16).to_le_bytes());
            }
            0x1_0000..=0x7fff_ffff => {
                self.op(b"J");
                self.op(&(n as u32).to_le_bytes());
            }
            _ => {
                self.op(&[0x8a, 8]);
                self.op(&n.to_le_bytes());
            }
        }
    }

    fn boolean(&mut self, value: bool) {
        self.op(if value { b"\x88" } else { b"\x89" });
    }

    fn tuple(&mut self, items: usize) {
        match items {
            0 => self.op(b")"),
            1 => self.op(b"\x85"),
            2 => self.op(b"\x86"),
            3 => self.op(b"\x87"),
            _ => self.op(b"t"),
        }
        if items > 0 {
            self.put();
        }
    }

    /// End a tuple that starts at a mark.
    fn mark_tuple(&mut self) {
        self.op(b"t");
        self.put();
    }

    /// Write the class pickle of a record, and start the state pickle.
    fn class(&mut self, module: &'static str, name: &'static str) {
        self.op(b"\x80\x03");
        self.global(module, name);
        self.op(b"N");
        self.tuple(2);
        self.op(b".\x80\x03");
    }

    fn reference(&mut self, module: &'static str, name: &'static str) {
        let oid = self.rng.next() % 200_000;
        self.bytes(&oid.to_be_bytes());
        self.global(module, name);
        self.tuple(2);
        self.op(b"Q");
    }

    fn date(&mut self) {
        self.global("DateTime.DateTime", "DateTime");
        self.op(b")\x81");
        self.put();
        self.op(b"(");
        let micros = 1_600_000_000_000_000 + self.rng.next() % 200_000_000_000_000;
        self.int(micros);
        self.boolean(false);
        self.shared_text("UTC");
        self.op(b"t");
        self.put();
        self.op(b"b");
    }

    /// Return a word of the vocabulary. The rank of a word follows a
    /// log-uniform law, near the law of word frequency in text: the first
    /// 100 words of 3000 give more than half of the words.
    fn word(&mut self) -> String {
        let u = (self.rng.next() % 1_000_000) as f64 / 1_000_000.0;
        let rank = (self.vocabulary.len() as f64).powf(u) as usize;
        self.vocabulary[rank - 1].clone()
    }

    fn words(&mut self, low: usize, high: usize) -> String {
        let count = self.rng.range(low, high);
        (0..count)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn uid(&mut self) -> String {
        format!("{:016x}{:016x}", self.rng.next(), self.rng.next())
    }

    fn slug(&mut self) -> String {
        self.words(1, 4).replace(' ', "-")
    }

    fn html(&mut self) -> String {
        let mut html = String::new();
        let target = self.rng.range(300, 6_000);
        while html.len() < target {
            match self.rng.range(0, 9) {
                0 => html.push_str(&format!("<h2>{}</h2>", self.words(2, 6))),
                1 => {
                    html.push_str("<ul>");
                    for _ in 0..self.rng.range(2, 5) {
                        html.push_str(&format!("<li>{}</li>", self.words(3, 10)));
                    }
                    html.push_str("</ul>");
                }
                2 => {
                    let uid = self.uid();
                    let label = self.words(1, 3);
                    html.push_str(&format!(
                        "<p>{} <a href=\"resolveuid/{uid}\">{label}</a> {}.</p>",
                        self.words(5, 20),
                        self.words(5, 20)
                    ));
                }
                3 => html.push_str(&format!(
                    "<p><strong>{}</strong> {}.</p>",
                    self.words(1, 3),
                    self.words(10, 40)
                )),
                _ => html.push_str(&format!("<p>{}.</p>", self.words(10, 60))),
            }
        }
        html
    }

    fn finish(mut self) -> Vec<u8> {
        self.op(b".");
        self.out
    }
}

/// The kinds of records in the set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Content,
    FolderBucket,
    MetadataBucket,
    IndexBucket,
    UidBucket,
    Length,
    WorkflowHistory,
    Annotation,
    FileRecord,
}

/// The kinds and their weights in 100.
const KINDS: [(Kind, u64); 9] = [
    (Kind::Content, 20),
    (Kind::FolderBucket, 8),
    (Kind::MetadataBucket, 12),
    (Kind::IndexBucket, 15),
    (Kind::UidBucket, 5),
    (Kind::Length, 10),
    (Kind::WorkflowHistory, 12),
    (Kind::Annotation, 10),
    (Kind::FileRecord, 8),
];

fn content(p: &mut Pickler<'_>) {
    let (class, portal_type) = CONTENT[p.rng.range(0, CONTENT.len() - 1)];
    p.class("plone.app.contenttypes.content", class);
    p.op(b"}");
    p.put();
    p.op(b"(");
    p.text("__ac_local_roles__");
    p.op(b"}");
    p.put();
    let owner = p.rng.pick(&USERS);
    p.text(owner);
    p.op(b"]");
    p.put();
    p.shared_text("Owner");
    p.op(b"as");
    p.text("_owner");
    p.op(b"]");
    p.put();
    p.text("acl_users");
    p.op(b"a");
    p.text(owner);
    p.tuple(2);
    let title = p.words(2, 8);
    let id = title.replace(' ', "-");
    for (key, value) in [("id", id), ("title", title)] {
        p.text(key);
        p.text(&value);
    }
    p.text("description");
    let description = if p.rng.chance(70) {
        p.words(8, 40)
    } else {
        String::new()
    };
    p.text(&description);
    if matches!(class, "Document" | "NewsItem" | "Event") {
        p.text("text");
        p.global("plone.app.textfield.value", "RichTextValue");
        p.op(b")\x81");
        p.put();
        p.op(b"}");
        p.put();
        p.op(b"(");
        let html = p.html();
        for (key, value) in [
            ("_raw", html.as_str()),
            ("_mimeType", "text/html"),
            ("_outputMimeType", "text/x-html-safe"),
            ("_encoding", "utf-8"),
        ] {
            p.text(key);
            p.text(value);
        }
        p.op(b"ub");
    }
    p.text("creators");
    p.text(owner);
    p.tuple(1);
    p.text("contributors");
    p.tuple(0);
    p.text("subject");
    p.op(b"(");
    let keywords = p.rng.range(0, 4);
    for _ in 0..keywords {
        let keyword = p.word();
        p.text(&keyword);
    }
    p.mark_tuple();
    for key in ["language", "rights", "changeNote"] {
        p.text(key);
        p.text("");
    }
    for key in ["creation_date", "modification_date", "effective_date"] {
        p.text(key);
        p.date();
    }
    p.text("expiration_date");
    p.op(b"N");
    p.text("exclude_from_nav");
    p.boolean(false);
    p.text("_plone.uuid");
    let uid = p.uid();
    p.text(&uid);
    p.text("portal_type");
    p.text(portal_type);
    p.text("workflow_history");
    p.reference("persistent.mapping", "PersistentMapping");
    p.text("__annotations__");
    p.reference("BTrees.OOBTree", "OOBTree");
    for permission in [
        "_Access_contents_information_Permission",
        "_View_Permission",
        "_Modify_portal_content_Permission",
    ] {
        p.text(permission);
        p.op(b"(");
        let count = p.rng.range(2, ROLES.len());
        for role in &ROLES[..count] {
            p.shared_text(role);
        }
        p.mark_tuple();
    }
    match class {
        "Folder" => {
            p.text("_tree");
            p.reference("BTrees.OOBTree", "OOBTree");
            p.text("_count");
            p.reference("BTrees.Length", "Length");
            p.text("_mt_index");
            p.reference("BTrees.OOBTree", "OOBTree");
        }
        "Event" => {
            for key in ["start", "end"] {
                p.text(key);
                p.global("datetime", "datetime");
                let stamp = p.rng.next().to_le_bytes();
                p.bytes(&[&stamp[..], &[0, 0]].concat());
                p.tuple(1);
                p.op(b"R");
            }
            p.text("location");
            let location = p.words(1, 3);
            p.text(&location);
            p.text("whole_day");
            p.boolean(false);
        }
        "File" | "Image" => {
            p.text(if class == "File" { "file" } else { "image" });
            p.reference("plone.namedfile.file", "NamedBlobFile");
        }
        "Link" => {
            p.text("remoteUrl");
            let url = format!("${{portal_url}}/{}/{}", p.slug(), p.slug());
            p.text(&url);
        }
        _ => {}
    }
    p.op(b"u");
}

/// The bucket of a folder: ids and references to content objects.
fn folder_bucket(p: &mut Pickler<'_>) {
    p.class("BTrees.OOBTree", "OOBucket");
    p.op(b"(");
    for _ in 0..p.rng.range(10, 60) {
        let id = p.slug();
        p.text(&id);
        let (class, _) = CONTENT[p.rng.range(0, CONTENT.len() - 1)];
        p.reference("plone.app.contenttypes.content", class);
    }
    p.mark_tuple();
    p.tuple(1);
}

/// The bucket of the catalog metadata: a tuple of values for each rid.
fn metadata_bucket(p: &mut Pickler<'_>) {
    p.class("BTrees.IOBTree", "IOBucket");
    p.op(b"(");
    let mut rid = p.rng.next() % 1_000_000_000;
    for _ in 0..p.rng.range(5, 25) {
        rid += p.rng.next() % 5_000;
        p.int(rid);
        p.op(b"(");
        let title = p.words(2, 8);
        p.text(&title);
        let description = p.words(0, 30);
        p.text(&description);
        let state = p.rng.pick(&STATES);
        p.text(state);
        let (_, portal_type) = CONTENT[p.rng.range(0, CONTENT.len() - 1)];
        p.text(portal_type);
        let id = title.replace(' ', "-");
        p.text(&id);
        p.text(&id);
        let uid = p.uid();
        p.text(&uid);
        let owner = p.rng.pick(&USERS);
        p.text(owner);
        p.text(owner);
        p.tuple(1);
        for _ in 0..4 {
            p.date();
        }
        let size = format!("{}.{} KB", p.rng.range(1, 900), p.rng.range(0, 9));
        p.text(&size);
        let folderish = p.rng.chance(20);
        p.boolean(folderish);
        p.boolean(false);
        p.op(b"NNNN");
        p.text("");
        p.text("text/html");
        p.tuple(0);
        p.mark_tuple();
    }
    p.mark_tuple();
    p.tuple(1);
}

/// The bucket of an index: sorted rids.
fn index_bucket(p: &mut Pickler<'_>) {
    p.class("BTrees.IIBTree", "IITreeSet");
    p.op(b"(");
    let mut rid = p.rng.next() % 1_000_000_000;
    for _ in 0..p.rng.range(30, 150) {
        rid += p.rng.next() % 50_000;
        p.int(rid);
    }
    p.mark_tuple();
    p.tuple(1);
}

/// The bucket of the UID index: UIDs and rids.
fn uid_bucket(p: &mut Pickler<'_>) {
    p.class("BTrees.OIBTree", "OIBucket");
    p.op(b"(");
    for _ in 0..p.rng.range(20, 90) {
        let uid = p.uid();
        p.text(&uid);
        let rid = p.rng.next() % 1_000_000_000;
        p.int(rid);
    }
    p.mark_tuple();
    p.tuple(1);
}

fn length(p: &mut Pickler<'_>) {
    p.class("BTrees.Length", "Length");
    let n = p.rng.next() % 5_000;
    p.int(n);
}

fn workflow_history(p: &mut Pickler<'_>) {
    p.class("persistent.mapping", "PersistentMapping");
    p.op(b"}");
    p.put();
    p.text("data");
    p.op(b"}");
    p.put();
    p.text("simple_publication_workflow");
    p.op(b"(");
    let entries = p.rng.range(1, 5);
    for _ in 0..entries {
        p.op(b"}");
        p.put();
        p.op(b"(");
        p.text("action");
        if p.rng.chance(30) {
            p.op(b"N");
        } else {
            let action = p.rng.pick(&["publish", "submit", "retract", "reject"]);
            p.text(action);
        }
        p.text("actor");
        let actor = p.rng.pick(&USERS);
        p.text(actor);
        p.text("comments");
        p.text("");
        p.text("review_state");
        let state = p.rng.pick(&STATES);
        p.text(state);
        p.text("time");
        p.date();
        p.op(b"u");
    }
    p.mark_tuple();
    p.op(b"ss");
}

fn annotation(p: &mut Pickler<'_>) {
    p.class("persistent.mapping", "PersistentMapping");
    p.op(b"}");
    p.put();
    p.text("data");
    p.op(b"}");
    p.put();
    p.op(b"(");
    for _ in 0..p.rng.range(1, 4) {
        let key = format!("plone.app.{}.{}", p.word(), p.word());
        p.text(&key);
        if p.rng.chance(50) {
            p.reference("BTrees.OOBTree", "OOBTree");
        } else {
            let value = p.words(1, 6);
            p.text(&value);
        }
    }
    p.op(b"us");
}

fn file_record(p: &mut Pickler<'_>) {
    p.class("plone.namedfile.file", "NamedBlobFile");
    p.op(b"}");
    p.put();
    p.op(b"(");
    p.text("_blob");
    p.reference("ZODB.blob", "Blob");
    p.text("contentType");
    let kind = p.rng.pick(&["image/png", "image/jpeg", "application/pdf"]);
    p.text(kind);
    p.text("filename");
    let name = format!("{}.{}", p.slug(), &kind[kind.find('/').unwrap() + 1..]);
    p.text(&name);
    p.text("size");
    let size = p.rng.next() % 5_000_000;
    p.int(size);
    p.op(b"u");
}

/// Return `count` records and their kinds.
fn records(count: usize) -> Vec<(Kind, Vec<u8>)> {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let vocabulary: Vec<String> = (0..3_000)
        .map(|_| {
            let syllables = rng.range(1, 4);
            (0..syllables)
                .map(|_| SYLLABLES[rng.range(0, SYLLABLES.len() - 1)])
                .collect()
        })
        .collect();
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let mut roll = rng.next() % 100;
        let kind = KINDS
            .iter()
            .find(|(_, weight)| {
                let hit = roll < *weight;
                roll = roll.saturating_sub(*weight);
                hit
            })
            .map(|(kind, _)| *kind)
            .expect("the weights add up to 100");
        let mut p = Pickler::new(&mut rng, &vocabulary);
        match kind {
            Kind::Content => content(&mut p),
            Kind::FolderBucket => folder_bucket(&mut p),
            Kind::MetadataBucket => metadata_bucket(&mut p),
            Kind::IndexBucket => index_bucket(&mut p),
            Kind::UidBucket => uid_bucket(&mut p),
            Kind::Length => length(&mut p),
            Kind::WorkflowHistory => workflow_history(&mut p),
            Kind::Annotation => annotation(&mut p),
            Kind::FileRecord => file_record(&mut p),
        }
        records.push((kind, p.finish()));
    }
    records
}

/// The sizes of a record set under one compression setting.
#[derive(Debug, Default)]
struct Sizes {
    data: u64,
    stored: u64,
    plain: usize,
}

impl Sizes {
    fn of<'a>(records: impl Iterator<Item = &'a [u8]>, compression: Compression) -> Self {
        let mut sizes = Self::default();
        for data in records {
            let (codec, payload) = codec::encode(data, compression);
            sizes.data += data.len() as u64;
            sizes.stored += payload.len() as u64;
            sizes.plain += usize::from(codec == Codec::None);
        }
        sizes
    }

    fn ratio(&self) -> f64 {
        self.data as f64 / self.stored as f64
    }
}

#[test]
fn the_synthetic_set_round_trips_and_shrinks() {
    let records = records(1_000);
    let mut sizes = Sizes::default();
    for (_, data) in &records {
        let (codec, payload) = codec::encode(data, Compression::default());
        assert_eq!(&codec::decode(codec, &payload).unwrap()[..], &data[..]);
        sizes.data += data.len() as u64;
        sizes.stored += payload.len() as u64;
    }
    // At the default level, these 1,000 records shrink by 1.87. The bound
    // below catches a fault in the codec or in the generator.
    assert!(sizes.ratio() > 1.7, "{sizes:?}");
}

/// Return the time of `run` for each record, as the median of 5 runs.
fn per_record(count: usize, mut run: impl FnMut()) -> Duration {
    let mut times: Vec<Duration> = (0..5)
        .map(|_| {
            let start = Instant::now();
            run();
            start.elapsed() / count as u32
        })
        .collect();
    times.sort();
    times[2]
}

/// Print the shrink ratio and the time per record of each zstd level.
/// Run it with `--release --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn shrink_ratio_and_time_per_record() {
    let records = records(5_000);
    let data: Vec<&[u8]> = records.iter().map(|(_, data)| &data[..]).collect();
    let n = data.len();
    let total: u64 = data.iter().map(|d| d.len() as u64).sum();
    println!(
        "records: {n}, data: {total} bytes, mean {} bytes",
        total / n as u64
    );
    println!();
    println!(
        "| level | ratio | ratio with headers | kept plain | compress | compress, reused context | decompress | decompress, reused context |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for level in [-5, 1, 3, 6, 9, 19] {
        let compression = Compression::Zstd { level };
        let sizes = Sizes::of(data.iter().copied(), compression);
        let headers = (n * super::HEADER_LEN) as f64;
        let framed = (sizes.data as f64 + headers) / (sizes.stored as f64 + headers);
        let encoded: Vec<(Codec, Vec<u8>)> = data
            .iter()
            .map(|d| {
                let (codec, payload) = codec::encode(d, compression);
                (codec, payload.into_owned())
            })
            .collect();

        let compress = per_record(n, || {
            for d in &data {
                std::hint::black_box(codec::encode(d, compression));
            }
        });
        let mut compressor = Compressor::new(level).unwrap();
        let compress_reused = per_record(n, || {
            for d in &data {
                let mut out = Vec::with_capacity(d.len());
                std::hint::black_box(compressor.compress_to_buffer(d, &mut out).ok());
            }
        });
        let decompress = per_record(n, || {
            for (codec, payload) in &encoded {
                std::hint::black_box(codec::decode(*codec, payload).unwrap());
            }
        });
        let mut decompressor = Decompressor::new().unwrap();
        let decompress_reused = per_record(n, || {
            for ((codec, payload), d) in encoded.iter().zip(&data) {
                if *codec == Codec::Zstd {
                    let mut out = Vec::with_capacity(d.len());
                    decompressor
                        .decompress_to_buffer(payload, &mut out)
                        .unwrap();
                    std::hint::black_box(out);
                }
            }
        });
        println!(
            "| {level} | {:.2} | {framed:.2} | {:.1} % | {compress:.1?} | {compress_reused:.1?} | {decompress:.1?} | {decompress_reused:.1?} |",
            sizes.ratio(),
            100.0 * sizes.plain as f64 / n as f64,
        );
    }

    println!();
    println!("At level 3, by kind:");
    println!();
    println!("| kind | records | mean size | ratio | kept plain |");
    println!("| --- | --- | --- | --- | --- |");
    for (kind, _) in KINDS {
        let of_kind: Vec<&[u8]> = records
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, d)| &d[..])
            .collect();
        let sizes = Sizes::of(of_kind.iter().copied(), Compression::default());
        println!(
            "| {kind:?} | {} | {} | {:.2} | {} |",
            of_kind.len(),
            sizes.data / of_kind.len() as u64,
            sizes.ratio(),
            sizes.plain
        );
    }
}
