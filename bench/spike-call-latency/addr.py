"""Log each set_server_addr and each canonical lookup of a fresh native client."""
import os, socket, sys, tempfile, threading, time
from pathlib import Path
import transaction, ZODB.config
from shootout.storages import STORAGES, zconfig
import zoe.zeo as zz

pc = time.perf_counter
T0 = [0.0]
log = []
orig_set = zz._NativeClientStorage.set_server_addr
def set_server_addr(self, addr):
    log.append(("set_server_addr", round((pc()-T0[0])*1e3, 2), threading.current_thread().name))
    return orig_set(self, addr)
zz._NativeClientStorage.set_server_addr = set_server_addr
orig_gh = socket.gethostbyaddr
def gethostbyaddr(h):
    b = pc()
    try:
        return orig_gh(h)
    finally:
        log.append(("gethostbyaddr", round((b-T0[0])*1e3, 2), round((pc()-b)*1e3, 2), threading.current_thread().name))
socket.gethostbyaddr = gethostbyaddr

with tempfile.TemporaryDirectory(dir=os.environ.get("SPIKE_DIR")) as d:
    with zconfig(STORAGES["zoe-native"], Path(d), None) as text:
        for i in range(3):
            T0[0] = pc()
            db = ZODB.config.databaseFromString(text)
            log.append(("db created", round((pc()-T0[0])*1e3, 2)))
            conn = db.open()
            conn.root()["x"] = i
            transaction.commit()
            log.append(("committed", round((pc()-T0[0])*1e3, 2)))
            time.sleep(0.2)
            conn.close(); db.close()
            log.append(("---",))
for e in log:
    print(e)
