# giong on Windows: the completion-port driver, as a prototype

Branch-only work for the Windows release of giong 1.x. Not published, not a
member of the workspace, builds on Windows only.

giong's drivers keep one contract (`src/driver/mod.rs`): an operation is
started together with its buffers, reports one result, and a future dropped
early leaves its buffers with the driver until the kernel can no longer
touch them. Windows completion ports work the same way, so the driver here
is a completion driver beside io_uring, with no readiness emulation: a
receive reads straight into the caller's buffer.

This crate holds that driver alone, to prove the parts of Windows that
carry the risk before the runtime is ported onto the shared portability
layer:

| What | How | Test |
|---|---|---|
| Accept | `AcceptEx` into a socket created ahead, then `SO_UPDATE_ACCEPT_CONTEXT` | `tcp_echo_through_accept_and_connect`, `many_accepts_wait_at_once_and_each_gets_one_connection` |
| Connect | `ConnectEx` on a bound socket, then `SO_UPDATE_CONNECT_CONTEXT` | `tcp_echo_through_accept_and_connect`, `connecting_to_a_closed_port_is_refused_with_its_error_code` |
| Stream I/O | `WSARecv`, `WSASend` | `a_megabyte_arrives_whole_in_both_directions` |
| Idle connections hold no buffer | a receive of zero bytes waits for data | `a_zero_byte_receive_waits_for_data_and_holds_no_buffer` |
| Datagrams | `WSARecvFrom`, `WSASendTo` | `datagrams_roundtrip_with_their_sender` |
| Files | overlapped `ReadFile`, `WriteFile` at an offset; no blocking pool | `files_read_and_write_at_offsets` |
| Cancellation | `CancelIoEx`; buffers (and an accept's socket) stay with the driver until the completion arrives | `a_dropped_receive_keeps_its_buffer_until_the_kernel_lets_go`, `a_dropped_accept_closes_the_socket_made_for_it`, `a_receive_cancelled_as_data_arrives_takes_the_bytes_or_leaves_them` |
| Wake-up from another thread | the bell is the port: `PostQueuedCompletionStatus` | `the_bell_wakes_a_parked_worker_from_another_thread` |
| Results inside the call | `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`: an operation that finishes at once costs one system call and no trip through the port | every test runs with it on and off |
| Shutdown | cancel everything, wait for every completion | `shutdown_cancels_what_is_in_flight` |

## Running it

On Windows: `cargo test`. The `Windows` workflow runs it on the
`windows-2022` and `windows-2025` runners.

From Linux, for a quick loop while writing (Wine is not Windows; the
runners are the verdict):

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt-get install mingw-w64 wine64
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=/usr/lib/wine/wine64 \
  cargo test --target x86_64-pc-windows-gnu
```

Under Wine 9 `a_peer_that_resets_is_reported_as_a_reset` fails most runs:
Wine reports a reset peer as an orderly close.

## Known gaps, to close when it moves into `src/driver/iocp.rs`

- Multishot accept and receive (`MultiIo`), written as loops over these
  single operations with pool buffers.
- `TransmitFile`, vectored sends, `WSARecvMsg`/`WSASendMsg`.
- One listening socket shared by the workers: a handle belongs to one port,
  so one worker's port sees every accept and hands the sockets out.
- Timer resolution of the wait (about 15.6 ms by default on Windows).
- Sockets layered by an installed provider (LSP) that is not an IFS handle
  cannot use completions inside the call; detect and turn it off for them.
- Registered I/O (RIO), after the completion-port driver is in.
