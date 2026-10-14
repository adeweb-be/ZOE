"""Write the pickles of zodbshootout's objects to a file, for micro.rs.

Not for merge. Each record is a 4-byte little-endian length, then the pickle.

Usage: python dump_pickles.py OUT SIZE COUNT
"""

import struct
import sys

import transaction
import ZODB
from ZODB.MappingStorage import MappingStorage
from ZODB.utils import p64
from ZODB.utils import u64
from zodbshootout._pobject import PObject
from zodbshootout._pobject import pobject_base_size
from zodbshootout.speedtest import SpeedTestData


def main():
    out, size, count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    st = SpeedTestData(1, count, size)
    gen = st._random_data(max(10, size - pobject_base_size))
    db = ZODB.DB(MappingStorage())
    conn = db.open()
    objs = [PObject(next(gen)) for _ in range(count)]
    conn.root()["o"] = objs
    transaction.commit()
    before = p64(u64(db.storage.lastTransaction()) + 1)
    with open(out, "wb") as f:
        for o in objs:
            data = db.storage.loadBefore(o._p_oid, before)[0]
            f.write(struct.pack("<I", len(data)))
            f.write(data)
    db.close()


if __name__ == "__main__":
    main()
