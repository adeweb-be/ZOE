# 0013. devenv is the only environment, just is the only entry point

Status: accepted

## Context

A laptop, a CI runner and an agent session must have the same tools at the same versions. Agents follow written commands literally and fail on commands that exist on one machine only.

## Decision

`devenv.nix` pins the Rust toolchain, the four CPython interpreters, Node and every CLI. A tool that is not in `devenv.nix` does not exist for the project. The `justfile` is the single entry point; `CLAUDE.md` files name recipes, never raw commands.

## Consequences

- `devenv.nix` is a red zone.
- A command an agent needs and cannot find becomes a recipe in the same pull request.
