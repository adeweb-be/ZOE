"""Print the main numbers of the JSON lines of driver.py on stdin."""

import json
import sys

for line in sys.stdin:
    if not line.startswith("{"):
        continue
    r = json.loads(line)
    for k in [k for k in r if isinstance(r[k], dict) and "ops" in r[k]]:
        if True:
            print(f"{r['label']:22} {r['mode']:9} {k:9} {r[k]['ops']:>7} ops/s  client cores {r.get('client_cores')}  "
                  f"client cpu s {r.get('client_cpu_s')}  server cpu s {r.get('server_cpu_s')}", flush=True)
