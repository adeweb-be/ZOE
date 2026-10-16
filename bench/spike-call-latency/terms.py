"""Split each loadBefore of a trace into its terms.

Usage: python terms.py PREFIX STORAGE

PREFIX is the ZOE_SPIKE_TRACE prefix of a run at concurrency 1. The files
PREFIX.* hold "probe nanoseconds" lines of all the processes of the run,
on one clock. A load runs from probe 17 to probe 18 in the Python process.
"""

import glob
import statistics
import sys

prefix, storage = sys.argv[1], sys.argv[2]
events = []
for path in [p for p in glob.glob(prefix + ".*") if not p.endswith(".json")]:
    with open(path) as f:
        for line in f:
            probe, t = line.split()
            events.append((int(t), int(probe)))
events.sort()

if storage == "zoe-native":
    steps = [
        (17, 16, "Python and ZODB to the extension"),
        (16, 10, "extension: pool, block_on, to the call"),
        (10, 11, "client: encode, queue the frame"),
        (11, 12, "client: writer task wakes, writes"),
        (12, 21, "socket to the server, reader wakes, reads"),
        (21, 22, "server: decode, ticket, read-pool thread"),
        (22, 23, "server: the load in the store"),
        (23, 24, "server: outbox, writer task wakes, writes"),
        (24, 13, "socket to the client, reader wakes, reads"),
        (13, 14, "client: decode, dispatch the reply"),
        (14, 15, "client: the caller wakes"),
        (15, 18, "extension: bytes to Python"),
    ]
else:
    steps = [
        (17, 11, "ClientStorage to the IO thread"),
        (11, 12, "IO thread: encode, write"),
        (12, 21, "socket to the server, loop wakes, reads"),
        (21, 22, "server: decode, dispatch"),
        (22, 23, "server: the load in FileStorage"),
        (23, 24, "server: encode, write"),
        (24, 13, "socket to the client, loop wakes, reads"),
        (13, 18, "IO thread: decode, set the result; the caller wakes"),
    ]
# Each probe is the first one after the probe that it names here.
anchor = {b: a for a, b, _ in steps}
if storage == "zoe-native":
    # The writer task can write before the caller marks the queued frame.
    anchor[12] = 10
    anchor[21] = 10

loads = []
starts = [k for k, (t, probe) in enumerate(events) if probe == 17]
for n, k in enumerate(starts):
    end = starts[n + 1] if n + 1 < len(starts) else len(events)
    window = events[k:end]
    times = {17: window[0][0]}
    ok = True
    for _, probe, _ in steps:
        after = times.get(anchor[probe])
        if after is None:
            ok = False
            break
        found = next((t for t, p in window if p == probe and t >= after), None)
        if found is None:
            ok = False
            break
        times[probe] = found
    if ok:
        loads.append(times)

print(f"{storage}: {len(loads)} loads with every probe")
total = [(ts[18] - ts[17]) / 1e3 for ts in loads]
for a, b, name in steps:
    values = [(ts[b] - ts[a]) / 1e3 for ts in loads]
    print(f"| {name} | {statistics.median(values):.1f} |")
print(f"| total of one loadBefore | {statistics.median(total):.1f} |")
