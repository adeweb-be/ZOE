# zoe-client

A Rust client for the ZEO protocol, with the ZOE extensions. `zoe.zeo.ClientStorage` and the server mode of Loupe use it. Specification: "Async and free-threading" and F28.

## Invariants

- The client speaks the same ZEO 6 msgpack protocol as `ZEO.ClientStorage`.
- The client uses an extension only after the server accepted it in `get_info`.
- `zoe.zeo.ClientStorage` is a drop-in for `ZEO.ClientStorage`, never a requirement.
- A change to the wire protocol is a stop-and-ask.
