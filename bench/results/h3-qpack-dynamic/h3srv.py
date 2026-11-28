"""aioquic HTTP/3 server whose responses repeat the heads a web server
sends, to exercise a client's QPACK decoder with pylsqpack's encoder.
Prints its byte counters every 100 requests of a connection.
Usage: h3srv.py port cert key"""
import asyncio, sys
from aioquic.asyncio import serve
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import HeadersReceived
from aioquic.quic.configuration import QuicConfiguration

_orig = H3Connection._encode_headers
def _counted(self, sid, headers):
    out = _orig(self, sid, headers)
    self._hdr = getattr(self, "_hdr", 0) + len(out)
    return out
H3Connection._encode_headers = _counted

class P(QuicConnectionProtocol):
    def __init__(self, *a, **k):
        super().__init__(*a, **k)
        self.h3 = H3Connection(self._quic)
        self.n = 0
    def quic_event_received(self, ev):
        for e in self.h3.handle_event(ev):
            if isinstance(e, HeadersReceived) and e.stream_ended:
                self.n += 1
                body = b"hello\n"
                self.h3.send_headers(e.stream_id, [
                    (b":status", b"200"), (b"server", b"aioquic-interop/1.2.0"),
                    (b"content-type", b"text/plain; charset=utf-8"),
                    (b"cache-control", b"private, max-age=0, must-revalidate"),
                    (b"set-cookie", b"session=" + b"b" * 120 + b"; Path=/; Secure; HttpOnly"),
                    (b"strict-transport-security", b"max-age=63072000; includeSubDomains"),
                    (b"x-request-id", str(self.n).encode()),
                    (b"content-length", str(len(body)).encode())])
                self.h3.send_data(e.stream_id, body, end_stream=True)
                self.transmit()
                if self.n % 100 == 0:
                    print("requests", self.n, "response header block bytes", self.h3._hdr,
                          "encoder stream bytes", self.h3._encoder_bytes_sent,
                          "decoder bytes received", self.h3._decoder_bytes_received, flush=True)

async def main():
    port, cert, key = int(sys.argv[1]), sys.argv[2], sys.argv[3]
    cfg = QuicConfiguration(is_client=False, alpn_protocols=H3_ALPN)
    cfg.load_cert_chain(cert, key)
    await serve("127.0.0.1", port, configuration=cfg, create_protocol=P)
    await asyncio.Future()

asyncio.run(main())
