//! The MCP server for ops and content agents. Specification: "AI-native:
//! MCP server and retrieval".
//!
//! [`serve_stdio`] opens a store folder for reads only and speaks MCP on
//! stdin and stdout, with the `rmcp` SDK. It offers the read-only ops
//! tools `object_get`, `history`, `diff` and `references`. They are the
//! functions of Loupe with an agent face: each tool calls `zoe-loupe`, and
//! its result is the JSON of `zoe-loupe`.
//!
//! The server runs as the ops principal of the local operator: the user
//! who starts the process. That principal has the read-only role. So the
//! server offers no tool that writes, and it does not offer `pack` or
//! `backup`.
//!
//! An error of a tool is a tool result with `isError`. The session goes
//! on.

mod ops;

use std::path::{Path, PathBuf};

use rmcp::ServiceExt;
use zoe_loupe::Folder;

/// An error of the server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The store does not open. For example, the folder holds no store, or
    /// another process holds the lock of the store.
    #[error("{}", open_failure(.path, .source))]
    Open {
        /// The folder of the store.
        path: PathBuf,
        /// The error of Loupe.
        source: zoe_loupe::Error,
    },

    /// The MCP session fails. For example, the client closes stdin before
    /// the `initialize` request.
    #[error("the MCP session failed: {0}")]
    Session(String),
}

/// Return the message of a store that does not open.
fn open_failure(path: &Path, error: &zoe_loupe::Error) -> String {
    match error {
        // This message already names the folder.
        zoe_loupe::Error::NoStore { .. } => error.to_string(),
        _ => format!("{}: {error}", path.display()),
    }
}

/// Open the store in the folder `path` for reads only, and serve the
/// read-only ops tools over MCP on stdin and stdout. Return when the
/// client closes stdin.
///
/// The server holds the lock of the store while it runs. So a running
/// instance and the server never share a store. The server writes only
/// MCP messages to stdout.
pub fn serve_stdio(path: &Path) -> Result<(), Error> {
    let folder = Folder::open(path).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    let session = |error: &dyn std::fmt::Display| Error::Session(error.to_string());
    // One thread runs the session. The tools run on the blocking pool.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| session(&e))?;
    let result = runtime.block_on(async {
        let service = ops::Ops::new(folder)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| session(&e))?;
        service.waiting().await.map_err(|e| session(&e))?;
        Ok(())
    });
    // The read of stdin cannot stop. Do not wait for it.
    runtime.shutdown_background();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_without_a_store_does_not_open() {
        let missing = std::env::temp_dir().join(format!("zoe-mcp-none-{}", std::process::id()));
        let error = serve_stdio(&missing).err();
        assert!(
            matches!(
                error,
                Some(Error::Open {
                    source: zoe_loupe::Error::NoStore { .. },
                    ..
                })
            ),
            "{error:?}"
        );
        // The message names the folder once.
        let message = error.unwrap().to_string();
        let folder = missing.display().to_string();
        assert_eq!(message.matches(&folder).count(), 1, "{message}");
    }
}
