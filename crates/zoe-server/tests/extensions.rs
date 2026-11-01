//! Acceptance test of the negotiation of the ZOE extensions (F13).
//!
//! A ZOE client declares the extensions that it wants as a third argument
//! of `register`: `register(storage_id, read_only, [name, ...])`. The
//! server keeps the names that it knows and serves, and `get_info` gives
//! them back under the key `extensions`, in the order of the declaration.
//! A stock client calls `register` with two arguments, declares nothing,
//! and its `get_info` has no `extensions` key: it never sees an
//! extension. A name that the server does not know is left out, never an
//! error. A third argument that is not a list of strings is an error.

mod common;

use common::{Server, Value, fresh_dir};

/// The keys of `get_info` that a stock client reads.
const INFO_KEYS: [&str; 6] = [
    "length",
    "size",
    "name",
    "supportsUndo",
    "supports_record_iternext",
    "interfaces",
];

fn keys(info: &Value) -> Vec<String> {
    info.as_map()
        .expect("get_info gives a map")
        .iter()
        .map(|(k, _)| k.as_str().unwrap().to_string())
        .collect()
}

fn get(info: &Value, key: &str) -> Option<Value> {
    info.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v.clone())
}

#[test]
fn a_stock_client_sees_no_extension() {
    let dir = fresh_dir("stock");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    conn.register("1", false).unwrap();
    let info = conn.call("get_info", vec![]).unwrap();
    let keys = keys(&info);
    for key in INFO_KEYS {
        assert!(keys.contains(&key.to_string()), "get_info has no {key}");
    }
    assert!(
        !keys.contains(&"extensions".to_string()),
        "a stock client sees extensions"
    );
}

#[test]
fn an_unknown_extension_is_left_out() {
    let dir = fresh_dir("unknown");
    let server = Server::path(&dir.join("store"));
    let mut conn = server.connect();
    conn.call(
        "register",
        vec![
            Value::from("1"),
            Value::Boolean(false),
            Value::Array(vec![Value::from("no_such_extension")]),
        ],
    )
    .unwrap();
    let info = conn.call("get_info", vec![]).unwrap();
    assert_eq!(get(&info, "extensions"), Some(Value::Array(vec![])));
    for key in INFO_KEYS {
        assert!(get(&info, key).is_some(), "get_info has no {key}");
    }
}

#[test]
fn a_declaration_that_is_not_a_list_of_names_is_refused() {
    let dir = fresh_dir("bad");
    let server = Server::path(&dir.join("store"));
    for bad in [
        Value::from("sequences"),
        Value::Array(vec![Value::from(7)]),
        Value::Map(vec![]),
    ] {
        let mut conn = server.connect();
        let refused = conn.call(
            "register",
            vec![Value::from("1"), Value::Boolean(false), bad.clone()],
        );
        assert!(refused.is_err(), "register took {bad:?}");
    }
}

#[test]
fn a_declaration_holds_for_its_connection_only() {
    let dir = fresh_dir("per-connection");
    let server = Server::path(&dir.join("store"));
    let mut declaring = server.connect();
    declaring
        .call(
            "register",
            vec![
                Value::from("1"),
                Value::Boolean(false),
                Value::Array(vec![Value::from("no_such_extension")]),
            ],
        )
        .unwrap();
    let mut stock = server.connect();
    stock.register("1", false).unwrap();
    let info = stock.call("get_info", vec![]).unwrap();
    assert!(get(&info, "extensions").is_none());
}
