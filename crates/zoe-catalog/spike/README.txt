The scripts of the spike on the catalog mirror. Not for merge.

- probe_sitecustomize.py: the recording patch of Catalog.search. Put it on
  PYTHONPATH as sitecustomize.py and set ZOE283_SEARCH_LOG to a file: each
  call of Catalog.search writes one JSON line (indexes, options, sort,
  batch, and the catalog records that the transaction changed).
  - suites: PYTHONPATH=DIR ZOE283_SEARCH_LOG=FILE just plone zoe suites
  - B5: copy it into bench/plone/src/plonebench/probe/sitecustomize.py and
    run `just bench plone run --stack zoe-native --scenario S --fixture F`
    with ZOE283_SEARCH_LOG set.
- inventory.py LABEL FILE... [-- LABEL FILE...]: the tables of question 6.
- testbed.py OUT_DIR [SEED] [N]: a catalog of N entries in a
  MappingStorage, with ZCatalog's answers to random queries, overlays and a
  commit; run with the interpreter of the zoe Plone scenario, then
  `mirror test OUT_DIR 4` (example of zoe-catalog).
- check.py STORE PLANS OVERLAYS MANIFEST: the plans and ZCatalog's answers
  on catalog-1m, and two transactions that change its catalog.
- drop.py DIR: drops the pages of the files of DIR from the page cache.
- report.py JSON...: the tables of the finding from `mirror bench`.

    mirror bench STORE CATALOG_OID OUT.json [--threads N] [--rebuild]
        [--plans PLANS] [--overlays OVERLAYS] [--rounds N] [--commits N]
