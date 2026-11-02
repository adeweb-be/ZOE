"""The catalog fixture: a Plone site whose catalog holds one entry for each
item of a plan, and no content object.

- `plan`: the entries, the counts and the query set, a pure function of a
  `CatalogSpec` (standard library only, so the build script imports it in
  Zope);
- `fixture`: `prepare`, the build of the fixture in a FileStorage, its pack,
  `zoe convert`, and the digests of both stores.

`python -m plonebench.catalog prepare` is the command line. `just bench
catalog prepare` runs it.
"""
