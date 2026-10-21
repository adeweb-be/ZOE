# 0049. The split merge keeps the bucket of the committed side: rule 3 writes into it, and a declared client invalidates it

Status: merged into ADR 0048 (Decision, part B "Rule 3 keeps the bucket of the committed side").

With a client that declares `server_writes`, rule 3 of the split merge writes the merged keys into the buckets of the committed side, under a lock that never waits; without it, rule 3 gives no answer when both sides split one bucket. Section N of this ADR is section BN of ADR 0048; its section 12 is in sections B2, B3, B4, B9 and B10. [ADR 0048](0048-the-split-merge.md) holds the decision.
