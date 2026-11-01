# 0050. The registry backend: a moved registry keeps its records in a durable namespace of its own

Status: merged into ADR 0043 (Decision, part B "The registry backend").

`zoe.plone.registry` moves a registry of plone.registry into the durable namespace `zoe.plone.registry:<id>`, with a guard, a write counter and a check that never reads a copy as empty. Section N of this ADR is section BN of ADR 0043. [ADR 0043](0043-kv-namespaces.md) holds the decision.
