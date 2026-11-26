#!/usr/bin/env python3
"""Memory per idle HTTPS connection of a server running on this machine.

    hold.py PORT N PID

Opens N HTTPS keep-alive connections to 127.0.0.1:PORT one after the other;
each does a full handshake (no session resumption) and one `GET /json`,
reads the answer and stays open, with the client's TLS state kept, as an
idle keep-alive client. Then prints one CSV line with the growth of the
server's (PID) anonymous resident memory and resident set, per connection:

    n,rss_anon_kib_per_conn,vmrss_kib_per_conn,vmrss_after_mib

Standard library only (OpenSSL through `ssl`).
"""

import socket
import ssl
import sys
import time

PORT, N, PID = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
REQUEST = b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n"


def memory():
    out = {}
    with open(f"/proc/{PID}/status") as f:
        for line in f:
            key, value = line.split(":", 1)
            if key in ("VmRSS", "RssAnon"):
                out[key] = int(value.split()[0])
    return out


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
ctx.set_alpn_protocols(["http/1.1"])

time.sleep(0.5)
before = memory()
conns = []
for _ in range(N):
    tls = ctx.wrap_socket(socket.create_connection(("127.0.0.1", PORT)), server_hostname="localhost")
    tls.sendall(REQUEST)
    answer = b""
    while b"\r\n\r\n" not in answer or not answer.endswith(b"}"):
        chunk = tls.recv(4096)
        if not chunk:
            sys.exit("the server closed a connection")
        answer += chunk
    conns.append(tls)
time.sleep(1.0)
after = memory()
print(
    f"{N},{(after['RssAnon'] - before['RssAnon']) / N:.2f},"
    f"{(after['VmRSS'] - before['VmRSS']) / N:.2f},{after['VmRSS'] / 1024:.1f}"
)
