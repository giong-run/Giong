//! Any bytes as one request to the MCP server, through its checks and its
//! JSON-RPC dispatch: the answer is one the server means to send, and no
//! request without the token gets through (see `giong-mcp/src/fuzzing.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mcp::fuzzing::check_request(data);
});
