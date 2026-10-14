# `just test` does not collect the conformance suites and the kill loop.
# `just conformance` and `just crash` name their directories, so pytest
# collects them there.
collect_ignore = ["conformance", "crash"]
