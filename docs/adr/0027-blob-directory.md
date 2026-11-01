# 0027. The blob directory

Status: merged into ADR 0023 (Decision, "The blob directory is an option of the core").

`Options::blob_dir` gives the blob directory, `blobs/` in the store directory by default; the store does not record it, and a writer locks it. [ADR 0023](0023-blob-record.md) holds the decision.
