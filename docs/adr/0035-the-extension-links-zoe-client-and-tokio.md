# 0035. The extension links zoe-client and tokio for the native client

Status: merged into ADR 0001 (Decision, "The extension links zoe-client and tokio, never rmcp").

The extension `zoe._zoe` links `zoe-client` and tokio for the native client, starts its threads at the first open, and never links rmcp. [ADR 0001](0001-synchronous-rust-core.md) holds the decision.
