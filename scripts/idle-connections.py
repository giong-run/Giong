#!/usr/bin/env python3
"""Idle connections against a server until it refuses them for low memory.

    idle-connections.py PORT MAX

Opens up to MAX HTTP/1.1 keep-alive connections to 127.0.0.1:PORT, from
source addresses 127.0.0.1 to 127.0.0.250 in turn (so that the local port
range is not the limit). Each sends `GET /`, reads the answer and stays open.
A server near its memory limit must refuse new connections with `503` and
`GIONG-E1121` in the body; once it has refused 20, the client checks that:

  * the connections the server holds are still served: the first, a middle
    and the last one each get `200` for another `GET /`;
  * the server takes connections again once memory is freed: after every
    held connection is closed, a new one gets `200` within 5 seconds.

Exits 1 if any check fails, or if the server never refused (MAX connections
fit, so the run proved nothing), or if a connection failed in any other way
(reset, closed, timed out: what a server killed for memory looks like).
Standard library only, so that it runs on any CI runner.
"""

import socket
import sys
import time

HOST = "127.0.0.1"
TIMEOUT = 10
REQUEST = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"


def answer(s):
    """Reads one response; returns (status, body)."""
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            raise ConnectionError("closed before the answer")
        buf += chunk
    head, _, body = buf.partition(b"\r\n\r\n")
    length = 0
    for line in head.split(b"\r\n")[1:]:
        k, _, v = line.partition(b":")
        if k.strip().lower() == b"content-length":
            length = int(v)
    while len(body) < length:
        chunk = s.recv(4096)
        if not chunk:
            raise ConnectionError("closed in the body")
        body += chunk
    return int(head.split()[1]), body


def connect(i, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(TIMEOUT)
    s.bind((f"127.0.0.{1 + i % 250}", 0))
    s.connect((HOST, port))
    s.sendall(REQUEST)
    return s, answer(s)


def main():
    port, most = int(sys.argv[1]), int(sys.argv[2])
    held, refused, failed = [], 0, 0
    start = time.monotonic()
    while len(held) < most and refused < 20:
        try:
            s, (status, body) = connect(len(held) + refused, port)
        except (OSError, ConnectionError) as e:
            failed += 1
            print(f"connection {len(held) + refused + 1} failed: {e!r}")
            if failed >= 5:
                break
            continue
        if status == 200:
            held.append(s)
        elif status == 503 and body.startswith(b"GIONG-E1121"):
            refused += 1
            s.close()
        else:
            print(f"unexpected answer: {status} {body[:60]!r}")
            failed += 1
            s.close()
    print(f"held {len(held)} idle connections, refused {refused} (GIONG-E1121), "
          f"{failed} failed, in {time.monotonic() - start:.1f} s")
    ok = failed == 0
    if refused == 0:
        print("FAILED: the server never refused; raise MAX or lower the memory limit")
        ok = False
    if held and refused:
        for i in sorted({0, len(held) // 2, len(held) - 1}):
            try:
                held[i].sendall(REQUEST)
                status, _ = answer(held[i])
            except (OSError, ConnectionError) as e:
                status = repr(e)
            if status != 200:
                print(f"FAILED: held connection {i + 1} got {status} instead of 200")
                ok = False
        print("held connections still served" if ok else "", end="\n" if ok else "")
    for s in held:
        s.close()
    if refused:
        deadline = time.monotonic() + 5
        while True:
            try:
                s, (status, _) = connect(0, port)
                s.close()
            except (OSError, ConnectionError) as e:
                status = repr(e)
            if status == 200:
                print("new connections served again after the idle ones closed")
                break
            if time.monotonic() > deadline:
                print(f"FAILED: a new connection still got {status} 5 s after the idle ones closed")
                ok = False
                break
            time.sleep(0.1)
    sys.exit(0 if ok else 1)


main()
