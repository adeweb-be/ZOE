# 0015. One on-disk format for both shapes

Status: accepted

## Context

A site often starts with one Zope instance and later needs a cluster. A conversion step at that moment is a risk and a maintenance window.

## Decision

The in-process storage and the server open the same directory with the same format. A lock file enforces one writer. Moving from one instance to a cluster is: stop the instance, start the server on the directory, point the instances at it.

## Consequences

- Switching shape is a restart, in both directions.
- Format changes are tested against both skins.
