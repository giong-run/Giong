//! The shape of each tool's answer, as the `outputSchema` it declares in
//! `tools/list` (JSON Schema 2020-12). Clients that know it validate the
//! answers, and models see the fields before the first call.
//!
//! A field that is present only in some answers (a worker that did not
//! answer, say) is in `properties` but not in `required`. Objects accept
//! fields they do not list, so that adding one is not a breaking change.

use serde_json::{json, Value};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn array(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn int() -> Value {
    json!({ "type": "integer", "minimum": 0 })
}

fn num() -> Value {
    json!({ "type": "number" })
}

fn text() -> Value {
    json!({ "type": "string" })
}

fn or_null(mut schema: Value) -> Value {
    let kind = schema["type"].clone();
    schema["type"] = json!([kind, "null"]);
    schema
}

fn state() -> Value {
    json!({ "type": "string", "enum": ["running", "parked", "stopped", "unknown"] })
}

fn driver() -> Value {
    json!({ "type": ["string", "null"], "description": "io_uring or epoll" })
}

fn blocked() -> Value {
    json!({
        "type": ["object", "null"],
        "description": "Set when the worker has been inside one poll for at least 100 ms.",
        "properties": {
            "for_ms": int(),
            "in_task": or_null(json!({
                "type": "string",
                "description": "Where the task was spawned: file:line:column.",
            })),
        },
        "required": ["for_ms", "in_task"],
    })
}

pub(crate) fn overview() -> Value {
    let driver_choice = json!({
        "type": ["object", "null"],
        "properties": {
            "chosen_by": { "type": "string", "enum": ["builder", "GIONG_DRIVER", "automatic"] },
            "io_uring_error": or_null(text()),
            "io_uring_error_code": or_null(text()),
            "kernel_workarounds": array(object(
                json!({ "name": text(), "description": text() }),
                &["name", "description"],
            )),
        },
        "required": ["chosen_by", "io_uring_error", "io_uring_error_code", "kernel_workarounds"],
    });
    object(
        json!({
            "giong_mcp": text(),
            "process": object(
                json!({
                    "pid": int(),
                    "uptime_s": or_null(num()),
                    "rss_bytes": or_null(int()),
                    "threads": or_null(int()),
                    "open_fds": or_null(int()),
                    "fd_limit": or_null(int()),
                }),
                &["pid", "uptime_s", "rss_bytes", "threads", "open_fds", "fd_limit"],
            ),
            "runtime": object(
                json!({ "workers": int(), "shutting_down": { "type": "boolean" },
                        "profiling": { "type": "boolean" } }),
                &["workers", "shutting_down", "profiling"],
            ),
            "workers": array(object(
                json!({
                    "worker": int(),
                    "state": state(),
                    "driver": driver(),
                    "driver_choice": driver_choice,
                    "tasks": int(),
                    "runnable": int(),
                    "busy": or_null(num()),
                    "polls_per_s": or_null(num()),
                    "blocked": blocked(),
                }),
                &["worker", "state", "driver", "driver_choice", "tasks", "runnable", "busy",
                  "polls_per_s", "blocked"],
            )),
            "warnings": array(text()),
        }),
        &["giong_mcp", "process", "runtime", "workers", "warnings"],
    )
}

pub(crate) fn workers() -> Value {
    let last_second = json!({
        "type": ["object", "null"],
        "description": "Null until the server has watched the worker for 100 ms.",
        "properties": {
            "window_ms": int(),
            "polls_per_s": num(),
            "ticks_per_s": num(),
            "spawned_per_s": num(),
            "busy": num(),
        },
        "required": ["window_ms", "polls_per_s", "ticks_per_s", "spawned_per_s", "busy"],
    });
    object(
        json!({
            "workers": array(object(
                json!({
                    "worker": int(),
                    "state": state(),
                    "driver": driver(),
                    "counters": object(
                        json!({ "ticks": int(), "polls": int(), "parks": int(),
                                "idle_s": num(), "spawned": int() }),
                        &["ticks", "polls", "parks", "idle_s", "spawned"],
                    ),
                    "last_second": last_second,
                    "blocked": blocked(),
                    "tasks": int(),
                    "runnable": int(),
                    "timers": int(),
                    "io_in_flight": int(),
                    "recv_buffers": object(
                        json!({ "total": int(), "free": int() }),
                        &["total", "free"],
                    ),
                    "draining": { "type": "boolean" },
                    "snapshot_error": {
                        "type": "string",
                        "description": "The worker did not describe itself: blocked, or \
                                        stopped. timers, io_in_flight, recv_buffers and \
                                        draining are then missing.",
                    },
                }),
                &["worker", "state", "driver", "counters", "last_second", "blocked", "tasks",
                  "runnable"],
            )),
        }),
        &["workers"],
    )
}

pub(crate) fn tasks() -> Value {
    object(
        json!({
            "total_tasks": int(),
            "locations": int(),
            "by_location": array(object(
                json!({
                    "location": text(),
                    "tasks": int(),
                    "per_worker": {
                        "type": "array",
                        "items": int(),
                        "description": "Missing when the call asked for one worker.",
                    },
                }),
                &["location", "tasks"],
            )),
            "workers": array(object(
                json!({ "worker": int(), "tasks": int(), "error": text() }),
                &["worker"],
            )),
        }),
        &["total_tasks", "locations", "by_location", "workers"],
    )
}

pub(crate) fn sources() -> Value {
    let counters = json!({ "type": "object", "additionalProperties": int() });
    object(
        json!({
            "sources": array(object(
                json!({
                    "kind": text(),
                    "name": text(),
                    "total": counters,
                    "per_worker": array(json!({
                        "type": "object",
                        "properties": { "worker": int() },
                        "required": ["worker"],
                        "additionalProperties": int(),
                    })),
                }),
                &["kind", "name", "total", "per_worker"],
            )),
            "unanswered": array(object(
                json!({ "worker": int(), "error": text() }),
                &["worker", "error"],
            )),
            "note": text(),
        }),
        &["sources", "unanswered"],
    )
}

pub(crate) fn profile() -> Value {
    let row = |share: &str| {
        let mut props = json!({
            "location": text(),
            "polls": int(),
            "total_ms": num(),
            "max_us": num(),
        });
        props[share] = num();
        let mut required = vec!["location", "polls", "total_ms", share, "max_us"];
        if share == "share" {
            props["mean_us"] = num();
            required.push("mean_us");
        }
        object(props, &required)
    };
    object(
        json!({
            "duration_ms": num(),
            "total": array(row("share_of_all_workers")),
            "workers": array(object(
                json!({
                    "worker": int(),
                    "busy": num(),
                    "polls": int(),
                    "in_tasks_ms": num(),
                    "top": array(row("share")),
                    "error": text(),
                    "blocked_in": or_null(text()),
                }),
                &["worker", "busy"],
            )),
            "note": text(),
        }),
        &["duration_ms", "total", "workers", "note"],
    )
}
