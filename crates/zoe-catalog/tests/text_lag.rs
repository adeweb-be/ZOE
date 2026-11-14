//! The lag of a catalog that no text index serves (ADR 0060, section 10).
//!
//! A registered catalog whose settings in `zoe.text` name no text index, or
//! do not parse, gets no index. Its lag stays 0: no index falls behind, so
//! no alert must fire.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use zoe_catalog::testing::{self, TextOp};
use zoe_catalog::text::{CatalogSettings, TextConfig};
use zoe_catalog::{Config, Mirrors, Opener, Reads};
use zoe_core::testing::fresh_dir;
use zoe_core::{Flavour, Store};

/// The catalog whose settings name no text index.
const NONE: u64 = 0x100;
/// The catalog whose settings do not parse.
const BROKEN: u64 = 0x200;

fn start(store: &Arc<Store>, dir: &Path) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    let config = Config {
        wait: Duration::from_millis(200),
        text: TextConfig {
            dir: Some(dir.to_owned()),
            read_only: false,
        },
        ..Config::default()
    };
    Mirrors::start(open, store.last_transaction().0, config)
}

#[test]
fn a_catalog_that_no_index_serves_has_no_lag() {
    let dir = fresh_dir("text-no-lag");
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let settings = CatalogSettings::default();
    let text = TextOp::Set {
        index: "SearchableText",
        rid: 1,
        language: "en",
        text: "kalo",
    };
    testing::commit_text(&store, NONE, &[text], Some(&settings), true).unwrap();
    // Settings that do not parse, and a registration key.
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    let key = BROKEN.to_be_bytes();
    txn.kv_set(Flavour::Durable, "zoe.text", &key, b"not json", None)
        .unwrap();
    txn.kv_set(Flavour::Durable, "zoe.catalog", &key, b"", None)
        .unwrap();
    txn.vote().unwrap();
    txn.finish().unwrap();
    let mirrors = start(&store, dir.path());
    let texts = mirrors.texts().expect("the text indexes");
    for catalog in [NONE, BROKEN] {
        assert!(
            texts.wait_for(catalog, Duration::from_secs(30), |c| c.is_some()),
            "{catalog:#x} gets an entry"
        );
    }
    // Time passes, and the lag stays 0.
    std::thread::sleep(Duration::from_millis(300));
    let counts = mirrors.counts();
    for catalog in [NONE, BROKEN] {
        let text = counts
            .texts
            .iter()
            .find(|c| c.catalog == catalog)
            .unwrap_or_else(|| panic!("{catalog:#x} has counts"));
        assert!(!text.ready, "{catalog:#x}");
        assert_eq!(text.lag_seconds, 0.0, "{catalog:#x}");
    }
}
