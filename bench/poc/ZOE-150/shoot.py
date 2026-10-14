"""zodbshootout in one process, with fewer transactions. Not for merge.

Run it with the arguments of zodbshootout and `--worker`: pyperf then runs
every benchmark in this process and starts no worker process.
ZOE150_INNER_LOOPS sets the transactions of each worker per benchmark;
zodbshootout sets 100, or 30 above a concurrency of 10.
"""

import os
import sys

import zodbshootout._runner as runner

_create = runner._create_speedtest


def _create_speedtest(options, data):
    speedtest = _create(options, data)
    loops = os.environ.get("ZOE150_INNER_LOOPS")
    if loops:
        speedtest.inner_loops = int(loops)
    return speedtest


runner._create_speedtest = _create_speedtest

if __name__ == "__main__":
    # A child process of the processes mode imports this module again as
    # __mp_main__. It must not start a run of its own.
    from zodbshootout.main import main

    sys.argv[0] = "zodbshootout"
    main()
