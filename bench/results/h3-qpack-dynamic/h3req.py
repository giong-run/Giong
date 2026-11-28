"""aioquic HTTP/3 client: N requests on one connection, repeated big headers.
Prints status codes and a digest of bodies. Usage: h3req.py host port n [path]"""
import asyncio, sys, ssl, time
from aioquic.quic.connection import QuicConnection
import socket, contextlib
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import HeadersReceived, DataReceived
from aioquic.quic.configuration import QuicConfiguration

HDR = [0]
_orig = H3Connection._encode_headers
def _counted(self, sid, headers):
    out = _orig(self, sid, headers); HDR[0] += len(out); return out
H3Connection._encode_headers = _counted

class P(QuicConnectionProtocol):
    def __init__(self, *a, **k):
        super().__init__(*a, **k)
        self.h3 = H3Connection(self._quic)
        self.waiting = {}
    def quic_event_received(self, ev):
        for e in self.h3.handle_event(ev):
            st = self.waiting.get(e.stream_id)
            if st is None: continue
            if isinstance(e, HeadersReceived): st[1].extend(e.headers)
            elif isinstance(e, DataReceived): st[2].extend(e.data)
            if getattr(e, "stream_ended", False): st[0].set_result(None)
    async def get(self, host, path, extra):
        sid = self._quic.get_next_available_stream_id()
        hdrs = [(b":method", b"GET"), (b":scheme", b"https"), (b":authority", host.encode()), (b":path", path.encode())] + extra
        fut = asyncio.get_event_loop().create_future()
        self.waiting[sid] = (fut, [], bytearray())
        self.h3.send_headers(sid, hdrs, end_stream=True)
        self.transmit()
        await asyncio.wait_for(fut, 10)
        _, h, b = self.waiting.pop(sid)
        return dict(h).get(b":status"), bytes(b)

@contextlib.asynccontextmanager
async def connect(host, port, configuration, create_protocol):
    loop = asyncio.get_event_loop()
    configuration.server_name = host
    conn = QuicConnection(configuration=configuration)
    transport, protocol = await loop.create_datagram_endpoint(
        lambda: create_protocol(conn), family=socket.AF_INET, local_addr=("127.0.0.1", 0))
    protocol.connect(("127.0.0.1", port))
    await protocol.wait_connected()
    try:
        yield protocol
    finally:
        protocol.close(); await protocol.wait_closed(); transport.close()

async def main():
    host, port, n = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    path = sys.argv[4] if len(sys.argv) > 4 else "/"
    cfg = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, verify_mode=ssl.CERT_NONE)
    extra = [(b"user-agent", b"aioquic-interop/1.2 (giong test client; long enough to matter)"),
             (b"cookie", b"session=" + b"a" * 200), (b"x-trace", b"0123456789abcdef" * 4)]
    async with connect(host, port, configuration=cfg, create_protocol=P) as p:
        t = time.time(); statuses = {}
        for i in range(n):
            s, body = await p.get(host, path, extra)
            statuses[s] = statuses.get(s, 0) + 1
        st = p._quic._streams
        print("statuses", statuses, "secs", round(time.time() - t, 3),
              "max_table", p.h3._max_table_capacity if hasattr(p.h3, "_max_table_capacity") else "?",
              "encoder_stream_id", p.h3._local_encoder_stream_id,
              "header block bytes", HDR[0], "enc stream bytes", p.h3._encoder_bytes_sent, "dec bytes recv", p.h3._decoder_bytes_received)
asyncio.run(main())
