import os, sys
total = 0
for root, _, files in os.walk(sys.argv[1]):
    for name in files:
        path = os.path.join(root, name)
        fd = os.open(path, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            total += os.fstat(fd).st_size
        finally:
            os.close(fd)
print(f"dropped {total / 1e9:.1f} GB from the page cache")
