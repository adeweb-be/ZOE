"""The configuration of pytest for the tests of the workspace.

The tests of `zoe.plone` that need a Plone site live in the package, for
`bin/test` of a Plone instance. pytest runs without Plone, so it collects
nothing in the sources.
"""

collect_ignore = ["zoedb/src"]
