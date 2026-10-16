"""The time of one durable write of 4 KiB on the volume of DIR: write, then sync."""
import fcntl, os, statistics, sys, time
d = sys.argv[1]; mode = sys.argv[2]
fd = os.open(os.path.join(d, "fsync.test"), os.O_RDWR | os.O_CREAT, 0o644)
buf = os.urandom(4096); times = []
for i in range(300):
    os.pwrite(fd, buf, (i % 64) * 4096)
    b = time.perf_counter_ns()
    if mode == "full":
        fcntl.fcntl(fd, fcntl.F_FULLFSYNC)
    else:
        os.fsync(fd)
    times.append(time.perf_counter_ns() - b)
os.close(fd); os.unlink(os.path.join(d, "fsync.test"))
t = sorted(times[20:])
print(f"{mode}: median {statistics.median(t)/1e3:.0f} us, p10 {t[len(t)//10]/1e3:.0f} us, p90 {t[9*len(t)//10]/1e3:.0f} us")
