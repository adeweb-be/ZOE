# 0016. Format room is reserved per record from the first store

Status: merged into ADR 0020 (Decision, section 2).

Every record carries a codec byte, a branch id and typed-field slots, and every blob record its BLAKE3 hash, from the first store written. [ADR 0020](0020-record-frame.md) holds the decision.
