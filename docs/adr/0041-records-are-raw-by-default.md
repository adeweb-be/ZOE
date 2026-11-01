# 0041. Records are raw by default; zstd is a setting

Status: merged into ADR 0020 (Decision, section 6).

The default compression is `none`; the setting `compression` takes `none`, `zstd` or `zstd:LEVEL`. [ADR 0020](0020-record-frame.md) holds the decision.
