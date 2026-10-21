# 0029. The frame checksum covers the key space

Status: merged into ADR 0020 (Decision, sections 3 and 4).

The checksum input of each frame starts with a one-byte tag of its key space, which the reader gives. [ADR 0020](0020-record-frame.md) holds the decision and the table of tags.
