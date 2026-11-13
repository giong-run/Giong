//! JSON-RPC 2.0 and the MCP methods a tools-only server answers.

use serde_json::{json, Map, Value};

use crate::tools::{self, State};

/// MCP protocol versions this server speaks, newest first.
pub(crate) const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

const INSTRUCTIONS: &str = "\
This server looks inside a running service built on giong, a thread-per-core async runtime \
for Rust. Every tool is read-only. Each worker is one thread with its own tasks; a task never \
moves between workers. Tasks are named by the source line that spawned them.\n\
Start with `overview`. A worker reported as blocked has been inside one poll of one task for \
that long: the task blocks the thread (a blocking call, a long computation, a loop with no \
.await), and the location says which task. `tasks` shows what is alive (a count that keeps \
growing at one location is a leak), `sources` shows connection pools, and `profile` measures \
where the workers spend their time over a short window.";

pub(crate) fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Answers one message, or a batch of them. `None` when nothing is to be
/// sent back: notifications, and responses from the client.
pub(crate) async fn handle(state: &State, message: Value) -> Option<Value> {
    match message {
        Value::Array(batch) => {
            if batch.is_empty() {
                return Some(error_response(Value::Null, -32600, "empty batch"));
            }
            let mut replies = Vec::new();
            for m in batch {
                if let Some(reply) = handle_one(state, m).await {
                    replies.push(reply);
                }
            }
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        m => handle_one(state, m).await,
    }
}

async fn handle_one(state: &State, message: Value) -> Option<Value> {
    let Value::Object(mut m) = message else {
        return Some(error_response(
            Value::Null,
            -32600,
            "a JSON-RPC message is an object",
        ));
    };
    let id = m.remove("id");
    if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "jsonrpc must be \"2.0\"",
        ));
    }
    let Some(method) = m.get("method").and_then(Value::as_str).map(str::to_owned) else {
        // A response to a request of ours: this server sends none.
        return None;
    };
    let params = match m.remove("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p,
        Some(_) => {
            return id.map(|id| error_response(id, -32602, "params must be an object"));
        }
    };
    // A notification: act on it if it means something, never answer.
    let id = id?;
    if !matches!(id, Value::String(_) | Value::Number(_)) {
        return Some(error_response(
            Value::Null,
            -32600,
            "id must be a string or a number",
        ));
    }
    Some(match method.as_str() {
        "initialize" => result_response(id, initialize(&params)),
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(id, json!({ "tools": tools::list() })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(id, -32602, "tools/call needs a tool name"));
            };
            let args = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(a)) => a.clone(),
                Some(_) => {
                    return Some(error_response(id, -32602, "arguments must be an object"));
                }
            };
            match tools::call(state, name, &args).await {
                None => error_response(id, -32602, &format!("unknown tool: {name}")),
                Some(Ok(value)) => result_response(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": pretty(&value) }],
                        "structuredContent": value,
                        "isError": false,
                    }),
                ),
                Some(Err(message)) => result_response(
                    id,
                    json!({
                        "content": [{ "type": "text", "text": message }],
                        "isError": true,
                    }),
                ),
            }
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn initialize(params: &Map<String, Value>) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .and_then(|v| VERSIONS.iter().find(|&&known| known == v))
        .copied()
        .unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "giong-mcp",
            "title": "giong runtime",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_clients_when_known() {
        let v = initialize(
            json!({ "protocolVersion": "2025-03-26" })
                .as_object()
                .unwrap(),
        );
        assert_eq!(v["protocolVersion"], "2025-03-26");
        let v = initialize(
            json!({ "protocolVersion": "1999-01-01" })
                .as_object()
                .unwrap(),
        );
        assert_eq!(v["protocolVersion"], VERSIONS[0]);
        assert_eq!(v["capabilities"]["tools"]["listChanged"], false);
    }
}
