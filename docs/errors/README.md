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

giong-tls and giong-postgres use the same codes. A code is never reused or renumbered,
and its page stays when it is no longer raised.

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

## Files

| Code | `Code::` | Meaning |
|---|---|---|
| [GIONG-E0200](E0200.md) | `InvalidPath` | invalid path |

## giong-http

None yet: its errors come from the network, or are HTTP responses sent to the client.

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
