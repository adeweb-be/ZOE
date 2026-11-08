//! The tantivy index of one catalog: its schema, its documents, its
//! directory and the maps between the rids and the documents (ADR 0060,
//! section 10).
//!
//! One document holds the texts of one rid in every text index of the
//! catalog. Each text index has one field for each of its languages, and a
//! field of the pairs of words in a row beside each. The field of a text is
//! the field of its language, else the field of the first language of the
//! index. The fast field `rid` holds the rid, and a delete of the term of
//! the rid removes the document.
//!
//! The payload of each tantivy commit holds the tid up to which the index
//! holds the deltas, and the layout of the settings that built it.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as Json;
use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::index::SegmentId;
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{PreTokenizedString, TextAnalyzer, Token};
use tantivy::{DocId, Index, IndexReader, ReloadPolicy, Searcher, TantivyDocument};

use super::analysis;
use super::settings::{Layout, Mode};

/// The name of the fast field of the rid.
pub const RID: &str = "rid";

/// The fields of one language of one text index.
#[derive(Clone, Debug)]
pub struct LangFields {
    /// The language.
    pub language: String,
    /// The field of the words.
    pub text: Field,
    /// The field of the pairs of words in a row.
    pub pairs: Field,
}

/// The fields of the schema of a catalog.
#[derive(Clone, Debug)]
pub struct Fields {
    /// The rid.
    pub rid: Field,
    /// The fields of each text index, by index id.
    pub indexes: HashMap<String, Vec<LangFields>>,
}

impl Fields {
    /// The fields of `index` that read the languages `languages`, or every
    /// field for `None`.
    pub fn of(&self, index: &str, languages: Option<&[String]>) -> Vec<LangFields> {
        let Some(all) = self.indexes.get(index) else {
            return Vec::new();
        };
        match languages {
            None => all.clone(),
            Some(wanted) => all
                .iter()
                .filter(|f| wanted.iter().any(|w| same_language(w, &f.language)))
                .cloned()
                .collect(),
        }
    }
}

/// Tell if two language tags name one language: equal in lower case.
fn same_language(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The place of the field of the language `language` among `languages`:
/// the same tag, else the same primary subtag, else the first.
pub fn field_of(languages: &[String], language: &str) -> usize {
    if let Some(i) = languages.iter().position(|l| same_language(l, language)) {
        return i;
    }
    let primary = |t: &str| t.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    let wanted = primary(language);
    languages
        .iter()
        .position(|l| primary(l) == wanted)
        .unwrap_or(0)
}

/// The languages of the fields of an index of `layout`: its languages, or
/// one field with no language when it lists none.
pub fn field_languages(languages: &[String]) -> Vec<String> {
    if languages.is_empty() {
        vec![String::new()]
    } else {
        languages.to_vec()
    }
}

/// The schema of `layout` and its fields.
pub fn schema(layout: &Layout) -> (Schema, Fields) {
    let mut b = Schema::builder();
    let rid = b.add_i64_field(RID, INDEXED | FAST);
    let words = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let pairs = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    let mut indexes = HashMap::new();
    for (i, (name, _, languages)) in layout.iter().enumerate() {
        let fields = field_languages(languages)
            .into_iter()
            .enumerate()
            .map(|(j, language)| LangFields {
                language,
                text: b.add_text_field(&format!("t{i}_{j}"), words.clone()),
                pairs: b.add_text_field(&format!("p{i}_{j}"), pairs.clone()),
            })
            .collect();
        indexes.insert(name.clone(), fields);
    }
    (b.build(), Fields { rid, indexes })
}

/// The payload of a commit: the tid and the layout.
pub fn payload(tid: u64, layout: &Layout) -> String {
    let layout: Vec<Json> = layout
        .iter()
        .map(|(name, mode, languages)| {
            Json::Array(vec![
                Json::String(name.clone()),
                Json::String(mode.name().into()),
                Json::Array(languages.iter().cloned().map(Json::String).collect()),
            ])
        })
        .collect();
    serde_json::json!({"tid": tid, "layout": layout}).to_string()
}

/// Read a payload: the tid and the layout. `None` for another form.
pub fn read_payload(text: &str) -> Option<(u64, Layout)> {
    let json: Json = serde_json::from_str(text).ok()?;
    let tid = json.get("tid")?.as_u64()?;
    let mut layout = Vec::new();
    for item in json.get("layout")?.as_array()? {
        let [name, mode, languages] = item.as_array()?.as_slice() else {
            return None;
        };
        layout.push((
            name.as_str()?.to_owned(),
            Mode::from_name(mode.as_str()?)?,
            languages
                .as_array()?
                .iter()
                .map(|l| l.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
        ));
    }
    Some((tid, layout))
}

/// The analyzers of the fields: one for each language.
#[derive(Clone, Default)]
pub struct Analyzers {
    by_language: HashMap<String, TextAnalyzer>,
}

impl Analyzers {
    /// The words of `text` in the language `language`.
    pub fn words(&mut self, language: &str, text: &str) -> Vec<String> {
        let analyzer = self
            .by_language
            .entry(language.to_ascii_lowercase())
            .or_insert_with(|| analysis::analyzer(language));
        analysis::words(analyzer, text)
    }
}

/// The tokens of `words` at the positions 0, 1, 2 and on.
fn tokens(words: impl Iterator<Item = String>) -> Vec<Token> {
    words
        .enumerate()
        .map(|(position, text)| Token {
            offset_from: 0,
            offset_to: 0,
            position,
            text,
            position_length: 1,
        })
        .collect()
}

/// The pairs of the words in a row of `words`.
pub fn pairs_of(words: &[String]) -> Vec<String> {
    words.windows(2).map(|w| analysis::pair(&w[0], &w[1])).collect()
}

/// The document of `rid` with `texts`: for each text index, its language
/// and its text. A text index of no field is left out.
pub fn document(
    fields: &Fields,
    analyzers: &mut Analyzers,
    rid: i32,
    texts: &[(String, String, String)],
) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_i64(fields.rid, i64::from(rid));
    for (index, language, text) in texts {
        let Some(langs) = fields.indexes.get(index) else {
            continue;
        };
        let names: Vec<String> = langs.iter().map(|f| f.language.clone()).collect();
        let f = &langs[field_of(&names, language)];
        let words = analyzers.words(&f.language, text);
        let pairs = pairs_of(&words);
        d.add_pre_tokenized_text(
            f.pairs,
            PreTokenizedString {
                text: String::new(),
                tokens: tokens(pairs.into_iter()),
            },
        );
        d.add_pre_tokenized_text(
            f.text,
            PreTokenizedString {
                text: String::new(),
                tokens: tokens(words.into_iter()),
            },
        );
    }
    d
}

/// The directory of the index of `catalog` in the store directory `dir`.
pub fn directory(dir: &Path, catalog: u64) -> PathBuf {
    dir.join("text").join(format!("{catalog:016x}"))
}

/// Create a new index of `layout` in `path`, after the removal of what
/// `path` holds.
pub fn create(path: &Path, layout: &Layout) -> tantivy::Result<(Index, Fields)> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::create_dir_all(path)?;
    let (schema, fields) = schema(layout);
    let index = Index::create_in_dir(path, schema)?;
    Ok((index, fields))
}

/// Open the index in `path`. A read-only open writes nothing in `path`.
pub fn open(path: &Path, read_only: bool) -> tantivy::Result<Index> {
    if read_only {
        let inner = MmapDirectory::open(path)?;
        Index::open(ReadOnly(inner))
    } else {
        Index::open_in_dir(path)
    }
}

/// A reader that reloads only on request: it starts no thread.
pub fn reader(index: &Index) -> tantivy::Result<IndexReader> {
    index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
}

/// A directory that refuses each write: the directory of a read-only
/// open. Its lock is a lock of nothing, and it watches nothing.
#[derive(Clone, Debug)]
struct ReadOnly(MmapDirectory);

fn refused() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "a read-only text index")
}

impl Directory for ReadOnly {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.0.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(refused()),
            filepath: path.to_owned(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.0.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(refused(), path.to_owned()))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.0.atomic_read(path)
    }

    fn atomic_write(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(refused())
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

/// Tell if `error` says that a directory does not exist.
pub fn is_missing(error: &tantivy::TantivyError) -> bool {
    matches!(
        error,
        tantivy::TantivyError::OpenDirectoryError(OpenDirectoryError::DoesNotExist(_))
    )
}

/// The maps of the documents of a reader: the rid of each document of each
/// segment, and the segment and the document of each rid.
#[derive(Debug, Default)]
pub struct Maps {
    /// The rid of each document of each segment, in the order of the
    /// segments of the searcher.
    pub doc_to_rid: Vec<Arc<Vec<i32>>>,
    /// The live documents: each rid with its segment and its document, in
    /// rid order.
    pub by_rid: Vec<(i32, u32, DocId)>,
    /// The number of documents of each segment.
    pub max_docs: Vec<u32>,
}

/// The rids of each segment that the last maps read, so a reload reads
/// only the new segments.
pub type SegmentRids = HashMap<SegmentId, Arc<Vec<i32>>>;

impl Maps {
    /// The maps of `searcher`. `cache` holds the rids of the segments that
    /// a reload already read, and keeps those of the segments of
    /// `searcher`.
    pub fn build(searcher: &Searcher, cache: &mut SegmentRids) -> tantivy::Result<Maps> {
        let mut maps = Maps::default();
        let mut seen = SegmentRids::new();
        for (ord, seg) in searcher.segment_readers().iter().enumerate() {
            let id = seg.segment_id();
            let rids = match cache.get(&id) {
                Some(r) => Arc::clone(r),
                None => {
                    let col = seg.fast_fields().i64(RID)?;
                    let rids: Vec<i32> = (0..seg.max_doc())
                        .map(|d| col.first(d).unwrap_or(0) as i32)
                        .collect();
                    Arc::new(rids)
                }
            };
            for doc in 0..seg.max_doc() {
                if seg.alive_bitset().is_none_or(|a| a.is_alive(doc)) {
                    maps.by_rid.push((rids[doc as usize], ord as u32, doc));
                }
            }
            seen.insert(id, Arc::clone(&rids));
            maps.doc_to_rid.push(rids);
            maps.max_docs.push(seg.max_doc());
        }
        maps.by_rid.sort_unstable_by_key(|e| e.0);
        *cache = seen;
        Ok(maps)
    }

    /// The live documents.
    pub fn num_docs(&self) -> u64 {
        self.by_rid.len() as u64
    }

    /// The segment and the document of `rid`.
    pub fn doc(&self, rid: i32) -> Option<(u32, DocId)> {
        self.by_rid
            .binary_search_by_key(&rid, |e| e.0)
            .ok()
            .map(|i| (self.by_rid[i].1, self.by_rid[i].2))
    }

    /// The bytes of the maps.
    pub fn heap_bytes(&self) -> usize {
        self.by_rid.capacity() * 12 + self.doc_to_rid.iter().map(|r| r.len() * 4).sum::<usize>()
    }
}
