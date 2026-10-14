"""Write the pickles of 2,000 to 3,200 bytes of a FileStorage to a file.

codecx reads the file: each record is a 4-byte little-endian length, then
the pickle. Run it with the Python of `just bench` on the Data.fs of a zeo
run of driver.py.

    python extract_samples.py work/zeo-threads-c16-s1024/Data.fs samples.bin
"""

import struct
import sys

from ZODB.FileStorage import FileStorage

source, target = sys.argv[1:3]
storage = FileStorage(source, read_only=True)
count = 0
total = 0
with open(target, "wb") as out:
    for txn in storage.iterator():
        for record in txn:
            if record.data and 2000 <= len(record.data) <= 3200:
                out.write(struct.pack("<I", len(record.data)))
                out.write(record.data)
                count += 1
                total += len(record.data)
        if count >= 3000:
            break
print(f"{count} samples, mean {total / count:.0f} bytes")
