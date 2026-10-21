//! The read-only ops tools: `object_get`, `history`, `diff`,
//! `references`, `space`, `verify` and `explain_conflict`.
//!
//! Each tool gives its JSON as the structured content of the result, and
//! as text too. Oids and tids are ZODB-form strings, as Loupe writes them:
//! `0x00`.
//!
//! - `object_get`, `history`, `diff`, `references` and `space` call
//!   `zoe-loupe`. `object_get` gives the `Object` of Loupe. `history`,
//!   `diff` and `references` give an object with the oid and the list of
//!   Loupe: `revisions`, `changes` or `references`. `space` gives the
//!   `Space` of Loupe with the newest part of its growth.
//! - `verify` calls `zoe_convert::verify` on the store of the folder and
//!   gives the keys of `zoe verify --format json`.
//! - `explain_conflict` reads the transaction with the iterator of the
//!   core and the neighbours with Loupe's `history`. The module `txn`
//!   gives the rules.
//!
//! Each list of a result has a limit, and the result gives its full
//! count. No result holds a state or pickle bytes, except `object_get`
//! and `diff`, which give Loupe's decoded state.
//!
//! A tool that fails gives a result with `isError` and a message. The
//! session goes on. Each tool runs on the blocking pool, because the
//! store is synchronous.

use std::fmt::Display;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, prompt_handler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use zoe_convert::Verification;
use zoe_core::ZoeError;
use zoe_loupe::{Folder, oid_repr, parse_oid, parse_tid, tid_repr};

use crate::txn;

/// The number of largest records that `space` gives by default.
const LARGEST: usize = 20;

/// The most largest records that `space` gives.
const LARGEST_LIMIT: usize = 1000;

/// The most transactions of the growth that `space` gives: the newest.
const GROWTH_LIMIT: usize = 100;

/// The most findings of each list that `verify` gives.
const FINDING_LIMIT: usize = 100;

/// An error of a tool. The client gets it as a result with `isError`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    /// Loupe fails: a bad id, an oid without a record, a path that leads
    /// to no object, or an error of the store.
    #[error("{}", loupe_message(.0))]
    Loupe(#[from] zoe_loupe::Error),

    /// `object_get` needs exactly one of `oid` and `path`.
    #[error("give oid or path, and not both")]
    OidOrPath,

    /// The record or its references do not decode, so the references
    /// are not known.
    #[error("oid {oid}: the references are not known: {error}")]
    NotDecoded {
        /// The oid in ZODB form.
        oid: String,
        /// The refusal of the decoder.
        error: String,
    },

    /// The store has no transaction with this tid.
    #[error("tid {0}: no transaction")]
    NoTransaction(String),

    /// The store has blob records, but the blob directory of the open has
    /// no layout file. The blob directory of the store is elsewhere.
    #[error(
        "the store has blobs, but the blob directory {0} has no layout file: \
         give the blob directory of the store with --blob-dir (ZOE_BLOB_DIR)"
    )]
    BlobDir(String),

    /// The result does not convert to JSON.
    #[error("cannot write the JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// The arguments of `object_get`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct ObjectGetArgs {
    /// The oid of the object in ZODB form, for example `0x00`. Give `oid`
    /// or `path`.
    oid: Option<String>,
    /// The Plone path of the object, for example `/plone/folder/doc`. It
    /// starts at `root["Application"]`. Give `oid` or `path`.
    path: Option<String>,
    /// A tid in ZODB form. The object shows as it was after the
    /// transaction of this tid. Without it, the object shows as of the
    /// last transaction.
    as_of: Option<String>,
}

/// The arguments of `history`, `references` and the prompt
/// `why_conflict`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct OidArgs {
    /// The oid of the object in ZODB form, for example `0x00`.
    pub(crate) oid: String,
}

/// The arguments of `diff`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    /// The oid of the object in ZODB form, for example `0x00`.
    oid: String,
    /// The tid of the state before, in ZODB form. The state is the one
    /// after the transaction of this tid.
    tid_a: String,
    /// The tid of the state after, in ZODB form. The state is the one
    /// after the transaction of this tid.
    tid_b: String,
}

impl ToolError {
    /// Tell if the error comes from an argument: an id that is not in
    /// ZODB form, an oid without a record, or a tid without a
    /// transaction. Each other error comes from the store or the server.
    pub(crate) fn is_bad_argument(&self) -> bool {
        matches!(
            self,
            ToolError::Loupe(
                zoe_loupe::Error::BadId(_)
                    | zoe_loupe::Error::NoObject { .. }
                    | zoe_loupe::Error::NoRecordAsOf { .. }
                    | zoe_loupe::Error::Store(ZoeError::NotFound { .. })
            ) | ToolError::OidOrPath
                | ToolError::NoTransaction(_)
        )
    }
}

/// The arguments of `verify`: none.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct NoArgs {}

/// The arguments of `space`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct SpaceArgs {
    /// The number of largest records to give: 20 by default, at most
    /// 1000.
    largest: Option<u32>,
}

/// The arguments of `explain_conflict` and of the prompt
/// `explain_transaction`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TidArgs {
    /// The tid of the transaction in ZODB form, for example
    /// `0x03f0b0c0d0e0f000`.
    pub(crate) tid: String,
}

/// The MCP server of the read-only ops tools on one store folder.
#[derive(Clone)]
pub(crate) struct Ops {
    pub(crate) folder: Arc<Folder>,
}

impl Ops {
    /// Return the server of the store folder `folder`.
    pub(crate) fn new(folder: Folder) -> Self {
        Self {
            folder: Arc::new(folder),
        }
    }

    /// Run `work` on the folder in the blocking pool. Return its JSON as a
    /// result, or its error as a result with `isError`.
    async fn run<F>(&self, work: F) -> CallToolResult
    where
        F: FnOnce(&Folder) -> Result<Value, ToolError> + Send + 'static,
    {
        let folder = Arc::clone(&self.folder);
        match tokio::task::spawn_blocking(move || work(&folder)).await {
            Ok(Ok(value)) => CallToolResult::structured(value),
            Ok(Err(error)) => failure(&error),
            // A panic stops the tool, not the session.
            Err(error) => failure(&format!("the tool stopped: {error}")),
        }
    }
}

#[tool_router]
impl Ops {
    /// Get one object of the store.
    #[tool(
        description = "Get one object of the store as decoded JSON: class, oid, serial, size, \
                       state, outbound references, and the refusal of the decoder when the \
                       record does not decode. Give oid or path. Give as_of to see the object \
                       as it was after the transaction of that tid.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn object_get(&self, Parameters(args): Parameters<ObjectGetArgs>) -> CallToolResult {
        self.run(move |folder| object_json(folder, &args)).await
    }

    /// List the revisions of an object.
    #[tool(
        description = "List the revisions of an object, newest first: the tid, the user and \
                       the description of the transaction, and the size of the record. A \
                       deletion has size 0.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn history(&self, Parameters(args): Parameters<OidArgs>) -> CallToolResult {
        self.run(move |folder| history_json(folder, &args)).await
    }

    /// Compare two revisions of an object.
    #[tool(
        description = "Compare the decoded state of an object as of tid_a with its state as of \
                       tid_b. Each change has a JSON pointer into the state, the value before \
                       and the value after. A value that one side does not have is left out.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn diff(&self, Parameters(args): Parameters<DiffArgs>) -> CallToolResult {
        self.run(move |folder| diff_json(folder, &args)).await
    }

    /// List the outbound references of an object.
    #[tool(
        description = "List the strong outbound persistent references of an object as of the \
                       last transaction, as ZODB's referencesf gives them: for each one, the oid \
                       that it leads to and a JSON pointer into the state of the object. A \
                       reference inside an opaque value points to that value.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn references(&self, Parameters(args): Parameters<OidArgs>) -> CallToolResult {
        self.run(move |folder| references_json(folder, &args)).await
    }

    /// Measure the space of the store.
    #[tool(
        description = "Measure the store as of its last transaction, as Loupe's space screen \
                       does: for each class the number of current records and their bytes, \
                       the blob files, the largest current records (20 by default, give \
                       largest for more, at most 1000), and the bytes that each of the newest \
                       100 transactions wrote. transactions gives the number of all \
                       transactions. The call reads every current record.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn space(&self, Parameters(args): Parameters<SpaceArgs>) -> CallToolResult {
        self.run(move |folder| space_json(folder, &args)).await
    }

    /// Check the store as `zoe verify` does.
    #[tool(
        description = "Check the store as zoe verify does: the checksum of each frame, each \
                       blob file, the persistent references of each current record and its \
                       class. The lists checksums, blobs, references and pickles hold the \
                       findings, at most 100 each; totals gives the full count of each list, \
                       and findings the sum. classes gives the count of each class. The call \
                       reads the whole store.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn verify(&self, Parameters(NoArgs {}): Parameters<NoArgs>) -> CallToolResult {
        self.run(verify_json).await
    }

    /// Show the transaction of a tid and the neighbours of its objects.
    #[tool(
        description = "Show what a transaction wrote and who else wrote the same objects. For \
                       the transaction of tid: its user and description, and each object that \
                       it wrote (at most 100, records gives the count) with its class and \
                       size. For each object, before and after are the revisions just before \
                       and just after it, with their tid, user and description. The store \
                       keeps no record of conflicts: this is the history only.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn explain_conflict(&self, Parameters(args): Parameters<TidArgs>) -> CallToolResult {
        self.run(move |folder| txn::explain(folder, parse_tid(&args.tid)?))
            .await
    }
}

#[tool_handler(
    name = "zoe",
    instructions = "The read-only ops tools and prompts of one ZOE store. Oids and tids are \
                    ZODB-form strings, for example 0x00. A path starts at \
                    root[\"Application\"], for example /plone/folder. No tool writes to the \
                    store."
)]
#[prompt_handler]
impl ServerHandler for Ops {}

/// Return the message of an error of Loupe. Oids and tids are in ZODB
/// form, as the tools take them.
fn loupe_message(error: &zoe_loupe::Error) -> String {
    match error {
        zoe_loupe::Error::Store(ZoeError::NotFound { oid }) => {
            format!("oid {}: no record", oid_repr(*oid))
        }
        zoe_loupe::Error::Store(ZoeError::CorruptRecord { oid, tid }) => {
            format!(
                "oid {}: corrupt record, tid {}",
                oid_repr(*oid),
                tid_repr(*tid)
            )
        }
        error => error.to_string(),
    }
}

/// Return a result with `isError` and the message `message`.
fn failure(message: &dyn Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_string())])
}

/// Return the object of `args` as Loupe's `Object`.
fn object_json(folder: &Folder, args: &ObjectGetArgs) -> Result<Value, ToolError> {
    let tid = match &args.as_of {
        Some(tid) => parse_tid(tid)?,
        None => folder.last_tid(),
    };
    let view = folder.as_of(tid);
    let object = match (&args.oid, &args.path) {
        (Some(oid), None) => view.object(parse_oid(oid)?)?,
        (None, Some(path)) => view.resolve(path)?,
        _ => return Err(ToolError::OidOrPath),
    };
    Ok(serde_json::to_value(object)?)
}

/// Return the revisions of the object of `args`, newest first.
fn history_json(folder: &Folder, args: &OidArgs) -> Result<Value, ToolError> {
    let oid = parse_oid(&args.oid)?;
    let revisions = serde_json::to_value(folder.history(oid)?)?;
    Ok(json!({"oid": oid_repr(oid), "revisions": revisions}))
}

/// Return the changes of the object of `args` from `tid_a` to `tid_b`.
fn diff_json(folder: &Folder, args: &DiffArgs) -> Result<Value, ToolError> {
    let oid = parse_oid(&args.oid)?;
    let (before, after) = (parse_tid(&args.tid_a)?, parse_tid(&args.tid_b)?);
    let changes = serde_json::to_value(folder.diff(oid, before, after)?)?;
    Ok(json!({
        "oid": oid_repr(oid),
        "tid_a": tid_repr(before),
        "tid_b": tid_repr(after),
        "changes": changes,
    }))
}

/// Return the outbound references of the object of `args`. A record whose
/// state or references do not decode is an error, because an empty list
/// would be wrong.
fn references_json(folder: &Folder, args: &OidArgs) -> Result<Value, ToolError> {
    let object = folder.object(parse_oid(&args.oid)?)?;
    if let Some(error) = object.error {
        return Err(ToolError::NotDecoded {
            oid: oid_repr(object.oid),
            error,
        });
    }
    Ok(json!({
        "oid": oid_repr(object.oid),
        "serial": tid_repr(object.serial),
        "references": serde_json::to_value(object.references)?,
    }))
}

/// Return the space of the store as Loupe's `Space`, with the newest
/// [`GROWTH_LIMIT`] transactions of the growth. `transactions` gives the
/// number of all transactions, and `tid` the last tid.
fn space_json(folder: &Folder, args: &SpaceArgs) -> Result<Value, ToolError> {
    let largest = args
        .largest
        .map_or(LARGEST, |n| (n as usize).min(LARGEST_LIMIT));
    let mut space = folder.space(largest)?;
    let transactions = space.growth.len();
    space
        .growth
        .drain(..transactions.saturating_sub(GROWTH_LIMIT));
    let mut value = serde_json::to_value(space)?;
    value["tid"] = json!(tid_repr(folder.last_tid()));
    value["transactions"] = json!(transactions);
    Ok(value)
}

/// Check the store of `folder` with `zoe_convert::verify`. A store with
/// blob records whose blob directory has no layout file is an error, as
/// for `zoe verify`.
fn verify_json(folder: &Folder) -> Result<Value, ToolError> {
    let store = folder.store();
    store.check_blob_dir().map_err(|error| match error {
        ZoeError::MissingBlobDir { path } => ToolError::BlobDir(path.display().to_string()),
        error => zoe_loupe::Error::from(error).into(),
    })?;
    Ok(verification_json(&zoe_convert::verify(store, None)))
}

/// Return the report of `zoe_convert::verify` with the keys of
/// `zoe verify --format json`, and oids and tids in ZODB form. Each list
/// holds at most [`FINDING_LIMIT`] findings. `totals` gives the full
/// count of each list, `findings` their sum, and `walked` the counts of
/// the walk.
fn verification_json(report: &Verification) -> Value {
    fn first<T>(items: &[T], json: impl Fn(&T) -> Value) -> Vec<Value> {
        items.iter().take(FINDING_LIMIT).map(json).collect()
    }
    let checksums = first(&report.checksums, |p| {
        json!({
            "oid": p.oid.map(oid_repr),
            "tid": p.tid.map(tid_repr),
            "error": p.error,
        })
    });
    let blobs = first(
        &report.blob_files,
        |p| json!({"oid": oid_repr(p.oid), "tid": tid_repr(p.tid), "error": p.error}),
    );
    let references = first(
        &report.references,
        |d| json!({"oid": oid_repr(d.oid), "tid": tid_repr(d.tid), "missing": oid_repr(d.missing)}),
    );
    let pickles = first(
        &report.pickles,
        |p| json!({"oid": oid_repr(p.oid), "tid": tid_repr(p.tid), "error": p.error}),
    );
    json!({
        "findings": report.problems(),
        "totals": {
            "checksums": report.checksums.len(),
            "blobs": report.blob_files.len(),
            "references": report.references.len(),
            "pickles": report.pickles.len(),
        },
        "walked": {
            "transactions": report.transactions,
            "sequences": report.sequences,
            "records": report.records,
            "objects": report.objects,
            "blobs": report.blobs,
        },
        "checksums": checksums,
        "blobs": blobs,
        "references": references,
        "pickles": pickles,
        "classes": report.classes,
        "unknown_classes": report.unknown_classes,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use rmcp::ServiceExt;
    use tokio::io::{
        AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };
    use zoe_core::{Oid, Store, Tid};
    use zoe_pickle::testing::{record, text as text_pickle};

    use super::*;

    /// A persistent reference to `oid` with the class `m.C`.
    fn reference(oid: u64) -> Vec<u8> {
        zoe_pickle::testing::reference(oid, "m", "C")
    }

    /// A record of the class `m.Doc` with a title.
    fn document(title: &str) -> Vec<u8> {
        record(
            "m",
            "Doc",
            &[&b"}"[..], &text_pickle("title"), &text_pickle(title), b"s"].concat(),
        )
    }

    /// A store in a new folder. The root holds the application, the
    /// application holds the document `site`, and a second transaction
    /// renames the document. Oid 3 has a record that does not decode.
    /// Return the folder and the two tids.
    fn store(name: &str) -> (PathBuf, Tid, Tid) {
        let dir = std::env::temp_dir().join(format!("zoe-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = record(
            "persistent.mapping",
            "PersistentMapping",
            &[
                &b"}"[..],
                &text_pickle("data"),
                b"}",
                &text_pickle("Application"),
                &reference(1),
                b"ss",
            ]
            .concat(),
        );
        let application = record(
            "m",
            "App",
            &[&b"}"[..], &text_pickle("site"), &reference(2), b"s"].concat(),
        );
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
        txn.store(Oid(0), Tid(0), &root).unwrap();
        txn.store(Oid(1), Tid(0), &application).unwrap();
        txn.store(Oid(2), Tid(0), &document("first")).unwrap();
        txn.store(Oid(3), Tid(0), b"not a pickle").unwrap();
        txn.vote().unwrap();
        let first = txn.finish().unwrap();
        let mut txn = store.tpc_begin(b"editor", b"rename", b"").unwrap();
        txn.store(Oid(2), first, &document("second")).unwrap();
        txn.vote().unwrap();
        let second = txn.finish().unwrap();
        (dir, first, second)
    }

    fn oid_args(oid: &str) -> OidArgs {
        OidArgs {
            oid: oid.to_owned(),
        }
    }

    #[test]
    fn object_get_gives_the_object_of_loupe() {
        let (dir, first, _) = store("object");
        let folder = Folder::open(&dir).unwrap();
        let by_oid = ObjectGetArgs {
            oid: Some("0x02".to_owned()),
            ..ObjectGetArgs::default()
        };
        let expected = serde_json::to_value(folder.object(Oid(2)).unwrap()).unwrap();
        assert_eq!(object_json(&folder, &by_oid).unwrap(), expected);
        assert!(expected.to_string().contains("second"), "{expected}");

        let by_path = ObjectGetArgs {
            path: Some("/site".to_owned()),
            ..ObjectGetArgs::default()
        };
        assert_eq!(object_json(&folder, &by_path).unwrap(), expected);

        let then = ObjectGetArgs {
            as_of: Some(tid_repr(first)),
            ..by_path
        };
        let then = object_json(&folder, &then).unwrap();
        assert_eq!(then["serial"], tid_repr(first));
        assert!(then.to_string().contains("first"), "{then}");
    }

    #[test]
    fn object_get_needs_oid_or_path() {
        let (dir, _, _) = store("oid-or-path");
        let folder = Folder::open(&dir).unwrap();
        let both = ObjectGetArgs {
            oid: Some("0x02".to_owned()),
            path: Some("/site".to_owned()),
            as_of: None,
        };
        for args in [both, ObjectGetArgs::default()] {
            let error = object_json(&folder, &args).err();
            assert!(matches!(error, Some(ToolError::OidOrPath)), "{error:?}");
        }
    }

    #[test]
    fn ids_that_are_not_in_zodb_form_are_errors() {
        let (dir, _, _) = store("bad-ids");
        let folder = Folder::open(&dir).unwrap();
        let bad_oid = ObjectGetArgs {
            oid: Some("2".to_owned()),
            ..ObjectGetArgs::default()
        };
        let bad_tid = ObjectGetArgs {
            oid: Some("0x02".to_owned()),
            as_of: Some("latest".to_owned()),
            ..ObjectGetArgs::default()
        };
        for args in [bad_oid, bad_tid] {
            let error = object_json(&folder, &args).err();
            assert!(
                matches!(error, Some(ToolError::Loupe(zoe_loupe::Error::BadId(_)))),
                "{error:?}"
            );
        }
    }

    #[test]
    fn history_gives_the_revisions_of_loupe() {
        let (dir, first, second) = store("history");
        let folder = Folder::open(&dir).unwrap();
        let history = history_json(&folder, &oid_args("0x02")).unwrap();
        assert_eq!(history["oid"], "0x02");
        let expected = serde_json::to_value(folder.history(Oid(2)).unwrap()).unwrap();
        assert_eq!(history["revisions"], expected);
        let tids: Vec<&Value> = history["revisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|revision| &revision["tid"])
            .collect();
        assert_eq!(tids, [&json!(tid_repr(second)), &json!(tid_repr(first))]);
    }

    #[test]
    fn diff_gives_the_changes_of_loupe() {
        let (dir, first, second) = store("diff");
        let folder = Folder::open(&dir).unwrap();
        let args = DiffArgs {
            oid: "0x02".to_owned(),
            tid_a: tid_repr(first),
            tid_b: tid_repr(second),
        };
        let diff = diff_json(&folder, &args).unwrap();
        let expected = folder.diff(Oid(2), first, second).unwrap();
        assert_eq!(diff["changes"], serde_json::to_value(&expected).unwrap());
        assert_eq!(expected.len(), 1, "{diff}");
        assert_eq!(diff["tid_a"], tid_repr(first));
        assert_eq!(diff["tid_b"], tid_repr(second));
    }

    #[test]
    fn references_gives_the_references_of_loupe() {
        let (dir, _, _) = store("references");
        let folder = Folder::open(&dir).unwrap();
        let references = references_json(&folder, &oid_args("0x01")).unwrap();
        let object = folder.object(Oid(1)).unwrap();
        assert_eq!(
            references["references"],
            serde_json::to_value(&object.references).unwrap()
        );
        assert_eq!(references["references"][0]["oid"], "0x02");
    }

    #[test]
    fn the_references_of_a_record_that_does_not_decode_are_an_error() {
        let (dir, _, _) = store("not-decoded");
        let folder = Folder::open(&dir).unwrap();
        // object_get shows the refusal in the object, as Loupe does.
        let object = ObjectGetArgs {
            oid: Some("0x03".to_owned()),
            ..ObjectGetArgs::default()
        };
        let object = object_json(&folder, &object).unwrap();
        assert!(object["error"].is_string(), "{object}");
        let error = references_json(&folder, &oid_args("0x03")).err();
        assert!(
            matches!(error, Some(ToolError::NotDecoded { .. })),
            "{error:?}"
        );
    }

    /// Run `future` to its end on a runtime with one thread.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn the_tools_are_the_read_only_ops_tools() {
        let tools = Ops::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "diff",
                "explain_conflict",
                "history",
                "object_get",
                "references",
                "space",
                "verify"
            ]
        );
        for tool in &tools {
            let annotations = tool.annotations.as_ref().unwrap();
            assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.name);
        }
    }

    #[test]
    fn a_tool_error_is_a_result_with_is_error() {
        let (dir, _, _) = store("is-error");
        let ops = Ops::new(Folder::open(&dir).unwrap());
        let missing = block_on(ops.history(Parameters(oid_args("0x99"))));
        assert_eq!(missing.is_error, Some(true));
        assert!(missing.structured_content.is_none());
        assert_eq!(text(&missing), "oid 0x99: no record");
        let found = block_on(ops.history(Parameters(oid_args("0x02"))));
        assert_eq!(found.is_error, Some(false));
        let content = found.structured_content.unwrap();
        assert_eq!(content["revisions"].as_array().unwrap().len(), 2);
    }

    /// A client of the server over an in-memory pipe. It writes one
    /// JSON-RPC message on each line and reads the replies.
    struct Client {
        write: WriteHalf<DuplexStream>,
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        next_id: u64,
    }

    impl Client {
        async fn send(&mut self, message: Value) {
            let line = format!("{message}\n");
            self.write.write_all(line.as_bytes()).await.unwrap();
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.send(request).await;
            self.reply(id).await
        }

        async fn request_without_params(&mut self, method: &str) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method}))
                .await;
            self.reply(id).await
        }

        async fn reply(&mut self, id: u64) -> Value {
            loop {
                let line = self.lines.next_line().await.unwrap().unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["id"] == id {
                    return message;
                }
            }
        }

        async fn call(&mut self, tool: &str, arguments: Value) -> Value {
            let params = json!({"name": tool, "arguments": arguments});
            self.request("tools/call", params).await
        }
    }

    #[test]
    fn bad_arguments_are_tool_errors_and_the_session_goes_on() {
        let (dir, _, _) = store("session");
        let ops = Ops::new(Folder::open(&dir).unwrap());
        block_on(async move {
            let (client, server) = tokio::io::duplex(1 << 16);
            let server = tokio::spawn(async move {
                let service = ops.serve(server).await.unwrap();
                service.waiting().await.unwrap();
            });
            let (read, write) = tokio::io::split(client);
            let mut client = Client {
                write,
                lines: BufReader::new(read).lines(),
                next_id: 0,
            };
            let init = client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"}
                    }),
                )
                .await;
            assert_eq!(init["result"]["serverInfo"]["name"], "zoe", "{init}");
            let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
            client.send(initialized).await;

            let bad = [
                json!({"oid": 2}),
                json!({"oid": "0x02", "colour": "red"}),
                json!({}),
            ];
            for arguments in bad {
                let reply = client.call("history", arguments.clone()).await;
                assert_eq!(reply["result"]["isError"], true, "{arguments}: {reply}");
            }
            // verify takes no argument, so it takes no list of classes.
            let reply = client.call("verify", json!({"classes": ["m.C"]})).await;
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            let reply = client.call("verify", json!({})).await;
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            // A tool that the server does not offer is a protocol error.
            let pack = client.call("pack", json!({})).await;
            assert_eq!(pack["error"]["code"], -32602, "{pack}");
            // So is a call without its params.
            let bare = client.request_without_params("tools/call").await;
            assert_eq!(bare["error"]["code"], -32601, "{bare}");
            // A message of the wrong shape gets an error.
            let wrong = b"{\"jsonrpc\": \"2.0\", \"id\": 77, \"method\": 42}\n";
            client.write.write_all(wrong).await.unwrap();
            let next = client.lines.next_line();
            let line = tokio::time::timeout(Duration::from_secs(10), next).await;
            let line = line.unwrap().unwrap().unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["error"]["code"], -32600, "{reply}");
            // rmcp skips a line that is not JSON, and sends no reply.
            client.write.write_all(b"not json\n").await.unwrap();

            let reply = client.call("references", json!({"oid": "0x00"})).await;
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let references = &reply["result"]["structuredContent"]["references"];
            assert_eq!(references[0]["oid"], "0x01", "{reply}");

            // The session ends when the client closes its end.
            drop(client);
            server.await.unwrap();
        });
    }

    /// The text of the first content of a result.
    fn text(result: &CallToolResult) -> &str {
        let content = result.content.first().and_then(|content| content.as_text());
        content.map(|text| text.text.as_str()).unwrap_or_default()
    }

    #[test]
    fn a_panic_in_a_tool_is_a_result_with_is_error_and_the_session_goes_on() {
        let (dir, _, _) = store("panic");
        let ops = Ops::new(Folder::open(&dir).unwrap());
        let work = |_: &Folder| -> Result<Value, ToolError> { panic!("a bug in a tool") };
        let panicked = block_on(ops.run(work));
        assert_eq!(panicked.is_error, Some(true));
        assert!(
            text(&panicked).starts_with("the tool stopped"),
            "{panicked:?}"
        );
        // The server still answers.
        let found = block_on(ops.history(Parameters(oid_args("0x02"))));
        assert_eq!(found.is_error, Some(false));
    }

    #[test]
    fn references_include_those_inside_a_plain_object() {
        // The root holds 1, a plain object that holds 2, and 3. ZODB's
        // `referencesf` gives 1, 2 and 3, and pack keeps 2.
        let dir = std::env::temp_dir().join(format!("zoe-mcp-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plain = [
            &b"cm\nPlain\n)\x81}"[..],
            &text_pickle("x"),
            &reference(2),
            b"sb",
        ]
        .concat();
        let state = [
            &b"}("[..],
            &text_pickle("a"),
            &reference(1),
            &text_pickle("b"),
            &plain,
            &text_pickle("c"),
            &reference(3),
            b"u",
        ]
        .concat();
        let store = Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(0), Tid(0), &record("m", "Root", &state))
            .unwrap();
        for oid in 1..=3 {
            txn.store(Oid(oid), Tid(0), &document("child")).unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);

        let folder = Folder::open(&dir).unwrap();
        let references = references_json(&folder, &oid_args("0x00")).unwrap();
        let found: Vec<(&str, &str)> = references["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["oid"].as_str().unwrap(), r["at"].as_str().unwrap()))
            .collect();
        assert_eq!(found, [("0x01", "/a"), ("0x02", "/b"), ("0x03", "/c")]);
    }

    #[test]
    fn space_gives_the_space_of_loupe_with_the_newest_growth() {
        let (dir, _, second) = store("space");
        let store = Store::open(&dir).unwrap();
        for _ in 0..GROWTH_LIMIT {
            let serial = store.last_transaction();
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(Oid(2), serial, &document("again")).unwrap();
            txn.vote().unwrap();
            txn.finish().unwrap();
        }
        drop(store);
        let folder = Folder::open(&dir).unwrap();
        let loupe = folder.space(LARGEST).unwrap();
        let space = space_json(&folder, &SpaceArgs::default()).unwrap();
        assert_eq!(
            space["by_class"],
            serde_json::to_value(&loupe.by_class).unwrap()
        );
        assert_eq!(
            space["largest"],
            serde_json::to_value(&loupe.largest).unwrap()
        );
        assert_eq!(space["tid"], tid_repr(folder.last_tid()));
        assert_eq!(space["transactions"], GROWTH_LIMIT + 2);
        let growth = space["growth"].as_array().unwrap();
        assert_eq!(growth.len(), GROWTH_LIMIT);
        // The growth keeps the newest transactions, in tid order.
        assert!(growth[0]["tid"].as_str().unwrap() > tid_repr(second).as_str());
        assert_eq!(growth[GROWTH_LIMIT - 1]["tid"], tid_repr(folder.last_tid()));

        let one = SpaceArgs { largest: Some(1) };
        let one = space_json(&folder, &one).unwrap();
        assert_eq!(one["largest"].as_array().unwrap().len(), 1);
        let many = SpaceArgs {
            largest: Some(u32::MAX),
        };
        assert!(space_json(&folder, &many).is_ok());
    }

    #[test]
    fn verify_gives_the_findings_in_zodb_form() {
        let (dir, first, _) = store("verify");
        let folder = Folder::open(&dir).unwrap();
        let report = verify_json(&folder).unwrap();
        // Oid 3 holds a record that does not decode.
        assert_eq!(report["findings"], 1, "{report}");
        assert_eq!(report["totals"]["pickles"], 1);
        assert_eq!(report["pickles"][0]["oid"], "0x03");
        assert_eq!(report["pickles"][0]["tid"], tid_repr(first));
        assert_eq!(report["references"], json!([]));
        assert_eq!(report["checksums"], json!([]));
        assert_eq!(report["blobs"], json!([]));
        assert_eq!(report["unknown_classes"], json!([]));
        assert_eq!(report["classes"]["m.Doc"], 1);
        assert_eq!(report["walked"]["transactions"], 2);
        assert_eq!(report["walked"]["objects"], 4);
    }

    #[test]
    fn each_list_of_verify_has_a_limit_and_a_total() {
        use zoe_convert::{Dangling, FrameProblem};
        let report = Verification {
            checksums: vec![FrameProblem {
                oid: None,
                tid: Some(Tid(5)),
                error: "corrupt transaction row".to_owned(),
            }],
            references: (0..=FINDING_LIMIT as u64)
                .map(|missing| Dangling {
                    oid: Oid(0),
                    tid: Tid(3),
                    missing: Oid(0x100 + missing),
                })
                .collect(),
            ..Verification::default()
        };
        let json = verification_json(&report);
        assert_eq!(json["findings"], FINDING_LIMIT + 2);
        assert_eq!(json["totals"]["references"], FINDING_LIMIT + 1);
        let references = json["references"].as_array().unwrap();
        assert_eq!(references.len(), FINDING_LIMIT);
        assert_eq!(references[0]["missing"], "0x0100");
        assert_eq!(references[0]["tid"], "0x03");
        assert_eq!(json["checksums"][0]["oid"], Value::Null);
        assert_eq!(json["checksums"][0]["tid"], "0x05");
    }

    #[test]
    fn verify_with_the_blob_directory_elsewhere_is_an_error() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("zoe-mcp-blobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (dir, blobs) = (root.join("store"), root.join("elsewhere"));
        let options = zoe_core::Options {
            blob_dir: Some(blobs),
            ..zoe_core::Options::default()
        };
        let store = Store::open_with(&dir, options).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let mut file = store.new_blob_file().unwrap();
        file.write_all(b"blob").unwrap();
        let blob = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";
        txn.store_blob(Oid(1), Tid(0), blob, file).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);

        let folder = Folder::open(&dir).unwrap();
        let error = verify_json(&folder).unwrap_err();
        assert!(matches!(error, ToolError::BlobDir(_)), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("has no layout file"), "{message}");
        assert!(message.contains("--blob-dir"), "{message}");
    }
}
