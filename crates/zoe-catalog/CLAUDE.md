# zoe-catalog

The catalog mirror: the ZCatalog indexes in memory, versioned per tid. Specification: "Catalog acceleration and search".

## Invariants

- A query is answered as of the snapshot tid of the client. A result never names an object that the Connection cannot load.
- A query with an index or an operator that the mirror does not cover takes the Python path, whole.
- A snapshot older than the mirror window takes the Python path.
- Rids, order and counts equal those of ZCatalog. Ties break by rid, as in ZCatalog.
- Security stays in Plone: `allowedRolesAndUsers` is an index like any other.
