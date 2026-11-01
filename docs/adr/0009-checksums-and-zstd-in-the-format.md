# 0009. Checksums and zstd in the on-disk format

Status: merged into ADR 0020 (Decision, sections 1, 3, 5 and 6).

Every frame carries a checksum that each read verifies, and zstd is a codec of the frame. Records are raw by default. [ADR 0020](0020-record-frame.md) holds the decision.
