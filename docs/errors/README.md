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
| [GIONG-E0014](E0014.md) | `ComputeBusy` | the compute lane is full |
| [GIONG-E0016](E0016.md) | `SignalNotAllowed` | this signal cannot be listened for |
| [GIONG-E0017](E0017.md) | `FdPassing` | descriptors could not be passed |
| [GIONG-E0018](E0018.md) | `ActivatedSocket` | the inherited socket cannot be used that way |
| [GIONG-E0019](E0019.md) | `ChildProcess` | cannot watch the child process |
| [GIONG-E0020](E0020.md) | `ThreadLimit` | a worker thread could not be started |
| [GIONG-E0080](E0080.md) | `BudgetFull` | the budget is at its limit |
| [GIONG-E0081](E0081.md) | `BudgetTooSmall` | a reservation larger than the budget |

## macOS

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0028](E0028.md) | `NetworkBuffersExhausted` | the system ran out of network buffers |

## Windows

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0060](E0060.md) | `UnixSocketInUse` | a file is already at the Unix socket path |
| [GIONG-E0061](E0061.md) | `UnixPathTooLong` | Unix socket path too long |
| [GIONG-E0062](E0062.md) | `UnixSocketsUnavailable` | no Unix domain sockets on this Windows |

## Messages between processes (`giong::ipc`)

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0090](E0090.md) | `IpcMessageTooLarge` | IPC message too large |
| [GIONG-E0091](E0091.md) | `IpcCorrupted` | IPC channel memory corrupted |
| [GIONG-E0092](E0092.md) | `IpcCapacity` | IPC capacity out of range |
| [GIONG-E0093](E0093.md) | `IpcHandshake` | IPC handshake failed |
| [GIONG-E0094](E0094.md) | `IpcPeerGone` | IPC peer process gone |
| [GIONG-E0095](E0095.md) | `IpcReceiverClosed` | IPC receiver closed |

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
| [GIONG-E0204](E0204.md) | `FileTooLarge` | file too large to read into memory |
| [GIONG-E0205](E0205.md) | `OutsideRoot` | the path leads outside the root |

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
| [GIONG-E1123](E1123.md) | `HttpServerOutOfFiles` | connection not accepted: the server is out of file descriptors |
| [GIONG-E1124](E1124.md) | `HttpServerAddressLimit` | connection refused: the client's address has too many connections open |

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

## giong-cli

The `giong` command reports `GIONG-E5000` to `GIONG-E5099`. Its findings can also carry the runtime's own codes (`GIONG-E0001` when io_uring is blocked, for example).

| Code | Name | Meaning |
|---|---|---|
| [GIONG-E5000](E5000.md) | `Usage` | the command line is not valid |
| [GIONG-E5001](E5001.md) | `NotAProject` | no Cargo project here |
| [GIONG-E5002](E5002.md) | `CargoFailed` | cargo could not be run |
| [GIONG-E5003](E5003.md) | `RustTooOld` | the Rust compiler is too old |
| [GIONG-E5004](E5004.md) | `TokioCrate` | a dependency needs the Tokio runtime |
| [GIONG-E5005](E5005.md) | `UnknownCode` | no such error code |
| [GIONG-E5006](E5006.md) | `BlockingCall` | a blocking call inside a worker |
| [GIONG-E5007](E5007.md) | `HostNameBind` | binding a host name |
| [GIONG-E5008](E5008.md) | `BorrowAcrossAwait` | a RefCell borrow held across an await |
| [GIONG-E5009](E5009.md) | `BuildFailed` | the code does not compile |
| [GIONG-E5010](E5010.md) | `Lint` | clippy has a finding |
| [GIONG-E5011](E5011.md) | `DiskUsed` | build directories use a lot of disk |
| [GIONG-E5012](E5012.md) | `SlowDebugInfo` | the dev profile writes full debug information |
| [GIONG-E5013](E5013.md) | `OpenFilesLimit` | the open-files limit is low |
| [GIONG-E5014](E5014.md) | `PortInUse` | the port is in use |
| [GIONG-E5015](E5015.md) | `DockerSeccomp` | Docker blocks io_uring |
| [GIONG-E5016](E5016.md) | `EnvMissing` | a variable from .env.example is not set |
| [GIONG-E5017](E5017.md) | `ServiceExited` | the service stopped |
| [GIONG-E5018](E5018.md) | `ServiceNotReady` | the service did not start listening |
| [GIONG-E5019](E5019.md) | `McpUnreachable` | the service's giong-mcp does not answer |
| [GIONG-E5020](E5020.md) | `WatchLimit` | too many files to watch |
| [GIONG-E5021](E5021.md) | `NoLinker` | no linker found |
| [GIONG-E5022](E5022.md) | `DevRunning` | giong dev already serves this project |
| [GIONG-E5023](E5023.md) | `CompilerWarning` | the compiler has a warning |
| [GIONG-E5024](E5024.md) | `ListensOnNetwork` | the service would listen on every network interface |
| [GIONG-E5030](E5030.md) | `BadProjectName` | the project name is not valid |
| [GIONG-E5031](E5031.md) | `TargetNotEmpty` | the directory for the new project is not empty |
| [GIONG-E5032](E5032.md) | `BadTemplateChoice` | the template does not offer that choice |
| [GIONG-E5033](E5033.md) | `WriteFailed` | a file of the project could not be written |
| [GIONG-E5034](E5034.md) | `WindowsLimited` | the project's crates do not build on Windows |
| [GIONG-E5035](E5035.md) | `BadGiongPath` | --giong-path is not a giong checkout |
| [GIONG-E5036](E5036.md) | `AgentFileForeign` | an agent file exists that giong did not write |
| [GIONG-E5037](E5037.md) | `McpJsonInvalid` | .mcp.json is not valid JSON |
| [GIONG-E5038](E5038.md) | `VersionMismatch` | the project uses another giong version than this command |
| [GIONG-E5039](E5039.md) | `NotGiongProject` | the project does not depend on giong |
| [GIONG-E5040](E5040.md) | `NoContainerRuntime` | neither Docker nor Podman answers |
| [GIONG-E5041](E5041.md) | `DbNotReady` | the database does not answer queries |
| [GIONG-E5042](E5042.md) | `DbUrl` | no usable database URL |
| [GIONG-E5043](E5043.md) | `MigrationChanged` | an applied migration was changed |
| [GIONG-E5044](E5044.md) | `MigrationFailed` | a migration failed |
| [GIONG-E5045](E5045.md) | `MigrationName` | a migration file is not valid |
| [GIONG-E5046](E5046.md) | `DbClientMissing` | no database client to run the SQL |
| [GIONG-E5047](E5047.md) | `MigrationUnknown` | the database has a migration the project does not |
| [GIONG-E5048](E5048.md) | `DbNotManaged` | the database was not started by giong db up |
| [GIONG-E5049](E5049.md) | `DbStartFailed` | the database container could not start |
| [GIONG-E5050](E5050.md) | `RecordSecret` | a recorded value looks like a secret |
| [GIONG-E5051](E5051.md) | `DashboardToken` | the dashboard refused a request without its token |
| [GIONG-E5053](E5053.md) | `DashboardPortInUse` | the dashboard's port is in use |
| [GIONG-E5054](E5054.md) | `DashboardFailed` | the dashboard could not do what the page asked |
| [GIONG-E5055](E5055.md) | `WorkerBlocked` | a worker of the service is stuck in one task |
| [GIONG-E5056](E5056.md) | `ServiceWarning` | the service reports a warning |
| [GIONG-E5057](E5057.md) | `NoService` | no running service to ask |
| [GIONG-E5058](E5058.md) | `McpNoTool` | the service's giong-mcp lacks a tool |
| [GIONG-E5059](E5059.md) | `McpToolError` | the service's giong-mcp refused a call |
| [GIONG-E5060](E5060.md) | `BenchTarget` | the service to measure cannot be reached |
| [GIONG-E5061](E5061.md) | `BenchAgainst` | the build to compare against cannot be made |
| [GIONG-E5062](E5062.md) | `BenchGeneratorBehind` | the load generator fell behind its schedule |
| [GIONG-E5063](E5063.md) | `BenchErrors` | requests failed during the benchmark |
| [GIONG-E5064](E5064.md) | `BenchRegression` | the change is slower than the build it is compared with |
| [GIONG-E5065](E5065.md) | `NoRecording` | no recorded requests to use |
| [GIONG-E5066](E5066.md) | `RecordedCaseFailed` | a recorded case answers differently |
| [GIONG-E5067](E5067.md) | `RecordingInvalid` | a recording cannot be read or used |
| [GIONG-E5068](E5068.md) | `RecordValueMissing` | a recorded case needs a value that is not set |
| [GIONG-E5069](E5069.md) | `ReplayUnreachable` | the service to replay against does not answer |
| [GIONG-E5070](E5070.md) | `ImageTarget` | the service cannot be built for Linux here |
| [GIONG-E5071](E5071.md) | `ImageLibraries` | the shared libraries of the binary cannot be found |
| [GIONG-E5072](E5072.md) | `ImageNoCerts` | no CA certificates to put in the image |
| [GIONG-E5073](E5073.md) | `ImageAdd` | a file to add to the image is not usable |
| [GIONG-E5074](E5074.md) | `ImageLoad` | the image could not be loaded into Docker or Podman |
| [GIONG-E5075](E5075.md) | `ImagePush` | the image could not be pushed to the registry |
| [GIONG-E5076](E5076.md) | `ImageTag` | the image name or tag is not valid |
| [GIONG-E5080](E5080.md) | `FixDirty` | the project has changes that are not committed |
| [GIONG-E5081](E5081.md) | `FixFailed` | cargo could not apply its fixes |
| [GIONG-E5082](E5082.md) | `FixConflict` | a fix no longer matches the code |
| [GIONG-E5083](E5083.md) | `TestFailed` | a test failed |
| [GIONG-E5084](E5084.md) | `DriverNotHere` | the tests could not run on every driver |
| [GIONG-E5085](E5085.md) | `NextestMissing` | cargo-nextest is not installed |
| [GIONG-E5086](E5086.md) | `TestTimeout` | the tests took longer than the time limit |
| [GIONG-E5087](E5087.md) | `GitInitFailed` | git init failed in the new project |
| [GIONG-E5088](E5088.md) | `DotenvBuildSetting` | .env sets a cargo or rustc setting |
| [GIONG-E5090](E5090.md) | `GiongOutdated` | a newer giong release is out |
| [GIONG-E5091](E5091.md) | `GiongMixed` | the project's giong crates are on different releases |
| [GIONG-E5092](E5092.md) | `CliOutdated` | this giong command is older than the newest release |
| [GIONG-E5093](E5093.md) | `UpgradeTarget` | the version to move to is not a release giong upgrade can move to |
| [GIONG-E5094](E5094.md) | `MsrvBelowTarget` | the project's rust-version is below what the release needs |
| [GIONG-E5095](E5095.md) | `PinnedVersion` | a giong dependency is pinned to a version |
| [GIONG-E5096](E5096.md) | `SelfUpdateFailed` | giong upgrade --self could not replace this command |
| [GIONG-E5097](E5097.md) | `ReleaseUnknown` | the newest giong release could not be looked up |
| [GIONG-E5098](E5098.md) | `ManifestShape` | a giong dependency is written in a form giong upgrade cannot edit |
