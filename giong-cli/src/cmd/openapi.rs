//! `giong openapi`: an OpenAPI 3.1 document from the running service's
//! routes and the coded errors giong-web answers with.
//!
//! Exact where it can be: paths, methods, path parameters and the error
//! answers giong-web makes itself. Bodies are left undescribed, because
//! giong-web reads them at run time (`req.json::<T>()`) and cannot know `T`
//! without annotations; a guessed schema would mislead an agent.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::routes::{self, App, Route, BASE_FLAG, MCP_FLAG};
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::report::{Failure, Report};
use crate::service;

pub const COMMAND: Command = Command {
    name: "openapi",
    summary: "write an OpenAPI document from the running service's routes",
    usage: "giong openapi [--out <file>] [--title <text>] [--mcp <address>] [--base-url <url>]\n\n\
            Writes an OpenAPI 3.1 document for the routes the running service serves (read \
            from its giong-mcp, which needs the `web` feature): every path and method, path \
            parameters, the coded problem+json errors giong-web answers with, and a curl \
            example per operation. Request and response bodies are marked as not described: \
            giong-web cannot know their types without annotations. Without --out the document \
            goes to standard output (or into the JSON report with --json).",
    flags: &[
        Flag {
            name: "out",
            kind: Kind::Value,
            help: "write the document to this file instead of standard output",
        },
        Flag {
            name: "title",
            kind: Kind::Value,
            help: "the API's title (default: the package name from Cargo.toml)",
        },
        MCP_FLAG,
        BASE_FLAG,
    ],
    max_words: 0,
    run,
};

/// The package name and version of the project here, if any.
fn package() -> Option<(String, String)> {
    package_at(&service::project_root()?)
}

/// The package name and version of the project at `root`, if any.
pub fn package_at(root: &std::path::Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let mut in_package = false;
    let (mut name, mut version) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "name" => name = Some(v),
                "version" => version = Some(v),
                _ => {}
            }
        }
    }
    Some((name?, version.unwrap_or_else(|| "0.0.0".into())))
}

/// `/files/{*path}` → `/files/{path}`: OpenAPI has no catch-all.
fn openapi_path(pattern: &str) -> String {
    pattern.replace("{*", "{")
}

/// `get_users_by_id` from `GET /users/{id}`; unique within a document
/// because method and pattern shape are.
fn operation_id(r: &Route) -> String {
    let mut id = r.method.to_ascii_lowercase();
    for seg in r.pattern.split('/').filter(|s| !s.is_empty()) {
        id.push('_');
        if let Some(name) = seg
            .strip_prefix("{*")
            .or_else(|| seg.strip_prefix('{'))
            .and_then(|s| s.strip_suffix('}'))
        {
            id.push_str("by_");
            id.push_str(name);
        } else {
            id.extend(
                seg.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }),
            );
        }
    }
    if id == r.method.to_ascii_lowercase() {
        id.push_str("_root");
    }
    id
}

fn problem(codes: &[&str], what: &str) -> Value {
    json!({
        "description": format!("{what} ({})", codes.join(", ")),
        "content": { "application/problem+json": {
            "schema": { "$ref": "#/components/schemas/Problem" },
        }},
        "x-giong-codes": codes,
    })
}

/// The OpenAPI operation for one route.
fn operation(r: &Route, base: &str) -> Value {
    let mut op = Map::new();
    op.insert("operationId".into(), json!(operation_id(r)));
    op.insert(
        "summary".into(),
        json!(format!("{} {}", r.method, r.pattern)),
    );
    op.insert(
        "description".into(),
        json!(format!(
            "Added at {}. {} middleware.",
            r.added_at, r.middleware
        )),
    );
    let params: Vec<Value> = r
        .params
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let last = i + 1 == r.params.len();
            let mut p = json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string", "minLength": 1 },
            });
            if last && r.catch_all {
                p["description"] = json!(
                    "the rest of the path: may contain `/` (a giong-web catch-all); \
                     never `.` or `..` segments"
                );
                p["x-giong-catch-all"] = json!(true);
            } else {
                p["description"] = json!("one path segment, percent-decoded");
            }
            p
        })
        .collect();
    if !params.is_empty() {
        op.insert("parameters".into(), json!(params));
    }
    if let Some(limit) = r.stream_limit {
        op.insert(
            "requestBody".into(),
            json!({
                "required": false,
                "description": format!("read as a stream, up to {limit} bytes; not described"),
                "content": { "application/octet-stream": {} },
            }),
        );
        op.insert("x-giong-stream-limit".into(), json!(limit));
    } else if matches!(r.method.as_str(), "POST" | "PUT" | "PATCH") {
        op.insert(
            "requestBody".into(),
            json!({
                "required": false,
                "description": "not described: giong-web reads the body at run time",
                "content": { "application/json": {} },
            }),
        );
    }
    let mut responses = Map::new();
    responses.insert(
        "default".into(),
        json!({ "description": "the handler's answer (not described)" }),
    );
    if !r.params.is_empty() {
        responses.insert(
            "400".into(),
            problem(&["GIONG-E1205"], "a parameter that does not parse"),
        );
    }
    if r.stream_limit.is_some() {
        responses.insert(
            "413".into(),
            problem(
                &["GIONG-E1111"],
                "the body is longer than the route's limit",
            ),
        );
    } else if matches!(r.method.as_str(), "POST" | "PUT" | "PATCH") {
        responses.insert(
            "415".into(),
            problem(&["GIONG-E1208"], "a JSON body without a JSON Content-Type"),
        );
        responses.insert(
            "422".into(),
            problem(&["GIONG-E1207"], "a JSON body that does not fit"),
        );
    }
    responses.insert(
        "500".into(),
        problem(
            &["GIONG-E1210"],
            "the handler failed; the cause is in the log",
        ),
    );
    responses.insert(
        "503".into(),
        problem(
            &["GIONG-E1209", "GIONG-E1211"],
            "no answer in time, or a service the handler needs is down",
        ),
    );
    op.insert("responses".into(), Value::Object(responses));
    op.insert(
        "x-codeSamples".into(),
        json!([{ "lang": "Shell", "label": "curl", "source": routes::curl(base, r) }]),
    );
    Value::Object(op)
}

/// The whole document.
pub fn document(apps: &[App], title: &str, version: &str, base: &str) -> Value {
    let mut paths: Map<String, Value> = Map::new();
    let mut seen = std::collections::BTreeSet::new();
    for (_, routes) in apps {
        for r in routes {
            // Workers serving several apps can list one route twice.
            if !seen.insert((r.method.clone(), r.pattern.clone())) {
                continue;
            }
            let path = paths
                .entry(openapi_path(&r.pattern))
                .or_insert_with(|| json!({}));
            let method = r.method.to_ascii_lowercase();
            let standard = [
                "get", "put", "post", "delete", "options", "head", "patch", "trace",
            ];
            if standard.contains(&method.as_str()) {
                path[method] = operation(r, base);
            } else {
                // OpenAPI 3.1 has no other methods: keep them visible.
                let other = path
                    .as_object_mut()
                    .expect("object")
                    .entry("x-giong-other-methods")
                    .or_insert_with(|| json!({}));
                other[r.method.clone()] = operation(r, base);
            }
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": title,
            "version": version,
            "description": "Generated by giong openapi from the routes the service serves. \
                Paths, methods, path parameters and giong-web's coded errors are exact; bodies \
                are not described. HEAD is answered by each GET route and OPTIONS by the \
                router.",
        },
        "servers": [{ "url": base }],
        "paths": paths,
        "components": { "schemas": { "Problem": {
            "type": "object",
            "description": "RFC 9457 problem details, as giong-web writes them",
            "required": ["status", "code", "title"],
            "properties": {
                "status": { "type": "integer" },
                "code": { "type": "string", "description": "stable: a GIONG-E code or the handler's own" },
                "title": { "type": "string" },
                "detail": { "type": "string" },
                "request_id": { "type": "string" },
            },
        }}},
    })
}

fn run(args: &Args) -> Result<Report, Failure> {
    let apps = routes::fetch(args.value("mcp"))?;
    let base = routes::base_url(args.value("base-url"));
    let (name, version) = package().unwrap_or_else(|| ("service".into(), "0.0.0".into()));
    let title = args.value("title").map(str::to_string).unwrap_or(name);
    let doc = document(&apps, &title, &version, &base);
    let operations: usize = doc["paths"]
        .as_object()
        .map(|p| {
            p.values()
                .map(|v| v.as_object().map_or(0, |o| o.len()))
                .sum()
        })
        .unwrap_or(0);
    let text = serde_json::to_string_pretty(&doc).expect("JSON");
    let mut r = Report::new("openapi");
    match args.value("out") {
        Some(out) => {
            let path = PathBuf::from(out);
            std::fs::write(&path, format!("{text}\n")).map_err(|e| {
                Failure::new(
                    Code::USAGE,
                    format!("cannot write {}: {e}", path.display()),
                    "give --out a path in a directory that exists and that you can write",
                )
            })?;
            r.line(format!(
                "Wrote {} ({} paths, {operations} operations).",
                path.display(),
                doc["paths"].as_object().map_or(0, Map::len)
            ));
            r.data("out", json!(path.display().to_string()));
        }
        None => {
            if !args.json() {
                r.line(text);
            }
        }
    }
    if apps.is_empty() {
        r.line("The service serves no giong-web app: the document has no paths.");
    }
    r.data("document", doc);
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(method: &str, pattern: &str, params: &[&str]) -> Route {
        Route {
            method: method.into(),
            pattern: pattern.into(),
            params: params.iter().map(|s| s.to_string()).collect(),
            catch_all: pattern.contains("{*"),
            stream_limit: None,
            middleware: 1,
            added_at: "src/main.rs:9:5".into(),
        }
    }

    #[test]
    fn operation_ids_are_readable_and_unique() {
        assert_eq!(
            operation_id(&route("GET", "/users/{id}", &["id"])),
            "get_users_by_id"
        );
        assert_eq!(operation_id(&route("GET", "/", &[])), "get_root");
        assert_eq!(
            operation_id(&route("PUT", "/files/{*path}", &["path"])),
            "put_files_by_path"
        );
        assert_eq!(operation_id(&route("GET", "/a-b.c", &[])), "get_a_b_c");
    }

    #[test]
    fn the_document_has_paths_parameters_errors_and_samples() {
        let mut up = route("PUT", "/files/{*path}", &["path"]);
        up.stream_limit = Some(1024);
        let apps = vec![
            (
                vec![0, 1],
                vec![
                    route("GET", "/users/{id}", &["id"]),
                    route("POST", "/users", &[]),
                    up,
                    route("PROPFIND", "/dav", &[]),
                ],
            ),
            (vec![0], vec![route("GET", "/users/{id}", &["id"])]),
        ];
        let doc = document(&apps, "api", "1.2.3", "http://127.0.0.1:8080");
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["info"]["title"], "api");
        assert_eq!(doc["info"]["version"], "1.2.3");
        let get = &doc["paths"]["/users/{id}"]["get"];
        assert_eq!(get["parameters"][0]["name"], "id");
        assert_eq!(get["parameters"][0]["in"], "path");
        assert_eq!(get["responses"]["400"]["x-giong-codes"][0], "GIONG-E1205");
        assert!(get.get("requestBody").is_none());
        assert_eq!(
            get["x-codeSamples"][0]["source"],
            "curl -i 'http://127.0.0.1:8080/users/1'"
        );
        let post = &doc["paths"]["/users"]["post"];
        assert!(post["requestBody"]["content"]["application/json"].is_object());
        assert_eq!(post["responses"]["422"]["x-giong-codes"][0], "GIONG-E1207");
        assert!(post["responses"].get("400").is_none());
        let put = &doc["paths"]["/files/{path}"]["put"];
        assert_eq!(put["parameters"][0]["x-giong-catch-all"], true);
        assert_eq!(put["x-giong-stream-limit"], 1024);
        assert_eq!(put["responses"]["413"]["x-giong-codes"][0], "GIONG-E1111");
        assert!(doc["paths"]["/dav"]["x-giong-other-methods"]["PROPFIND"].is_object());
        assert_eq!(doc["paths"].as_object().unwrap().len(), 4);
        // Every $ref resolves.
        let text = doc.to_string();
        assert!(text.contains("#/components/schemas/Problem"));
        assert!(doc["components"]["schemas"]["Problem"].is_object());
    }

    #[test]
    fn an_empty_service_gives_an_empty_but_valid_document() {
        let doc = document(&[], "x", "0.1.0", "http://h");
        assert_eq!(doc["paths"], json!({}));
        assert_eq!(doc["servers"][0]["url"], "http://h");
    }
}
