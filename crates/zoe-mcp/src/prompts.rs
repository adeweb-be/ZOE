//! The prompts: `explain_transaction`, `why_conflict` and
//! `summarise_changes`.
//!
//! Each prompt gives one user message: a request to the agent and the
//! JSON data of the request. The data is what the tools give for it:
//!
//! - `explain_transaction(tid)`: the result of `explain_conflict` for
//!   `tid`.
//! - `why_conflict(oid)`: the class of the object and its newest
//!   revisions, from `history`.
//! - `summarise_changes(since)`: the transactions after `since`, oldest
//!   first, each with its user, its description and the classes and oids
//!   of its records.
//!
//! The data has the limits of the module `txn`. A bad argument is a
//! JSON-RPC error with the code -32602.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::schemars::JsonSchema;
use rmcp::{prompt, prompt_router};
use serde::Deserialize;
use serde_json::Value;
use zoe_loupe::{Folder, parse_oid, parse_tid};

use crate::ops::{OidArgs, Ops, TidArgs, ToolError};
use crate::txn;

/// The arguments of `summarise_changes`.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct SinceArgs {
    /// A tid in ZODB form. The summary covers the transactions after it.
    since: String,
}

/// Return one user message: `request`, then `data` as JSON in a fenced
/// block.
///
/// The fence is longer than each run of backticks in the JSON. So a text
/// of the store, such as a description, cannot close the block.
fn message(request: &str, data: &Value) -> Vec<PromptMessage> {
    let data = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
    let longest = data.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let text = format!(
        "{request}\n\nThe data, from the read-only tools of the store:\n\n{fence}json\n{data}\n{fence}"
    );
    vec![PromptMessage::new_text(Role::User, text)]
}

impl Ops {
    /// Run `work` on the folder in the blocking pool. Return its JSON, or
    /// its error as a JSON-RPC error: -32602 for a bad argument, -32603
    /// for the other errors.
    async fn data<F>(&self, work: F) -> Result<Value, ErrorData>
    where
        F: FnOnce(&Folder) -> Result<Value, ToolError> + Send + 'static,
    {
        let folder = Arc::clone(&self.folder);
        match tokio::task::spawn_blocking(move || work(&folder)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.is_bad_argument() => {
                Err(ErrorData::invalid_params(error.to_string(), None))
            }
            Ok(Err(error)) => Err(ErrorData::internal_error(error.to_string(), None)),
            Err(error) => Err(ErrorData::internal_error(
                format!("the prompt stopped: {error}"),
                None,
            )),
        }
    }
}

#[prompt_router(vis = "pub(crate)")]
impl Ops {
    /// Explain this transaction.
    #[prompt(
        name = "explain_transaction",
        description = "Explain the transaction of a tid: who wrote it, what it wrote, and the \
                       revisions before and after each object that it wrote."
    )]
    async fn explain_transaction(
        &self,
        Parameters(args): Parameters<TidArgs>,
    ) -> Result<Vec<PromptMessage>, ErrorData> {
        let data = self
            .data(move |folder| txn::explain(folder, parse_tid(&args.tid)?))
            .await?;
        Ok(message(
            "Explain this transaction of a ZODB store: who committed it, what it changed, and \
             why. Name the objects by class and oid. Use the revisions before and after each \
             object to place it among the other writers. Call the tool diff for the changes \
             of an object.",
            &data,
        ))
    }

    /// Why did this object conflict.
    #[prompt(
        name = "why_conflict",
        description = "Explain why an object can get a ConflictError: its class and the \
                       transactions that wrote it last."
    )]
    async fn why_conflict(
        &self,
        Parameters(args): Parameters<OidArgs>,
    ) -> Result<Vec<PromptMessage>, ErrorData> {
        let data = self
            .data(move |folder| txn::recent_writers(folder, parse_oid(&args.oid)?))
            .await?;
        Ok(message(
            "Explain why this object of a ZODB store can conflict. A ConflictError comes when \
             two transactions write the same object at the same time. The store keeps no \
             record of the conflicts, so read the newest revisions: which users and which \
             actions write the object, and how often. Say what can make the writes collide, \
             and how the application can avoid it. Call the tool explain_conflict with the tid \
             of a revision to see what else that transaction wrote.",
            &data,
        ))
    }

    /// Summarise what changed since a tid.
    #[prompt(
        name = "summarise_changes",
        description = "Summarise the transactions after a tid: their users, descriptions, \
                       and the classes and objects that they wrote."
    )]
    async fn summarise_changes(
        &self,
        Parameters(args): Parameters<SinceArgs>,
    ) -> Result<Vec<PromptMessage>, ErrorData> {
        let data = self
            .data(move |folder| txn::changes_since(folder, parse_tid(&args.since)?))
            .await?;
        Ok(message(
            "Summarise what changed in this ZODB store since the tid in since. Group the \
             transactions by user and by action, and name the classes that changed most. When \
             more is true, the list stops before the last transaction: say so.",
            &data,
        ))
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::{Oid, Store, Tid};
    use zoe_loupe::tid_repr;

    use super::*;

    /// A store with two transactions on oid 0: `create` by `admin`, then
    /// `rename` by `editor`. Return the server and the two tids.
    fn ops(name: &str) -> (Ops, Tid, Tid) {
        let dir =
            std::env::temp_dir().join(format!("zoe-mcp-prompt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let record = b"\x80\x03cm\nDoc\n.\x80\x03}.";
        let mut txn = store.tpc_begin(b"admin", b"create", b"").unwrap();
        txn.store(Oid(0), Tid(0), record).unwrap();
        txn.vote().unwrap();
        let first = txn.finish().unwrap();
        let mut txn = store.tpc_begin(b"editor", b"rename", b"").unwrap();
        txn.store(Oid(0), first, record).unwrap();
        txn.vote().unwrap();
        let second = txn.finish().unwrap();
        drop(store);
        (Ops::new(Folder::open(&dir).unwrap()), first, second)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// The text of the one message of a prompt.
    fn text(messages: &[PromptMessage]) -> String {
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        let text = messages[0].content.as_text().unwrap();
        text.text.clone()
    }

    #[test]
    fn the_prompts_are_the_three_ops_prompts() {
        let prompts = Ops::prompt_router().list_all();
        let mut names: Vec<&str> = prompts.iter().map(|p| p.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["explain_transaction", "summarise_changes", "why_conflict"]
        );
        for prompt in &prompts {
            let arguments = prompt.arguments.as_ref().unwrap();
            assert_eq!(arguments.len(), 1, "{}", prompt.name);
            assert_eq!(arguments[0].required, Some(true), "{}", prompt.name);
        }
    }

    #[test]
    fn each_prompt_carries_the_data_of_its_tool() {
        let (ops, first, second) = ops("data");
        let folder = Arc::clone(&ops.folder);
        let tid = TidArgs {
            tid: tid_repr(second),
        };
        let got = text(&block_on(ops.explain_transaction(Parameters(tid))).unwrap());
        let data = serde_json::to_string_pretty(&txn::explain(&folder, second).unwrap()).unwrap();
        assert!(got.contains(&data), "{got}");

        let oid = OidArgs {
            oid: "0x00".to_owned(),
        };
        let got = text(&block_on(ops.why_conflict(Parameters(oid))).unwrap());
        let data = txn::recent_writers(&folder, Oid(0)).unwrap();
        assert!(
            got.contains(&serde_json::to_string_pretty(&data).unwrap()),
            "{got}"
        );

        let since = SinceArgs {
            since: tid_repr(first),
        };
        let got = text(&block_on(ops.summarise_changes(Parameters(since))).unwrap());
        let data = txn::changes_since(&folder, first).unwrap();
        assert!(
            got.contains(&serde_json::to_string_pretty(&data).unwrap()),
            "{got}"
        );
        assert!(
            got.contains("rename") && !got.contains("\"create\""),
            "{got}"
        );
    }

    #[test]
    fn a_text_of_the_store_cannot_close_the_fence() {
        let data = serde_json::json!({"description": "a ``` b ```` c"});
        let got = text(&message("Explain.", &data));
        let body = got.split_once("store:\n\n").unwrap().1;
        assert!(body.starts_with("`````json\n"), "{got}");
        assert!(body.ends_with("\n`````"), "{got}");
        let plain = text(&message("Explain.", &serde_json::json!({})));
        assert!(plain.ends_with("```json\n{}\n```"), "{plain}");
    }

    #[test]
    fn a_store_error_is_an_internal_error() {
        let (ops, _, _) = ops("internal");
        let work = |_: &Folder| -> Result<Value, ToolError> {
            Err(zoe_loupe::Error::from(zoe_core::ZoeError::CorruptTxn { tid: Tid(5) }).into())
        };
        let error = block_on(ops.data(work)).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn a_bad_argument_is_an_invalid_params_error() {
        let (ops, _, _) = ops("bad");
        let bad = TidArgs {
            tid: "latest".to_owned(),
        };
        let error = block_on(ops.explain_transaction(Parameters(bad))).unwrap_err();
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "latest: not a ZODB id");
        let missing = OidArgs {
            oid: "0x09".to_owned(),
        };
        let error = block_on(ops.why_conflict(Parameters(missing))).unwrap_err();
        assert_eq!(error.message, "oid 0x09: no record");
    }
}
