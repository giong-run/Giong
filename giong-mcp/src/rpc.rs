//! JSON-RPC 2.0 and the MCP methods a tools-only server answers.

use serde_json::{json, Map, Value};

use crate::tools::{self, State};

/// MCP protocol versions this server speaks, newest first.
pub(crate) const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// The one version that has JSON-RPC batches: 2025-06-18 removed them.
const BATCHES: &str = "2025-03-26";

/// Most messages in one batch.
pub(crate) const MAX_BATCH: usize = 16;

/// Most bytes in one answer, a batch's included. A tool's JSON travels
/// twice in it (as `structuredContent` and as text, escaped), so one tool
/// result may be a third of it.
pub(crate) const MAX_ANSWER: usize = 4 * 1024 * 1024;

const INSTRUCTIONS: &str = "\
This server looks inside a running service built on giong, a thread-per-core async runtime \
for Rust. The tools only read, except that `profile` makes every worker time its polls while it \
measures. Each worker is one thread with its own tasks; a task never moves between workers. \
Tasks are named by the source line that spawned them.\n\
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

/// Answers one message, or a batch of them, as the JSON text to send back.
/// `None` when nothing is to be sent back: notifications, and responses
/// from the client. `version` is the request's `MCP-Protocol-Version`.
pub(crate) async fn handle(state: &State, message: Value, version: Option<&str>) -> Option<String> {
    handle_within(state, message, version, MAX_ANSWER).await
}

async fn handle_within(
    state: &State,
    message: Value,
    version: Option<&str>,
    max_answer: usize,
) -> Option<String> {
    let refuse = |message: &str| Some(error_response(Value::Null, -32600, message).to_string());
    let batch = match message {
        Value::Array(batch) => batch,
        m => {
            let reply = handle_one(state, m, max_answer).await?.to_string();
            if reply.len() > max_answer {
                return Some(
                    error_response(Value::Null, -32603, &too_large(reply.len(), max_answer))
                        .to_string(),
                );
            }
            return Some(reply);
        }
    };
    // Clients of 2025-03-26 send no version header.
    if version.is_some_and(|v| v != BATCHES) {
        return refuse(
            "this MCP version has no JSON-RPC batches (2025-06-18 removed them): send one \
             message per request",
        );
    }
    if batch.is_empty() {
        return refuse("empty batch");
    }
    if batch.len() > MAX_BATCH {
        return refuse(&format!(
            "a batch holds at most {MAX_BATCH} messages, and this one has {}: send fewer at a \
             time",
            batch.len()
        ));
    }
    let mut out = String::from("[");
    for m in batch {
        let Some(reply) = handle_one(state, m, max_answer).await else {
            continue;
        };
        let reply = reply.to_string();
        if out.len() + reply.len() + 1 > max_answer {
            return refuse(&format!(
                "{}: send fewer messages per batch",
                too_large(out.len() + reply.len() + 1, max_answer)
            ));
        }
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str(&reply);
    }
    out.push(']');
    (out.len() > 2).then_some(out)
}

fn too_large(bytes: usize, max: usize) -> String {
    format!("the answer would be {bytes} bytes, more than giong-mcp's limit of {max}")
}

async fn handle_one(state: &State, message: Value, max_answer: usize) -> Option<Value> {
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
    let method = match m.remove("method") {
        Some(Value::String(method)) => method,
        Some(_) => {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                -32600,
                "method must be a string",
            ))
        }
        // A response to a request of ours: this server sends none.
        None if m.contains_key("result") || m.contains_key("error") => return None,
        None => {
            return Some(error_response(
                id.unwrap_or(Value::Null),
                -32600,
                "a request needs a method",
            ))
        }
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
                Some(Ok(value)) => {
                    // Compact: the text is what most clients show the model,
                    // which pays for every byte.
                    let text = value.to_string();
                    if text.len() > max_answer / 3 {
                        tool_error(
                            id,
                            &format!(
                                "{}; ask for less: a smaller limit, or one worker",
                                too_large(text.len(), max_answer / 3)
                            ),
                        )
                    } else {
                        result_response(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": text }],
                                "structuredContent": value,
                                "isError": false,
                            }),
                        )
                    }
                }
                Some(Err(message)) => tool_error(id, &message),
            }
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    })
}

/// A tool call that failed in a way the model can correct.
fn tool_error(id: Value, message: &str) -> Value {
    result_response(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    )
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
