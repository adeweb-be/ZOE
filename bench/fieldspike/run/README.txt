The scripts of the spike ZOE-281 (field-addressable objects). Not for merge.

They expect this checkout at $HOME/.cache/zoe-build/ZOE-281/src, the zoe
binary and the example in $HOME/.cache/zoe-build/ZOE-281/target, the zoeclient
Plone instance (just plone zoeclient build) and the small fixture.

  b5.sh SCENARIO...      the B5 runs, with zoe-with-log as ZOE_BINARY
  analyze-run.sh SCENARIO  dump.py, fieldspike fields, analyze.py, votes.py,
                           analyze_votes.py on the kept store of a run
  micro.sh               the micro-benchmarks (fieldspike bench and decode,
                           pybench.py on 3.13 and 3.14t; PYTHON_VENVS names
                           the directory of the venvs)
  wtable.py SCENARIO...  the table of the write volume

inventory.py and sample.py run in Zope with
bench/plone/src/plonebench/inzope/run.py and a zope.conf whose <zoe> stanza
opens a copy of a kept store read-only. indexer_records.py writes the
records of the batches of the indexer for fieldspike decode.
