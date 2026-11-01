# 0040. A sync on macOS is fsync(2), as for FileStorage

Status: merged into ADR 0006 (Decision, "A sync on macOS is fsync(2)").

On Apple systems, every sync of `OsVfs` calls `fsync(2)`, not `F_FULLFSYNC`. [ADR 0006](0006-python-and-platform-targets.md) holds the decision.
