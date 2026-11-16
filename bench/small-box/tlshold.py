#!/usr/bin/env python3
"""How many idle HTTPS connections a server holds before it refuses or dies.

    tlshold.py PORT CGROUP MAX [STEP]

Opens HTTPS connections to 127.0.0.1:PORT in batches of STEP (default 2,000),
from source addresses 127.0.0.1 to 127.0.0.250 in turn, so that the local
port range is not the limit. Each connection does a full handshake (TLS 1.3
or whatever the server picks, no session resumption) and one keep-alive
`GET /json`, reads the answer, and then stays idle; the client keeps its TLS
state so that the server sees an ordinary idle keep-alive client.

After each batch it prints one CSV line:

    held,failed,mem_current_kib,rss_kib,sock_kib,kernel_kib,anon_kib,oom_kills

read from the server's cgroup (CGROUP: its directory under /sys/fs/cgroup)
and the resident memory of its processes. It stops at MAX connections, when a batch has
failures (refused, reset, timed out: the server stopped taking connections),
when the cgroup reports an out-of-memory kill, or when the server's cgroup is
gone (its process died). The last line says which:

    end,<held>,<reason>

Standard library only.
"""

import os
import socket
import ssl
import sys
import time

PORT = int(sys.argv[1])
CGROUP = sys.argv[2]
MAX = int(sys.argv[3])
STEP = int(sys.argv[4]) if len(sys.argv) > 4 else 2000
TIMEOUT = 10
REQUEST = b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n"

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
ctx.set_alpn_protocols(["http/1.1"])


def read_kib(path, key=None):
    try:
        with open(path) as f:
            if key is None:
                return int(f.read().split()[0]) // 1024
            for line in f:
                k, v = line.split()[:2]
                if k == key:
                    return int(v) // 1024
    except (OSError, ValueError):
        return -1
    return -1


def rss_kib():
    """The resident memory of every process in the cgroup (nginx has two)."""
    total = 0
    try:
        with open(os.path.join(CGROUP, "cgroup.procs")) as f:
            pids = f.read().split()
    except OSError:
        return -1
    for pid in pids:
        try:
            with open(f"/proc/{pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        total += int(line.split()[1])
        except OSError:
            pass
    return total


def oom_kills():
    try:
        with open(os.path.join(CGROUP, "memory.events")) as f:
            for line in f:
                k, v = line.split()
                if k == "oom_kill":
                    return int(v)
    except OSError:
        return -1
    return 0


def response_done(buf):
    head, sep, body = buf.partition(b"\r\n\r\n")
    if not sep:
        return False
    for line in head.split(b"\r\n")[1:]:
        k, _, v = line.partition(b":")
        if k.strip().lower() == b"content-length":
            return len(body) >= int(v)
    return True


def one(i):
    src = f"127.0.0.{1 + i % 250}"
    raw = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    raw.settimeout(TIMEOUT)
    raw.bind((src, 0))
    raw.connect(("127.0.0.1", PORT))
    s = ctx.wrap_socket(raw, server_hostname="localhost")
    s.sendall(REQUEST)
    buf = b""
    while not response_done(buf):
        chunk = s.recv(4096)
        if not chunk:
            raise ConnectionError("closed before the answer")
        buf += chunk
    if not buf.startswith(b"HTTP/1.1 200"):
        raise ConnectionError("answer %r" % buf[:20])
    return s


def sample(held, failed):
    m = lambda f, k=None: read_kib(os.path.join(CGROUP, f), k)
    print(f"{held},{failed},{m('memory.current')},{rss_kib()},{m('memory.stat', 'sock')},"
          f"{m('memory.stat', 'kernel')},{m('memory.stat', 'anon')},{oom_kills()}", flush=True)


def main():
    print("held,failed,mem_current_kib,rss_kib,sock_kib,kernel_kib,anon_kib,oom_kills", flush=True)
    held = []
    oom0 = oom_kills()
    time.sleep(1)
    sample(0, 0)
    reason = "max"
    while len(held) < MAX:
        failed = 0
        for _ in range(min(STEP, MAX - len(held))):
            try:
                held.append(one(len(held)))
            except (OSError, ssl.SSLError, ConnectionError):
                failed += 1
                if failed >= 20:
                    break
        time.sleep(1)
        sample(len(held), failed)
        oom = oom_kills()
        if oom > oom0:
            reason = "oom-kill"
            break
        if oom < 0:
            reason = "server-gone"
            break
        if failed:
            reason = "refused"
            break
    print(f"end,{len(held)},{reason}", flush=True)
    # Holds the connections until the caller has read what it needs.
    if os.environ.get("HOLD_SECS"):
        time.sleep(float(os.environ["HOLD_SECS"]))


main()
