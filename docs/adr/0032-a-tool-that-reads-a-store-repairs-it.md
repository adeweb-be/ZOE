# 0032. A tool that reads a store repairs it when a writer left it open

Status: merged into ADR 0028 (Decision, "A tool that reads a store repairs it").

A tool that reads a store opens it with `Store::open_read_only_repairing`: it repairs `zoe.redb` when a writer left it open, says `repaired the store` on stderr, and changes no byte of a clean store. [ADR 0028](0028-the-zoe-command.md) holds the decision.
