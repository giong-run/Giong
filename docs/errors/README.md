# Error codes

Every error giong raises itself starts with a code and ends with a link to its page here:

```text
GIONG-E0102: no address found for db.internal. Fix: check the spelling of the name,
/etc/hosts, and the DNS servers in /etc/resolv.conf (in a container, the network it is
attached to). See https://github.com/giong-run/Giong/blob/main/docs/errors/E0102.md
```

Errors giong only passes on, such as "connection refused" from the kernel, have no code.
Functions still return `std::io::Error`; match on the code, not on the message:

```rust
use giong::error::{self, Code};

fn on_error(err: &std::io::Error) {
    if error::code(err) == Some(Code::HostNotFound) {
        // ...
    }
}
```

giong-http, giong-web, giong-tls, giong-postgres, giong-mysql, giong-redis, giong-mcp and
giong-compat use the same codes. A code is never reused or renumbered, and its page stays
when it is no longer raised.

## The runtime

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0001](E0001.md) | `IoUringUnavailable` | io_uring is not available |
| [GIONG-E0002](E0002.md) | `KernelTooOld` | the kernel is too old |
| [GIONG-E0003](E0003.md) | `SignalsTaken` | another runtime handles the signals |
| [GIONG-E0004](E0004.md) | `InvalidConfig` | invalid runtime setting |
| [GIONG-E0005](E0005.md) | `Unexpected` | unexpected result from the kernel |
| [GIONG-E0006](E0006.md) | `MixedReads` | read used after recv |
| [GIONG-E0007](E0007.md) | `TimedOut` | the time limit passed |
| [GIONG-E0008](E0008.md) | `NoSuchWorker` | no such worker |
| [GIONG-E0009](E0009.md) | `BufferRingRefused` | the kernel refused io_uring buffer rings |
| [GIONG-E0010](E0010.md) | `LockedMemoryLimit` | the locked-memory limit is used up |
| [GIONG-E0011](E0011.md) | `NotInRuntime` | not inside a giong runtime |
| [GIONG-E0012](E0012.md) | `RuntimeAlreadyRunning` | a runtime already runs on this thread |
| [GIONG-E0013](E0013.md) | `WrongWorker` | an operation was polled on another worker |

## Network and name resolution

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0100](E0100.md) | `InvalidAddress` | invalid address |
| [GIONG-E0101](E0101.md) | `InvalidHostName` | invalid host name |
| [GIONG-E0102](E0102.md) | `HostNotFound` | host not found |
| [GIONG-E0103](E0103.md) | `NoAddresses` | no address to use |
| [GIONG-E0104](E0104.md) | `NoDnsServer` | no DNS server configured |
| [GIONG-E0105](E0105.md) | `DnsTimeout` | DNS server did not answer |
| [GIONG-E0106](E0106.md) | `DnsServerFailure` | DNS server failed |
| [GIONG-E0107](E0107.md) | `DnsBadResponse` | malformed DNS answer |
| [GIONG-E0108](E0108.md) | `DnsServerUnreachable` | cannot reach the DNS server |
| [GIONG-E0109](E0109.md) | `EmptyBuffer` | buffer with no room |
| [GIONG-E0110](E0110.md) | `RecvBufferTooSmall` | receive buffers too small for GRO |
| [GIONG-E0111](E0111.md) | `RecvAlreadyStarted` | receive option set after recv started |

## Files

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0200](E0200.md) | `InvalidPath` | invalid path |
| [GIONG-E0201](E0201.md) | `FileTooShort` | the file ended before the bytes to send |
| [GIONG-E0202](E0202.md) | `OffsetOutOfRange` | file offset out of range |
| [GIONG-E0203](E0203.md) | `InvalidOpenOptions` | invalid combination of open options |

## giong-http

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E1101](E1101.md) | `HttpClientRequest` | invalid HTTP request |
| [GIONG-E1102](E1102.md) | `HttpClientConnect` | cannot connect to the HTTP server |
| [GIONG-E1103](E1103.md) | `HttpClientTimeout` | HTTP request timed out |
| [GIONG-E1104](E1104.md) | `HttpClientTooLarge` | HTTP response too large |
| [GIONG-E1105](E1105.md) | `HttpClientRedirect` | redirect not followed |
| [GIONG-E1106](E1106.md) | `HttpClientClosed` | connection closed before the response |
| [GIONG-E1107](E1107.md) | `HttpClientProtocol` | malformed HTTP response |
| [GIONG-E1108](E1108.md) | `HttpClientStatus` | HTTP error status |
| [GIONG-E1109](E1109.md) | `HttpClientDecode` | cannot decode the response body |
| [GIONG-E1110](E1110.md) | `HttpClientProxy` | the HTTP proxy refused the request |
| [GIONG-E1111](E1111.md) | `HttpBodyTooLarge` | request body too large |
| [GIONG-E1112](E1112.md) | `HttpBodyIncomplete` | request body cut short |
| [GIONG-E1113](E1113.md) | `HttpBodyMalformed` | malformed request body |
| [GIONG-E1114](E1114.md) | `HttpBodyNotSent` | response body not sent |
| [GIONG-E1115](E1115.md) | `Http2Protocol` | HTTP/2 protocol error |
| [GIONG-E1116](E1116.md) | `Http2TooManyResets` | too many HTTP/2 streams reset |
| [GIONG-E1117](E1117.md) | `Http2Flood` | too many HTTP/2 control frames |
| [GIONG-E1118](E1118.md) | `Http2HeadersTooLarge` | HTTP/2 header block too large |
| [GIONG-E1119](E1119.md) | `HttpInvalidResponse` | invalid HTTP response |
| [GIONG-E1120](E1120.md) | `HttpServerBusy` | request refused: the server's memory budget for requests is full |
| [GIONG-E1121](E1121.md) | `HttpServerLowMemory` | connection refused: the server is low on memory |
| [GIONG-E1122](E1122.md) | `HttpInvalidTrailer` | invalid HTTP trailer field |

## giong-web

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E1201](E1201.md) | `WebInvalidRoute` | invalid route pattern |
| [GIONG-E1202](E1202.md) | `WebDuplicateRoute` | route added twice |
| [GIONG-E1203](E1203.md) | `WebNoRoute` | no route for the path |
| [GIONG-E1204](E1204.md) | `WebMethodNotAllowed` | method not allowed |
| [GIONG-E1205](E1205.md) | `WebBadParameter` | invalid path or query parameter |
| [GIONG-E1206](E1206.md) | `WebNoSuchParameter` | the route has no such parameter |
| [GIONG-E1207](E1207.md) | `WebBadBody` | unreadable request body |
| [GIONG-E1208](E1208.md) | `WebUnsupportedMediaType` | unsupported Content-Type |
| [GIONG-E1209](E1209.md) | `WebTimeout` | no response in time |
| [GIONG-E1210](E1210.md) | `WebInternal` | the handler failed |
| [GIONG-E1211](E1211.md) | `WebUnavailable` | a service the handler needs is unavailable |
| [GIONG-E1212](E1212.md) | `WebCorsRejected` | cross-origin request refused |
| [GIONG-E1213](E1213.md) | `WebConfig` | invalid giong-web setting |

## giong-mcp

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E1301](E1301.md) | `McpNotLocal` | the MCP server listens on this machine only |
| [GIONG-E1302](E1302.md) | `McpBadAddress` | invalid address for the MCP server |
| [GIONG-E1303](E1303.md) | `McpBind` | the MCP server cannot listen on its address |
| [GIONG-E1304](E1304.md) | `McpNoToken` | the MCP server needs a token on TCP |
| [GIONG-E1305](E1305.md) | `McpBadToken` | invalid token for the MCP server |

## giong-compat

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E1401](E1401.md) | `CompatWriteFailed` | an earlier buffered write failed |
| [GIONG-E1402](E1402.md) | `CompatShutDown` | write after shutdown |
| [GIONG-E1403](E1403.md) | `CompatConfig` | invalid giong-compat setting |

## giong-tls

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E2001](E2001.md) | `NoTrustedRoots` | no trusted certificate authorities |
| [GIONG-E2002](E2002.md) | `BadCertificateData` | unreadable certificate data |
| [GIONG-E2003](E2003.md) | `InvalidServerName` | invalid TLS server name |
| [GIONG-E2004](E2004.md) | `TlsCertificate` | certificate rejected |
| [GIONG-E2005](E2005.md) | `TlsProtocol` | TLS protocol error |
| [GIONG-E2006](E2006.md) | `TlsHandshakeTimeout` | TLS handshake timed out |
| [GIONG-E2007](E2007.md) | `TlsHandshakeEof` | connection closed during the TLS handshake |
| [GIONG-E2008](E2008.md) | `TlsBroken` | TLS stream broken |
| [GIONG-E2009](E2009.md) | `KtlsUnavailable` | kernel TLS is not available |
| [GIONG-E2010](E2010.md) | `KtlsSetup` | the kernel refused the TLS keys |
| [GIONG-E2011](E2011.md) | `KtlsKeyUpdate` | the kernel cannot change TLS keys |
| [GIONG-E2012](E2012.md) | `TlsKeysExhausted` | TLS keys used up |
| [GIONG-E2013](E2013.md) | `TlsConfig` | TLS setting not possible here |

## giong-postgres

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E3001](E3001.md) | `PostgresConfig` | invalid PostgreSQL configuration |
| [GIONG-E3002](E3002.md) | `PostgresConnect` | cannot connect to PostgreSQL |
| [GIONG-E3003](E3003.md) | `PostgresAuth` | PostgreSQL authentication failed |
| [GIONG-E3004](E3004.md) | `PostgresServer` | PostgreSQL server error |
| [GIONG-E3005](E3005.md) | `PostgresType` | PostgreSQL type mismatch |
| [GIONG-E3006](E3006.md) | `PostgresRowCount` | wrong number of rows |
| [GIONG-E3007](E3007.md) | `PostgresTooLarge` | PostgreSQL result too large |
| [GIONG-E3008](E3008.md) | `PostgresClosed` | PostgreSQL connection closed |
| [GIONG-E3009](E3009.md) | `PostgresPoolTimeout` | PostgreSQL pool wait timed out |
| [GIONG-E3010](E3010.md) | `PostgresProtocol` | PostgreSQL protocol error |
| [GIONG-E3011](E3011.md) | `PostgresStatement` | statement refused before sending |

## giong-mysql

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E3101](E3101.md) | `MysqlConfig` | invalid MySQL configuration |
| [GIONG-E3102](E3102.md) | `MysqlConnect` | cannot connect to MySQL |
| [GIONG-E3103](E3103.md) | `MysqlAuth` | MySQL authentication failed |
| [GIONG-E3104](E3104.md) | `MysqlServer` | MySQL server error |
| [GIONG-E3105](E3105.md) | `MysqlType` | MySQL type mismatch |
| [GIONG-E3106](E3106.md) | `MysqlRowCount` | wrong number of rows |
| [GIONG-E3107](E3107.md) | `MysqlTooLarge` | MySQL result too large |
| [GIONG-E3108](E3108.md) | `MysqlClosed` | MySQL connection closed |
| [GIONG-E3109](E3109.md) | `MysqlPoolTimeout` | MySQL pool wait timed out |
| [GIONG-E3110](E3110.md) | `MysqlProtocol` | MySQL protocol error |
| [GIONG-E3111](E3111.md) | `MysqlStatement` | statement refused before sending |

## giong-redis

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E3201](E3201.md) | `RedisConfig` | invalid Redis configuration |
| [GIONG-E3202](E3202.md) | `RedisConnect` | cannot connect to Redis |
| [GIONG-E3203](E3203.md) | `RedisAuth` | Redis authentication failed |
| [GIONG-E3204](E3204.md) | `RedisServer` | Redis server error |
| [GIONG-E3205](E3205.md) | `RedisType` | Redis type mismatch |
| [GIONG-E3206](E3206.md) | `RedisCommand` | command refused before sending |
| [GIONG-E3207](E3207.md) | `RedisTooLarge` | Redis reply too large |
| [GIONG-E3208](E3208.md) | `RedisClosed` | Redis connection closed |
| [GIONG-E3209](E3209.md) | `RedisPoolTimeout` | Redis pool wait timed out |
| [GIONG-E3210](E3210.md) | `RedisProtocol` | Redis protocol error |
| [GIONG-E3211](E3211.md) | `RedisTimeout` | Redis reply timed out |
| [GIONG-E3601](E3601.md) | `QuicConfig` | invalid QUIC configuration |
| [GIONG-E3602](E3602.md) | `QuicConnect` | cannot set up the QUIC connection |
| [GIONG-E3603](E3603.md) | `QuicConnectionLost` | the QUIC connection was lost |
| [GIONG-E3604](E3604.md) | `QuicStreamReset` | the QUIC stream was reset |
| [GIONG-E3605](E3605.md) | `QuicEarlyDataRejected` | the server refused the 0-RTT data |

## giong-proto

A prototype for giong 1.1, not published yet; its codes are `giong_proto::ErrorKind` until it joins `giong::error::Code`.

| Code | `ErrorKind::` | Meaning |
|---|---|---|
| [GIONG-E4001](E4001.md) | `ErrorKind::Truncated` | the message ends in the middle of a field |
| [GIONG-E4002](E4002.md) | `ErrorKind::BadVarint` | malformed varint |
| [GIONG-E4003](E4003.md) | `ErrorKind::BadTag` | invalid field tag |
| [GIONG-E4004](E4004.md) | `ErrorKind::BadGroup` | unbalanced group |
| [GIONG-E4005](E4005.md) | `ErrorKind::BadUtf8` | string field is not UTF-8 |
| [GIONG-E4006](E4006.md) | `ErrorKind::TooDeep` | messages nested too deeply |
| [GIONG-E4007](E4007.md) | `ErrorKind::BadPacked` | malformed packed field |
| [GIONG-E4008](E4008.md) | `ErrorKind::TooLarge` | message larger than 2 GiB |

## giong-grpc

New in giong 1.1. A call that fails ends with a gRPC status; the statuses giong-grpc raises itself carry one of these codes at the start of their message and in `Status::error_code`.

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E4101](E4101.md) | `GrpcInvalidMetadata` | invalid gRPC metadata |
| [GIONG-E4102](E4102.md) | `GrpcMessageTooLarge` | gRPC message too large |
| [GIONG-E4103](E4103.md) | `GrpcProtocol` | gRPC protocol error |
| [GIONG-E4104](E4104.md) | `GrpcEncoding` | unsupported gRPC message encoding |
| [GIONG-E4105](E4105.md) | `GrpcDecode` | gRPC message is not valid protobuf |
| [GIONG-E4106](E4106.md) | `GrpcTransport` | gRPC connection failed |
| [GIONG-E4107](E4107.md) | `GrpcEndpoint` | invalid gRPC endpoint |
| [GIONG-E4108](E4108.md) | `GrpcDeadline` | gRPC deadline exceeded |
| [GIONG-E4109](E4109.md) | `GrpcCancelled` | gRPC call cancelled |
| [GIONG-E4110](E4110.md) | `GrpcUnimplemented` | gRPC method not found |
