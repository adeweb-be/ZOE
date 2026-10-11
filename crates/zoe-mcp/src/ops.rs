//! The read-only ops tools: `object_get`, `history`, `diff` and
//! `references`.
//!
//! Each tool calls `zoe-loupe` and gives its JSON as the structured
//! content of the result, and as text too. `object_get` gives the
//! `Object` of Loupe. The other tools give an object with the oid and
//! the list of Loupe: `revisions`, `changes` or `references`. Oids and
//! tids are ZODB-form strings, as Loupe writes them: `0x00`.
//!
//! A tool that fails gives a result with `isError` and a message. The
//! session goes on. Each tool runs on the blocking pool, because the
//! store is synchronous.

use std::fmt::Display;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use zoe_loupe::{Folder, oid_repr, parse_oid, parse_tid, tid_repr};

/// An error of a tool. The client gets it as a result with `isError`.
#[derive(Debug, thiserror::Error)]
enum ToolError {
    /// Loupe fails: a bad id, an oid without a record, a path that leads
    /// to no object, or an error of the store.
    #[error(transparent)]
    Loupe(#[from] zoe_loupe::Error),

    /// `object_get` needs exactly one of `oid` and `path`.
    #[error("give oid or path, and not both")]
    OidOrPath,

    /// The record does not decode, so its references are not known.
    #[error("oid {oid}: the references are not known, because the record does not decode: {error}")]
    NotDecoded {
        /// The oid in ZODB form.
        oid: String,
        /// The refusal of the decoder.
        error: String,
    },

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

/// The arguments of `history` and `references`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct OidArgs {
    /// The oid of the object in ZODB form, for example `0x00`.
    oid: String,
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

/// The MCP server of the read-only ops tools on one store folder.
#[derive(Clone)]
pub(crate) struct Ops {
    folder: Arc<Folder>,
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
        description = "List the outbound persistent references of an object as of the last \
                       transaction: for each one, the oid that it leads to and a JSON pointer \
                       into the state of the object.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn references(&self, Parameters(args): Parameters<OidArgs>) -> CallToolResult {
        self.run(move |folder| references_json(folder, &args)).await
    }
}

#[tool_handler(
    name = "zoe",
    instructions = "The read-only ops tools of one ZOE store. Oids and tids are ZODB-form \
                    strings, for example 0x00. A path starts at root[\"Application\"], for \
                    example /plone/folder. No tool writes to the store."
)]
impl ServerHandler for Ops {}

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

/// Return the outbound references of the object of `args`. A record that
/// does not decode is an error, because an empty list would be wrong.
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rmcp::ServiceExt;
    use tokio::io::{
        AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };
    use zoe_core::{Oid, Store, Tid};

    use super::*;

    /// A text in a pickle: `BINUNICODE`.
    fn text(value: &str) -> Vec<u8> {
        let length = u32::try_from(value.len()).unwrap().to_le_bytes();
        [&b"X"[..], &length, value.as_bytes()].concat()
    }

    /// A persistent reference to `oid` with the class `m.C`.
    fn reference(oid: u64) -> Vec<u8> {
        [&b"C\x08"[..], &oid.to_be_bytes(), b"cm\nC\n\x86Q"].concat()
    }

    /// A record of the class `module.name` with the state pickle `state`.
    fn record(module: &str, name: &str, state: &[u8]) -> Vec<u8> {
        let class = format!("c{module}\n{name}\n.");
        [b"\x80\x03", class.as_bytes(), b"\x80\x03", state, b"."].concat()
    }

    /// A record of the class `m.Doc` with a title.
    fn document(title: &str) -> Vec<u8> {
        record(
            "m",
            "Doc",
            &[&b"}"[..], &text("title"), &text(title), b"s"].concat(),
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
                &text("data"),
                b"}",
                &text("Application"),
                &reference(1),
                b"ss",
            ]
            .concat(),
        );
        let application = record(
            "m",
            "App",
            &[&b"}"[..], &text("site"), &reference(2), b"s"].concat(),
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
    fn the_tools_are_the_four_read_only_ops_tools() {
        let tools = Ops::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, ["diff", "history", "object_get", "references"]);
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
            // A tool that the server does not offer is a protocol error.
            let pack = client.call("pack", json!({})).await;
            assert!(pack["error"].is_object(), "{pack}");

            let reply = client.call("references", json!({"oid": "0x00"})).await;
            assert_eq!(reply["result"]["isError"], false, "{reply}");
            let references = &reply["result"]["structuredContent"]["references"];
            assert_eq!(references[0]["oid"], "0x01", "{reply}");

            // The session ends when the client closes its end.
            drop(client);
            server.await.unwrap();
        });
    }
}
