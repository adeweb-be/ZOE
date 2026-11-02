# The text index spike (ZOE-284)

The prototype of the native text index and the scripts of its measures.
Not for merge. The finding is `docs/spikes/text-index.md` on `main`.

- `corpus.py PRESET OUT`: the texts of a catalog fixture plan, as JSON lines.
- `zctext.py VENDOR CORPUS DOCS CASES`: ZCTextIndex (OkapiIndex and the
  lexicon of `plone_lexicon`) on a corpus and a synthetic query set; the
  expected results. VENDOR holds `UnicodeSplitter/` of Products.CMFPlone
  6.1.5 and `plone_i18n_base.py`, `plone/i18n/normalizer/base.py` of
  plone.i18n 5.1.1.
- `queries.py VENDOR PLAN CORPUS OUT`: the query set of `catalog-1m`.
- `zcsave.py VENDOR DATA_FS CORPUS SAVES`: the records of one save of
  ZCTextIndex on the `Data.fs` of `catalog-1m`.
- `examples/textspike.rs`: `build`, `bench`, `compat`, `deltas`,
  `window`, `threads`, `words`, `overlaycost`.
- `ZOE_UNIDECODE_TABLE` names a JSON list of the transliteration of each
  character of the basic plane by Python's Unidecode.
