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

use rmcp::service::ServerInitializeError;
use rmcp::transport::IntoTransport;
use rmcp::{RoleServer, ServiceExt};
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
/// client closes stdin, also before the `initialize` request.
///
/// The server holds the lock of the store while it runs. So a running
/// instance and the server never share a store. The server writes only
/// MCP messages to stdout.
pub fn serve_stdio(path: &Path) -> Result<(), Error> {
    let folder = Folder::open(path).map_err(|source| Error::Open {
        path: path.to_owned(),
        source,
    })?;
    // One thread runs the session. The tools run on the blocking pool.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Session(e.to_string()))?;
    let result = runtime.block_on(serve(folder, rmcp::transport::stdio()));
    // The read of stdin cannot stop. Do not wait for it.
    runtime.shutdown_background();
    result
}

/// Serve the read-only ops tools of `folder` on `transport`. Return when
/// the client closes the transport, also before the `initialize` request.
async fn serve<T, E, A>(folder: Folder, transport: T) -> Result<(), Error>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let session = |error: &dyn std::fmt::Display| Error::Session(error.to_string());
    let service = match ops::Ops::new(folder).serve(transport).await {
        Ok(service) => service,
        // The client closed the transport before `initialize`. Nothing
        // failed.
        Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
        Err(e) => return Err(session(&e)),
    };
    service.waiting().await.map_err(|e| session(&e))?;
    Ok(())
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

    /// A store with an empty root, in a new folder.
    fn folder(name: &str) -> Folder {
        let dir = std::env::temp_dir().join(format!("zoe-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = zoe_core::Store::open(&dir).unwrap();
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let root = b"\x80\x03cm\nC\n.\x80\x03}.";
        txn.store(zoe_core::Oid(0), zoe_core::Tid(0), root).unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);
        Folder::open(&dir).unwrap()
    }

    /// Serve `folder` to a client that writes `input` and then closes its
    /// end. Return the result of the session and what the server wrote.
    fn session(folder: Folder, input: &'static [u8]) -> (Result<(), Error>, String) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (client, server) = tokio::io::duplex(1 << 16);
                let session = tokio::spawn(serve(folder, server));
                let (mut read, mut write) = tokio::io::split(client);
                write.write_all(input).await.unwrap();
                write.shutdown().await.unwrap();
                let mut output = String::new();
                read.read_to_string(&mut output).await.unwrap();
                (session.await.unwrap(), output)
            })
    }

    #[test]
    fn a_client_that_leaves_before_initialize_ends_the_session_cleanly() {
        let (result, output) = session(folder("eof"), b"");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "");
        // rmcp skips a line that is not JSON.
        let (result, output) = session(folder("not-json"), b"not json\n");
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "");
    }

    #[test]
    fn a_request_before_initialize_gets_a_reply_and_the_session_waits() {
        let line = b"{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": \"tools/list\"}\n";
        let (result, output) = session(folder("request-first"), line);
        assert!(result.is_ok(), "{result:?}");
        let reply: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(reply["id"], 1, "{output}");
    }

    #[test]
    fn a_notification_before_initialize_fails_the_session() {
        let line = b"{\"jsonrpc\": \"2.0\", \"method\": \"notifications/initialized\"}\n";
        let (result, _) = session(folder("no-initialize"), line);
        assert!(matches!(result, Err(Error::Session(_))), "{result:?}");
    }
}
