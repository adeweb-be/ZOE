# 0017. The distribution is zoedb; every identifier uses the stem zoe

Status: accepted

## Context

The PyPI name `ZOE` belongs to an abandoned project and the crates.io name `zoe` to an active, unrelated crate.

## Decision

The stem for every identifier is `zoe`, written ZOE in prose. The PyPI distribution is `zoedb` and the import name is `zoe`; the Plone integration is the `plone` extra and imports as `zoe.plone`. Crates are `zoe-*`; no bare `zoe` crate is published. The CLI is `zoe`, the stanza `<zoe>`, the URI scheme `zoe://`, the environment variables `ZOE_*`, the image `ghcr.io/adeweb-be/zoe`.

## Consequences

- `pip install zoedb` and `import zoe` differ; the documentation says so once.
- A transfer of the PyPI name `ZOE` is requested separately.
