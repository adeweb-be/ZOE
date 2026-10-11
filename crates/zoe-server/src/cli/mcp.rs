//! `zoe mcp`: serve the read-only ops tools of a store to an agent, over
//! MCP on stdin and stdout.
//!
//! The command opens the store for reads only, with the lock of the store.
//! It runs as the ops principal of the local operator, and offers no tool
//! that writes. stdout carries only MCP messages, so the command writes no
//! report. It ends with 0 when the client closes stdin. It compiles only
//! with the feature `serve`, because `zoe-mcp` uses tokio.

use std::path::PathBuf;

use clap::Args;

use super::Failure;

/// The options of `zoe mcp`.
#[derive(Debug, Args)]
pub(super) struct McpArgs {
    /// The directory of the store. The command opens it for reads only.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,
}

/// Run `zoe mcp` until the client closes stdin.
pub(super) fn run(args: &McpArgs) -> Result<(), Failure> {
    zoe_mcp::serve_stdio(&args.path).map_err(|error| Failure::Message(error.to_string()))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use zoe_core::SystemClock;

    use super::super::{Cli, FAILURE, run};

    #[test]
    fn mcp_needs_a_path() {
        assert!(Cli::try_parse_from(["zoe", "mcp"]).is_err());
        assert!(Cli::try_parse_from(["zoe", "mcp", "--path", "store"]).is_ok());
    }

    #[test]
    fn a_folder_without_a_store_fails_and_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("zoe-cli-mcp-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cli = Cli::try_parse_from(["zoe", "mcp", "--path", dir.to_str().unwrap()]).unwrap();
        let mut out = Vec::new();
        assert_eq!(run(&cli, &SystemClock, &mut out), FAILURE);
        assert!(out.is_empty());
        // The open creates nothing.
        assert!(!dir.exists());
    }
}
