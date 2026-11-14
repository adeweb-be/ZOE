//! The native text index of the catalogs (ADR 0060), behind the feature
//! `text`.
//!
//! - `settings`: the settings of the text indexes of a catalog, in the
//!   durable namespace `zoe.text`.
//! - `clause`: the text clause, its codec, and the replies of `text_query`
//!   and `text_info`.
//! - `analysis`: the analyzer of the enhanced mode.
//! - `lexicon`: the words of the lexicon `plone_lexicon` of Plone 6.1, of
//!   the compatibility mode, with its tables in `plone_table.rs`, which
//!   `plone_table.py` generates.
//! - `index`: the tantivy index of a catalog, its directory and its maps.
//! - `translate`: the parse tree as a tantivy query, and the bound of its
//!   hits.
//! - `search`: the search inside a filter, and the integer scores.
//! - `overlay`: the text overlay, which scores the replaced documents.
//! - `compat`: the search of the compatibility mode: the score of
//!   `OkapiIndex` over the index and the text overlay.
//! - `texts`: the text indexes of a store and their thread.
//! - `engine`: the answer of a text clause and of `text_info`.
//! - `rebuild`: the rebuild of `zoe catalog rebuild-text`, in
//!   `text/<oid>.new`, and its two renames.
//!
//! The index is a cache: the deltas of the store are the source of truth,
//! and a rebuild from them loses nothing. The crate starts the thread of the
//! text indexes with the mirrors of a store that holds a registered
//! catalog, never at the load of a module.

pub mod analysis;
pub mod clause;
pub mod compat;
pub mod engine;
pub mod index;
pub mod lexicon;
pub mod overlay;
mod plone_table;
pub mod rebuild;
pub mod search;
pub mod settings;
pub mod texts;
pub mod translate;

pub use clause::{
    CallError, Node, Staged, TextClause, TextError, TextErrorKind, TextHits, TextInfo,
};
pub use engine::{Answer, Query};
pub use search::{Filter, Strategy, Want};
pub use settings::{CatalogSettings, IndexSettings, Mode};
pub use texts::{Failure, TextConfig, TextHooks, TextState, Texts};

/// The search of a text clause inside the rids of the other clauses of a
/// plan, for what the plan wants.
pub type TextSearch<'a> =
    dyn FnMut(search::Filter<'_>, Want) -> Result<search::Found, Failure> + 'a;
