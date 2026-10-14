"""Raw cost of a sync after N KiB written, sequential or scattered 4 KiB pages."""
import os, random, statistics, sys, time
path = sys.argv[1]
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
size = 1 << 30
os.ftruncate(fd, size); os.fsync(fd)
page = os.urandom(4096)
end = 0
def run(kib, scattered, sync, reps=150):
    global end
    times = []
    for _ in range(reps):
        t = time.perf_counter()
        n = kib // 4
        if scattered:
            for _ in range(n):
                os.pwrite(fd, page, random.randrange(size // 4096) * 4096)
        else:
            os.pwrite(fd, page * n, end % (size - n * 4096)); end += n * 4096
        sync(fd)
        times.append(time.perf_counter() - t)
    times.sort()
    return statistics.median(times) * 1e3, times[int(len(times) * 0.9)] * 1e3
for name, sync in [("fdatasync", os.fdatasync), ("fsync", os.fsync)]:
    for kib, sc in [(4, False), (64, False), (288, False), (288, True), (800, False), (800, True)]:
        med, p90 = run(kib, sc, sync)
        print(f"{name:<9} {kib:>4} KiB {'scattered' if sc else 'sequential':<10} median {med:6.2f} ms  p90 {p90:6.2f} ms")
os.close(fd); os.unlink(path)
