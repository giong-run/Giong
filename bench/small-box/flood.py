#!/usr/bin/env python3
"""A slow-upload flood: how many held uploads a server takes before it dies.

    flood.py PORT CGROUP MAX

Opens HTTP/1.1 connections to 127.0.0.1:PORT one after another, up to MAX.
Each sends `POST /upload` announcing 1,000,000 bytes of body, sends 999,999
of them and then holds the connection, one byte short, so that a server that
buffers bodies in memory holds almost all of it. A well-behaved client asks
for `GET /json` on a fresh connection every 100 ms meanwhile.

Every 25 uploads it prints one CSV line, read from the server's cgroup
(CGROUP: its directory under /sys/fs/cgroup):

    uploads,held,refused,mem_current_kib,mem_peak_kib,oom_kills,ok,bad

`held` counts uploads whose connection is still open and unanswered,
`refused` those the server answered early (a 503, a 413) or closed. It stops
at MAX, or when the cgroup reports an out-of-memory kill; the last line is

    end,<uploads>,<held>,<refused>,<reason>,<ok>,<bad>

where ok and bad count the well-behaved requests answered with 200 and not.
Standard library only.
"""

import os
import socket
import sys
import threading
import time

PORT = int(sys.argv[1])
CGROUP = sys.argv[2]
MAX = int(sys.argv[3])
BODY = 1_000_000
CHUNK = b"x" * 65536
TIMEOUT = 10


def cg(name, key=None):
    try:
        with open(os.path.join(CGROUP, name)) as f:
            if key is None:
                return int(f.read().split()[0])
            for line in f:
                k, v = line.split()
                if k == key:
                    return int(v)
    except (OSError, ValueError):
        return -1
    return 0


def upload(i):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(TIMEOUT)
    s.bind((f"127.0.0.{1 + i % 250}", 0))
    s.connect(("127.0.0.1", PORT))
    s.sendall(b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\n"
              b"Content-Length: %d\r\n\r\n" % BODY)
    left = BODY - 1
    while left:
        n = min(left, len(CHUNK))
        s.sendall(CHUNK[:n])
        left -= n
    return s


def still_held(s):
    s.setblocking(False)
    try:
        s.recv(64)  # an answer or the end of the stream: not held
        return False
    except BlockingIOError:
        return True
    except OSError:
        return False
    finally:
        s.setblocking(True)


class Checker(threading.Thread):
    def __init__(self):
        super().__init__(daemon=True)
        self.ok = 0
        self.bad = 0
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            try:
                s = socket.create_connection(("127.0.0.1", PORT), timeout=TIMEOUT)
                s.sendall(b"GET /json HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                answer = s.recv(64)
                s.close()
                good = answer.startswith(b"HTTP/1.1 200")
            except OSError:
                good = False
            if good:
                self.ok += 1
            else:
                self.bad += 1
            time.sleep(0.1)


def main():
    print("uploads,held,refused,mem_current_kib,mem_peak_kib,oom_kills,ok,bad", flush=True)
    oom0 = cg("memory.events", "oom_kill")
    checker = Checker()
    checker.start()
    socks, refused, reason = [], 0, "max"
    for i in range(MAX):
        try:
            socks.append(upload(i))
        except OSError:
            refused += 1
        if (i + 1) % 25 == 0 or i + 1 == MAX:
            held = sum(1 for s in socks if still_held(s))
            oom = cg("memory.events", "oom_kill")
            print(f"{i + 1},{held},{i + 1 - held},{cg('memory.current') // 1024},"
                  f"{cg('memory.peak') // 1024},{oom},{checker.ok},{checker.bad}", flush=True)
            if oom > oom0:
                reason = "oom-kill"
                break
    time.sleep(3)
    checker.stop.set()
    checker.join()
    held = sum(1 for s in socks if still_held(s))
    print(f"end,{i + 1},{held},{i + 1 - held},{reason},{checker.ok},{checker.bad}", flush=True)
    for s in socks:
        s.close()


main()
