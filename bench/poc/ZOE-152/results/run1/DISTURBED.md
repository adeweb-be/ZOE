# Run 1 is disturbed

It ran under the exclusive lock of the machine. During it, another session
that did not take the lock ran `cargo test --workspace` (the zoe-server tests)
for about 20 minutes. The rounds of this run were taken on a loaded machine.
The finding does not use them; run 2 repeats them on a quiet machine.

Two faults of `hit.py` also show here, fixed for run 2: the 16-thread wall
time could be negative (a thread finished before the main thread took its
start time), and the 100 KB cells used a 20 MB cache for 1,000 objects of
100 KB, so most reads missed (see `misses`). Their numbers are not hits.
