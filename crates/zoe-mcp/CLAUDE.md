# zoe-mcp

The MCP server for ops and content agents. Specification: "AI-native: MCP server and retrieval".

## Invariants

- The server runs as a principal. Every content result goes through the roles bitmap before ranking.
- Objects outside the catalog are visible to the ops role only.
- Ops tools are read-only. `pack` and `backup` need the read-write role.
- An anonymous principal never receives a restricted passage.
