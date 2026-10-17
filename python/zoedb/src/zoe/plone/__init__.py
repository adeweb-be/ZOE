"""The Plone integration. Only the `plone` extra imports Plone.

Plone loads `configure.zcml` of this package through the plone.autoinclude
entry point of zoedb. `uidgenerator` gives `portal_uidgenerator` its ids
from a sequence of the store. `report` marks the transaction of each
request with its method, for the writes-on-GET report of `zoe serve`.
"""
