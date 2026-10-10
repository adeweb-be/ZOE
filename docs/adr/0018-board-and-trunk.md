# 0018. GitHub Issues and Projects are the board; trunk-based development

Status: accepted

## Context

The human and the agent sessions need one place that holds the state of every card. Agents work in parallel and must not step on each other's changes.

## Decision

Cards are GitHub issues on a Projects board with the columns Backlog, Ready, Building, Review, Verifying, Human and Done. The board holds the state of every card. Work is trunk-based: one short-lived branch `card/<id>` per card, one agent per worktree, squash merges, no force pushes.

## Consequences

- Every merged line traces back to one card and one pull request.
