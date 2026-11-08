//! An empty text counts as a document of its text index, as in ZCTextIndex
//! (ADR 0060, section 14): `text_info` counts it, in the index and among
//! the documents that changed after the tid of the index.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zoe_catalog::testing::{self, TextOp};
use zoe_catalog::text::{CatalogSettings, IndexSettings, TextConfig, engine};
use zoe_catalog::{Config, Mirrors, Opener, Reads};
use zoe_core::Store;
use zoe_core::testing::fresh_dir;

const CATALOG: u64 = 0x100;
const WAIT: Duration = Duration::from_secs(30);

fn start(store: &Arc<Store>, dir: &Path) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let config = Config {
        wait: WAIT,
        text: TextConfig {
            dir: Some(dir.to_owned()),
            read_only: false,
        },
        ..Config::default()
    };
    Mirrors::start(open, store.last_transaction().0, config)
}

fn set(rid: i32, text: &str) -> TextOp<'_> {
    TextOp::Set {
        index: "Title",
        rid,
        language: "en",
        text,
    }
}

fn docs(mirrors: &Mirrors, store: &Store) -> u64 {
    let tid = store.last_transaction().0 + 1;
    engine::text_info(mirrors.texts(), store, CATALOG, "Title", tid, None)
        .expect("a call of the right form")
        .expect("an answer")
        .docs
}

#[test]
fn an_empty_text_counts_as_a_document() {
    let dir = fresh_dir("text-empty");
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let mut settings = CatalogSettings::default();
    settings
        .indexes
        .insert("Title".into(), IndexSettings::enhanced(&["en"]));
    // A word, an empty text, and a text of stop words only.
    let ops = [set(1, "kalo"), set(2, ""), set(3, "the")];
    let tid = testing::commit_text(&store, CATALOG, &ops, Some(&settings), true).unwrap();
    let mirrors = start(&store, dir.path());
    let texts = mirrors.texts().expect("the text indexes");
    assert!(texts.wait_for(CATALOG, WAIT, |c| {
        c.is_some_and(|c| c.ready && c.tid >= tid)
    }));
    assert_eq!(docs(&mirrors, &store), 3);
    // A commit that the index does not hold yet counts too.
    testing::commit_text(&store, CATALOG, &[set(4, "")], None, false).unwrap();
    assert_eq!(docs(&mirrors, &store), 4);
    // An unindex leaves the count.
    let gone = TextOp::Unindex {
        index: "Title",
        rid: 2,
    };
    testing::commit_text(&store, CATALOG, &[gone], None, false).unwrap();
    assert_eq!(docs(&mirrors, &store), 3);
}
