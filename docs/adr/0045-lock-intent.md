# 0045. Early write-intent locks: the lock before the read of a declared hot object

Status: merged into ADR 0044 (Decision, part B "Early write-intent locks").

With `early-locks on`, an object of a class of `early-lock-classes` takes its intent lock (`lock_intent`) when its state loads. Section N of this ADR is section BN of ADR 0044. [ADR 0044](0044-the-lock-hand-off.md) holds the decision.
