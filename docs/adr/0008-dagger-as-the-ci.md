# 0008. Dagger is the CI

Status: accepted

## Context

Agents and humans must reproduce a CI result locally, and CI logic in YAML cannot be run or tested outside the CI service.

## Decision

CI is a Dagger module written in Python in `ci/`, with one function per job. Every function runs in a container built from the devenv definition. GitHub Actions workflows are a few lines that call `dagger call`; they hold no logic. `just ci` runs the same functions locally.

## Consequences

- A CI failure reproduces on any machine with Docker.
- CI and the developer shell use the same tools from the same definition.
- `ci/` and `.github/` are red zones that only a human changes.
