# 0018. GitHub Issues and Projects are the board; trunk-based development

Status: accepted

## Context

The loops run without anyone watching and need one place to read state from. Agents work in parallel and must not step on each other's changes.

## Decision

Cards are GitHub issues on a Projects board with the columns Backlog, Ready, Building, Review, Verifying, Human and Done. The board is the only state the loops read. Work is trunk-based: one short-lived branch `card/<id>` per card, one agent per worktree, squash merges, no force pushes.

## Consequences

- A loop that cannot find its state on the board does nothing.
- Every merged line traces back to one card and one pull request.
