# 0012. docs/ in git is the source of truth, published to GitHub Pages

Status: accepted

## Context

The specification and the working methods were written outside the repository. Agents read files, not external documents, and a document that drifts from the code misleads them.

## Decision

`docs/` in git is the source of truth: the technical specification, the working-methods document and the ADRs. They were exported once and are maintained in git. The documentation site is built with mdBook from `docs/` and published to the GitHub Pages site of the repository on every merge to trunk. Nothing is hosted on a private domain.

## Consequences

- A card that changes behaviour updates the specification in the same pull request.
- The documenter loop keeps the documents current after every merge.
