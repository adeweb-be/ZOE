# 0007. uv for the project's packages, buildout for Plone instances

Status: accepted

## Context

The project ships Python packages (`zoedb` and its `plone` extra) and tests against real Plone instances. Plone sites are built with zc.buildout and pinned `versions.cfg` files; Python packaging today is uv.

## Decision

uv owns the project's own packages under `python/`: a workspace, a committed `uv.lock`, the wheel built with maturin. zc.buildout owns the Plone instances under `plone/`: one configuration per scenario, `versions.cfg` pinned to a Plone release, `zoedb` taken from the wheel that uv built. No mxdev. Nothing crosses that line.

## Consequences

- Plone instances are built the way adopting sites build them.
- The Plone suites run from `bin/test`; the project's own tests run with pytest.
- A dependency change ships with its lock file in the same commit.
