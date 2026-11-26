"""giong dev restarts with streamed responses in flight.

usage: streams.py SERVICE_DIR PORT RESTARTS STREAMS STREAM_SECS [LOAD_THREADS]

Run `giong dev --listen 127.0.0.1:PORT` in SERVICE_DIR first (see README.md).
For each restart: open STREAMS streamed responses of STREAM_SECS seconds,
wait 0.5 s so they are halfway through, save a new VERSION, wait until a
request is answered by the new build, then wait for every stream to end.
A stream passes when it ends with the chunked terminator, its last line is
`done`, it has every line, and every line came from the old build (one
version, one pid). LOAD_THREADS clients loop short requests the whole time.
"""
import collections, json, os, re, socket, statistics, sys, threading, time

service, port, restarts = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
nstreams, secs = int(sys.argv[4]), int(sys.argv[5])
load = int(sys.argv[6]) if len(sys.argv) > 6 else 16
addr = ("127.0.0.1", port)
src = os.path.join(service, "src", "main.rs")
stats = collections.Counter(); errs = collections.Counter(); lock = threading.Lock()
stop = threading.Event()


def request(path, timeout):
    s = socket.create_connection(addr, timeout=timeout)
    s.sendall(f"GET {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n".encode())
    data = b""
    while True:
        d = s.recv(65536)
        if not d:
            break
        data += d
    s.close()
    return data


def dechunk(raw):
    """Body of a chunked response, and whether its terminating chunk arrived."""
    head, _, rest = raw.partition(b"\r\n\r\n")
    if b"transfer-encoding: chunked" not in head.lower():
        return rest, False
    body = b""
    while True:
        line, sep, rest = rest.partition(b"\r\n")
        if not sep:
            return body, False
        n = int(line.split(b";")[0] or b"x", 16)
        if n == 0:
            return body, True
        if len(rest) < n + 2:
            return body + rest[:n], False
        body += rest[:n]
        rest = rest[n + 2:]


def get_version(timeout=5):
    raw = request("/", timeout)
    return raw.partition(b"\r\n\r\n")[2].decode() if raw else "EMPTY"


def loader():
    while not stop.is_set():
        try:
            body = get_version(10)
            with lock:
                stats["ok" if body.startswith("v") else "bad:" + body[:20]] += 1
        except Exception as e:
            with lock:
                errs[type(e).__name__ + ":" + str(e)[:40]] += 1
            time.sleep(0.01)


def streamer(out, idx):
    t0 = time.time()
    try:
        raw = request(f"/stream?secs={secs}", secs + 40)
        body, terminated = dechunk(raw)
        lines = body.decode().splitlines()
        out[idx] = {"terminated": terminated, "lines": lines, "secs": time.time() - t0}
    except Exception as e:
        out[idx] = {"error": type(e).__name__ + ":" + str(e)[:60], "lines": [], "secs": time.time() - t0}


ts = [threading.Thread(target=loader, daemon=True) for _ in range(load)]
[t.start() for t in ts]
want = secs * 10 + 1
streams = collections.Counter(); bad = []; latencies = []; stream_secs = []
for i in range(1, restarts + 1):
    old = f"v{i - 1}"
    results = [None] * nstreams
    st = [threading.Thread(target=streamer, args=(results, k)) for k in range(nstreams)]
    [t.start() for t in st]
    time.sleep(0.5)
    text = open(src).read()
    text = re.sub(r'const VERSION: &str = "v\d+";', f'const VERSION: &str = "v{i}";', text)
    t0 = time.time()
    with open(src, "w") as f:
        f.write(text)
    while True:
        try:
            if get_version() == f"v{i}":
                break
        except Exception:
            pass
        if time.time() - t0 > 120:
            print("timeout waiting for", i)
            sys.exit(1)
        time.sleep(0.005)
    latencies.append(time.time() - t0)
    [t.join() for t in st]
    for r in results:
        lines = r["lines"]
        builds = {tuple(l.split()[:2]) for l in lines}
        ok = (r.get("terminated") and len(lines) == want and lines[-1].endswith(" done")
              and len(builds) == 1 and next(iter(builds))[0] == old)
        streams["completed_on_old_build" if ok else "failed"] += 1
        stream_secs.append(r["secs"])
        if not ok:
            bad.append({"restart": i, "error": r.get("error"), "terminated": r.get("terminated"),
                        "lines": len(lines), "last": lines[-1] if lines else None,
                        "builds": sorted(" ".join(b) for b in builds), "secs": round(r["secs"], 2)})
    if i % 10 == 0 or bad:
        with lock:
            print(f"{i} restarts, streams {dict(streams)}, {sum(stats.values())} short requests, "
                  f"errors {dict(errs)}", flush=True)
stop.set()
[t.join() for t in ts]
lat = sorted(latencies)
print(json.dumps({
    "restarts": restarts, "stream_secs": secs, "streams_per_restart": nstreams,
    "streams": dict(streams), "failed_streams": bad[:20],
    "stream_duration_s": {"min": round(min(stream_secs), 2), "max": round(max(stream_secs), 2)},
    "short_requests": sum(stats.values()), "short_results": dict(stats), "short_errors": dict(errs),
    "save_to_serve_s": {"median": round(statistics.median(lat), 3), "max": round(lat[-1], 3)},
}))
