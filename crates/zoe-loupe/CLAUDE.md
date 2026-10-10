# zoe-loupe

The Rust side of Loupe, the desktop inspector and editor. Specification: "Loupe: desktop inspector and editor".

## Invariants

- Loupe never opens a `Data.fs`. It converts one.
- Every Loupe write is one ZODB transaction: user `loupe:<operating-system user>`, a description that the operator types, and extension data with the tool name and version.
- Writes need expert mode for the session.
- Opaque pickle nodes are not editable.
- A delete is refused while inbound references exist, unless the operator chooses cascade.
- Every commit shows the pickle diff first.
- Credentials live in the operating-system keychain, never in a configuration file.
