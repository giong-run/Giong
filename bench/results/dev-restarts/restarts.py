"""100 restarts of giong dev under constant load: failed requests and save-to-serve latency.

usage: restarts.py SERVICE_DIR PORT RESTARTS CONCURRENCY

Run `giong dev --listen 127.0.0.1:PORT` in SERVICE_DIR first (see README.md).
"""
import collections, json, os, re, socket, statistics, sys, threading, time

service, port, restarts, conc = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
addr = ("127.0.0.1", port)
src = os.path.join(service, "src", "main.rs")
state = os.path.join(service, ".giong", "dev.json")
stats = collections.Counter(); errs = collections.Counter(); lock = threading.Lock()
stop = threading.Event()


def get(timeout=10):
    s = socket.create_connection(addr, timeout=timeout)
    s.sendall(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
    data = b""
    while True:
        d = s.recv(4096)
        if not d:
            break
        data += d
    s.close()
    return data.split(b"\r\n\r\n", 1)[-1].decode() if data else "EMPTY"


def worker():
    while not stop.is_set():
        try:
            body = get()
            with lock:
                stats["ok" if body.startswith("v") else body] += 1
        except Exception as e:
            with lock:
                errs[type(e).__name__ + ":" + str(e)[:40]] += 1
            time.sleep(0.01)


ts = [threading.Thread(target=worker, daemon=True) for _ in range(conc)]
[t.start() for t in ts]
latencies = []
for i in range(1, restarts + 1):
    text = open(src).read()
    text = re.sub(r'const VERSION: &str = "v\d+";', f'const VERSION: &str = "v{i}";', text)
    t0 = time.time()
    with open(src, "w") as f:
        f.write(text)
    while True:
        try:
            if get(timeout=5) == f"v{i}":
                break
        except Exception:
            pass
        if time.time() - t0 > 120:
            print("timeout waiting for", i)
            sys.exit(1)
        time.sleep(0.005)
    latencies.append(time.time() - t0)
    # Let the old build retire before the next save.
    time.sleep(0.6)
    if i % 10 == 0:
        with lock:
            print(f"{i} restarts, {sum(stats.values())} requests, errors {dict(errs)}", flush=True)
stop.set()
[t.join() for t in ts]
d = json.load(open(state))
lat = sorted(latencies)
print(json.dumps({
    "restarts": restarts, "dev_restarts": d.get("restarts"),
    "requests": sum(stats.values()), "results": dict(stats), "errors": dict(errs),
    "save_to_serve_s": {"median": round(statistics.median(lat), 3), "p90": round(lat[int(len(lat) * .9) - 1], 3),
                        "min": round(lat[0], 3), "max": round(lat[-1], 3)},
}))
