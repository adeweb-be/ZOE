"""The round trip of loopback TCP between two processes, blocking sockets.

The client sends a request of 64 bytes, the server answers SIZE bytes.
Each side reads on the thread that waits: no handoff.
"""
import os, socket, statistics, sys, time

size = int(sys.argv[1]) if len(sys.argv) > 1 else 1100
n = 20000
srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
port = srv.getsockname()[1]
if os.fork() == 0:
    c, _ = srv.accept(); c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    reply = b"x" * size
    while True:
        req = c.recv(64)
        if not req:
            os._exit(0)
        c.sendall(reply)
s = socket.create_connection(("127.0.0.1", port)); s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
req = b"r" * 64
times = []
for i in range(n):
    b = time.perf_counter_ns()
    s.sendall(req)
    got = 0
    while got < size:
        got += len(s.recv(65536))
    times.append(time.perf_counter_ns() - b)
s.close(); os.wait()
times = times[2000:]
print(f"reply {size} B: median {statistics.median(times)/1e3:.1f} us, p10 {sorted(times)[len(times)//10]/1e3:.1f} us")
