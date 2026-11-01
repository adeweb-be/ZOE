# 0013. The tools: devenv and just, uv and buildout, Dagger for CI, and docs/ on GitHub Pages

Status: accepted.

## Context

A laptop, a CI runner and an agent session must have the same tools at the same versions. Agents follow written commands literally and fail on commands that exist on one machine only.

The project ships Python packages (`zoedb` and its `plone` extra) and tests against real Plone instances. Plone sites are built with zc.buildout and pinned `versions.cfg` files; Python packaging today is uv.

Agents and humans must reproduce a CI result locally, and CI logic in YAML cannot be run or tested outside the CI service.

Agents read files, not external documents, and a document that drifts from the code misleads them.

## Decision

### devenv and just

`devenv.nix` pins the Rust toolchain, the four CPython interpreters, Node and every CLI. A tool that is not in `devenv.nix` does not exist for the project. The `justfile` is the single entry point; `CLAUDE.md` files name recipes, never raw commands.

### uv and buildout

uv owns the project's own packages under `python/`: a workspace, a committed `uv.lock`, the wheel built with maturin. zc.buildout owns the Plone instances under `plone/`: one configuration per scenario, `versions.cfg` pinned to a Plone release, `zoedb` taken from the wheel that uv built. No mxdev. Nothing crosses that line.

### Dagger is the CI

CI is a Dagger module written in Python in `ci/`, with one function per job. Every function runs in a container built from the devenv definition. GitHub Actions workflows are a few lines that call `dagger call`; they hold no logic. `just ci` runs the same functions locally.

### docs/ in git, published to GitHub Pages

`docs/` in git is the source of truth: the technical specification, the working-methods document and the ADRs. They are maintained in git only. The documentation site is built with mdBook from `docs/` and published to the GitHub Pages site of the repository on every merge to trunk. Nothing is hosted on a private domain.

## Consequences

- `devenv.nix` and `ci/` change through cards, reviewed like code (ADR 0018). `.github/` is a red zone that only a human changes.
- A command an agent needs and cannot find becomes a recipe in the same pull request.
- Plone instances are built the way adopting sites build them.
- The Plone suites run from `bin/test`; the project's own tests run with pytest.
- A dependency change ships with its lock file in the same commit.
- A CI failure reproduces on any machine with Docker.
- CI and the developer shell use the same tools from the same definition.
- A card that changes behaviour updates the specification in the same pull request.
- The documenter keeps the documents current after every merge.

## Merged ADRs

- ADR 0007, uv for the project's packages, buildout for Plone instances: Decision, "uv and buildout".
- ADR 0008, Dagger is the CI: Decision, "Dagger is the CI".
- ADR 0012, docs/ in git is the source of truth, published to GitHub Pages: Decision, "docs/ in git, published to GitHub Pages".
