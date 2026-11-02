//! The size measure: the scratch delta store and the text index.

use zoe_catalog::text::compat::{Compat, Node, Stats};
use zoe_catalog::text::enhanced::{Options, Translator};
use zoe_catalog::text::index::{self, Mode};
use zoe_catalog::text::overlay;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_default();
    let store = zoe_core::text::TextDeltas::create(path.as_ref(), zoe_core::Compression::None);
    println!("{}", store.is_ok());
    let dir = std::path::PathBuf::from(std::env::args().nth(2).unwrap_or_default());
    for mode in [Mode::Compat, Mode::Enhanced] {
        let Ok((idx, fields)) = index::open(mode, &dir) else { continue };
        let searcher = index::reader(&idx).unwrap().searcher();
        let node = Node::Atom { words: vec!["x".into()] };
        let stats = Stats::count(&searcher, "n_SearchableText").unwrap();
        let mut c = Compat::new(&searcher, fields.text[0], "n_SearchableText", stats);
        println!("{:?}", c.run(&node).unwrap().map(|h| h.len()));
        let mut t = Translator::new(&idx, Options { fields: vec![(fields.text[0], 1.0)], fuzzy: 1, pairs: vec![fields.pairs[0]] });
        let q = t.translate(&node).unwrap();
        println!("{:?}", overlay::enhanced(&searcher, None, &*q, |_| true, 10).unwrap().0);
        let w = idx.writer_with_num_threads::<tantivy::TantivyDocument>(1, 20_000_000);
        println!("{}", w.is_ok());
    }
}
