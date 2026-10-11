# `just test` does not collect the conformance suites. `just conformance`
# names their directory, so pytest collects them there.
collect_ignore = ["conformance"]
