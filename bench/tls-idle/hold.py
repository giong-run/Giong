#!/usr/bin/env python3
"""Memory per idle HTTPS connection of a server running on this machine.

    hold.py PORT N PID [PROTOCOL]

Opens N HTTPS keep-alive connections to 127.0.0.1:PORT one after the other;
each does a full handshake (no session resumption) and one `GET /json`,
reads the answer and stays open, with the client's TLS state kept, as an
idle keep-alive client. PROTOCOL is the one asked for through ALPN:
`http/1.1` (the default) or `h2` (the preface, SETTINGS and its
acknowledgement, and the request on stream 1). Then prints one CSV line with
the growth of the server's (PID) anonymous resident memory and resident set,
per connection:

    n,rss_anon_kib_per_conn,vmrss_kib_per_conn,vmrss_after_mib

Standard library only (OpenSSL through `ssl`).
"""

import socket
import ssl
import sys
import time

PORT, N, PID = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
PROTOCOL = sys.argv[4] if len(sys.argv) > 4 else "http/1.1"
REQUEST = b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n"


def frame(kind, flags, stream, payload=b""):
    return len(payload).to_bytes(3, "big") + bytes([kind, flags]) + stream.to_bytes(4, "big") + payload


# :method GET and :scheme https from the static table, :path and :authority
# as literals without indexing (RFC 7541).
H2_REQUEST = (
    b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
    + frame(0x4, 0, 0)
    + frame(0x1, 0x5, 1, b"\x82\x87\x04\x05/json\x01\x09localhost")
)


def get_h1(tls):
    tls.sendall(REQUEST)
    answer = b""
    while b"\r\n\r\n" not in answer or not answer.endswith(b"}"):
        chunk = tls.recv(4096)
        if not chunk:
            sys.exit("the server closed a connection")
        answer += chunk


def get_h2(tls):
    """Sends the request, acknowledges the server's SETTINGS, and reads frames
    until stream 1 ends."""
    tls.sendall(H2_REQUEST)
    buf = b""
    while True:
        while len(buf) < 9 or len(buf) < 9 + int.from_bytes(buf[:3], "big"):
            chunk = tls.recv(16384)
            if not chunk:
                sys.exit("the server closed a connection")
            buf += chunk
        length = int.from_bytes(buf[:3], "big")
        kind, flags = buf[3], buf[4]
        stream = int.from_bytes(buf[5:9], "big") & 0x7FFFFFFF
        buf = buf[9 + length:]
        if kind == 0x4 and not flags & 0x1:
            tls.sendall(frame(0x4, 0x1, 0))
        if stream == 1 and kind in (0x0, 0x1) and flags & 0x1:
            return


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
ctx.set_alpn_protocols([PROTOCOL])

time.sleep(0.5)
before = memory()
conns = []
for _ in range(N):
    tls = ctx.wrap_socket(socket.create_connection(("127.0.0.1", PORT)), server_hostname="localhost")
    if tls.selected_alpn_protocol() != PROTOCOL:
        sys.exit(f"the server chose {tls.selected_alpn_protocol()}, not {PROTOCOL}")
    if PROTOCOL == "h2":
        get_h2(tls)
    else:
        get_h1(tls)
    conns.append(tls)
time.sleep(1.0)
after = memory()
print(
    f"{N},{(after['RssAnon'] - before['RssAnon']) / N:.2f},"
    f"{(after['VmRSS'] - before['VmRSS']) / N:.2f},{after['VmRSS'] / 1024:.1f}"
)
