#!/usr/bin/env python3
"""Slow uploads against a server, with a well-behaved client mixed in.

    slow-uploads.py PORT H1 H2

Opens H1 HTTP/1.1 connections and H2 HTTP/2 (prior knowledge) connections to
127.0.0.1:PORT. Each sends `POST /upload` announcing 1,000,000 bytes of body,
sends 999,999 of them (over HTTP/2 as the server's flow control allows) and
then holds the connection, one byte short, so that a server that buffers
bodies holds almost all of it. Meanwhile another thread sends a `GET /` and a
small `POST` on a fresh connection every 100 ms, and those must all be
answered with 200.

Prints one line per kind of client and exits 1 if a well-behaved request
failed. Refusals of the uploads (a 503, a reset stream, a closed connection)
are what a server with a memory budget does, and are counted, not failures.
Standard library only, so that it runs on any CI runner.
"""

import socket
import struct
import sys
import threading
import time

BODY = 1_000_000
CHUNK = b"x" * 65536
HOST = "127.0.0.1"
TIMEOUT = 10


def h1_upload(port):
    """One HTTP/1.1 upload; returns how it went, and the socket if it is
    held (`settle` looks at it again at the end)."""
    s = socket.create_connection((HOST, port), timeout=TIMEOUT)
    try:
        s.sendall(b"POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n" % BODY)
        left = BODY - 1
        while left:
            n = min(left, len(CHUNK))
            s.sendall(CHUNK[:n])
            left -= n
    except OSError:
        return "closed", None
    return "held", s


def settle(s):
    """What became of an HTTP/1.1 upload that seemed held: a server that
    refuses it answers (and closes) while the body is still being sent."""
    s.setblocking(False)
    try:
        answer = s.recv(64)
    except BlockingIOError:
        return "held"
    except OSError:
        return "closed"
    finally:
        s.setblocking(True)
    if answer.startswith(b"HTTP/1.1 503"):
        return "refused"
    return "answered %r" % answer[:12] if answer else "closed"


# ------------------------------------------------------------------ HTTP/2

PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
DATA, HEADERS, RST_STREAM, SETTINGS, GOAWAY, WINDOW_UPDATE = 0, 1, 3, 4, 7, 8
END_STREAM, END_HEADERS, ACK = 0x1, 0x4, 0x1
INITIAL_WINDOW_SIZE = 4


def frame(kind, flags, stream, payload=b""):
    return struct.pack(">I", len(payload))[1:] + bytes([kind, flags]) + struct.pack(">I", stream) + payload


def literal(index, value):
    """A header field without indexing, its name from the static table."""
    value = value.encode()
    if index < 15:
        head = bytes([index])
    else:
        head = bytes([15, index - 15])
    return head + bytes([len(value)]) + value


def request_block(length):
    return (
        bytes([0x83, 0x86])  # :method POST, :scheme http
        + literal(4, "/upload")  # :path
        + literal(1, "x")  # :authority
        + literal(28, str(length))  # content-length
    )


class H2:
    def __init__(self, port):
        self.s = socket.create_connection((HOST, port), timeout=TIMEOUT)
        self.buf = b""
        self.conn_window = 65535
        self.initial = 65535
        self.window = 0
        self.reset = None
        self.goaway = False
        self.settings_seen = False
        self.s.sendall(PREFACE + frame(SETTINGS, 0, 0))

    def read_frame(self):
        while len(self.buf) < 9 or len(self.buf) < 9 + int.from_bytes(self.buf[:3], "big"):
            chunk = self.s.recv(65536)
            if not chunk:
                raise OSError("closed")
            self.buf += chunk
        n = int.from_bytes(self.buf[:3], "big")
        kind, flags = self.buf[3], self.buf[4]
        stream = int.from_bytes(self.buf[5:9], "big") & 0x7FFFFFFF
        payload = self.buf[9 : 9 + n]
        self.buf = self.buf[9 + n :]
        if kind == SETTINGS and not flags & ACK:
            for i in range(0, len(payload), 6):
                key, value = struct.unpack(">HI", payload[i : i + 6])
                if key == INITIAL_WINDOW_SIZE:
                    self.window += value - self.initial
                    self.initial = value
            self.settings_seen = True
            self.s.sendall(frame(SETTINGS, ACK, 0))
        elif kind == WINDOW_UPDATE:
            more = int.from_bytes(payload[:4], "big") & 0x7FFFFFFF
            if stream == 0:
                self.conn_window += more
            else:
                self.window += more
        elif kind == RST_STREAM:
            self.reset = int.from_bytes(payload[:4], "big")
        elif kind == GOAWAY:
            self.goaway = True
        return kind


def h2_upload(port):
    c = H2(port)
    try:
        # The server's SETTINGS read (and acknowledged), so that the stream
        # starts with the server's window.
        while not c.settings_seen:
            c.read_frame()
        c.window = c.initial
        c.s.sendall(frame(HEADERS, END_HEADERS, 1, request_block(BODY)))
        left = BODY - 1
        while left and c.reset is None and not c.goaway:
            n = min(left, c.window, c.conn_window, 16384)
            if n <= 0:
                c.read_frame()
                continue
            c.s.sendall(frame(DATA, 0, 1, CHUNK[:n]))
            c.window -= n
            c.conn_window -= n
            left -= n
        # Anything the server says right after the last piece.
        c.s.settimeout(0.05)
        try:
            while c.reset is None and not c.goaway:
                c.read_frame()
        except (socket.timeout, BlockingIOError):
            pass
        c.s.settimeout(TIMEOUT)
    except OSError:
        return "closed", None
    if c.reset == 7:
        return "refused", None
    if c.reset is not None or c.goaway:
        return "reset %s" % c.reset, None
    return "held", c.s


# ----------------------------------------------------------- well-behaved


class Checker(threading.Thread):
    def __init__(self, port):
        super().__init__(daemon=True)
        self.port = port
        self.ok = 0
        self.failed = []
        self.stop = threading.Event()

    def ask(self, raw):
        s = socket.create_connection((HOST, self.port), timeout=TIMEOUT)
        try:
            s.sendall(raw)
            answer = b""
            while b"\r\n\r\n" not in answer:
                chunk = s.recv(4096)
                if not chunk:
                    break
                answer += chunk
            return answer.split(b"\r\n", 1)[0]
        finally:
            s.close()

    def run(self):
        small = b"y" * 1000
        requests = [
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
            b"POST /small HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n%s" % (len(small), small),
        ]
        i = 0
        while not self.stop.is_set():
            try:
                status = self.ask(requests[i % 2])
            except OSError as e:
                status = str(e).encode()
            if status.startswith(b"HTTP/1.1 200"):
                self.ok += 1
            else:
                self.failed.append(status)
            i += 1
            time.sleep(0.1)


def main():
    port, n1, n2 = (int(a) for a in sys.argv[1:4])
    checker = Checker(port)
    checker.start()
    held = []
    outcomes = {"http/1.1": {}, "http/2": {}}
    # The two kinds in turn, so that both reach the server's budget.
    order = [("http/1.1", h1_upload)] * n1
    for i in range(n2):
        order.insert(min(2 * i + 1, len(order)), ("http/2", h2_upload))
    for kind, upload in order:
        try:
            outcome, sock = upload(port)
        except OSError as e:
            outcome, sock = "cannot connect: %s" % e, None
        if sock is not None:
            held.append((kind, sock))
        else:
            outcomes[kind][outcome] = outcomes[kind].get(outcome, 0) + 1
    # Hold everything for a moment, as an attacker would.
    time.sleep(3)
    checker.stop.set()
    checker.join()
    for kind, sock in held:
        outcome = settle(sock) if kind == "http/1.1" else "held"
        outcomes[kind][outcome] = outcomes[kind].get(outcome, 0) + 1
        sock.close()
    for kind, counts in outcomes.items():
        print("%s uploads: %s" % (kind, ", ".join("%s %d" % kv for kv in sorted(counts.items()))))
    print("well-behaved requests: %d answered with 200, %d not%s" % (
        checker.ok, len(checker.failed), (": %r" % checker.failed[:5]) if checker.failed else ""))
    sys.exit(1 if checker.failed or checker.ok == 0 else 0)


if __name__ == "__main__":
    main()
