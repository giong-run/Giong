//! `start_from_env`: one test, alone in its process, since it sets
//! environment variables.

use std::net::TcpListener;

use giong::error::Code;
use giong_mcp::{start_from_env, FromEnv};

#[test]
fn start_from_env_never_stops_the_service() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();

    std::env::remove_var("GIONG_MCP");
    std::env::remove_var("GIONG_MCP_TOKEN");
    assert!(matches!(start_from_env(&handle), FromEnv::Off));

    // Set, but not usable: the error is kept, and the service goes on.
    std::env::set_var("GIONG_MCP", "127.0.0.1:0");
    let started = start_from_env(&handle);
    assert_eq!(
        started.error().and_then(giong::error::code),
        Some(Code::McpNoToken),
        "{started:?}"
    );
    std::env::set_var("GIONG_MCP_TOKEN", "");
    let started = start_from_env(&handle);
    assert_eq!(
        started.error().and_then(giong::error::code),
        Some(Code::McpBadToken),
        "{started:?}"
    );
    std::env::set_var("GIONG_MCP_TOKEN", "0123456789abcdef");
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    std::env::set_var("GIONG_MCP", holder.local_addr().unwrap().to_string());
    let started = start_from_env(&handle);
    assert_eq!(
        started.error().and_then(giong::error::code),
        Some(Code::McpBind),
        "{started:?}"
    );
    assert!(started.server().is_none());

    // And when it can, it runs for as long as the value is kept.
    std::env::set_var("GIONG_MCP", "127.0.0.1:0");
    let started = start_from_env(&handle);
    let addr = started.server().unwrap().local_addr().unwrap();
    assert!(started.error().is_none());
    std::net::TcpStream::connect(addr).unwrap();
    drop(started);
    assert!(std::net::TcpStream::connect(addr).is_err());
}
