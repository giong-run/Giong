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

/// The package name and version of the project at `root`, if any. A
/// `version.workspace = true` is read from the workspace's Cargo.toml.
pub fn package_at(root: &std::path::Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let name = toml_value(&text, "package", "name")?;
    let version = match toml_value(&text, "package", "version") {
        Some(v) => Some(v),
        None if inherits_version(&text) => root
            .ancestors()
            .skip(1)
            .filter_map(|dir| std::fs::read_to_string(dir.join("Cargo.toml")).ok())
            .find_map(|t| toml_value(&t, "workspace.package", "version")),
        None => None,
    };
    Some((name, version.unwrap_or_else(|| "0.0.0".into())))
}

/// A plain `key = "value"` in `[table]`, enough for a package's name and
/// version; `None` for anything else (`version.workspace = true`).
fn toml_value(text: &str, table: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line.trim_end_matches(|c: char| c != ']') == format!("[{table}]");
            continue;
        }
        if !inside {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        let quote = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &v[1..];
        return rest.find(quote).map(|end| rest[..end].to_string());
    }
    None
}

/// Whether `[package]` takes its version from the workspace.
fn inherits_version(text: &str) -> bool {
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[package]";
            continue;
        }
        let flat: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if inside
            && (flat.starts_with("version.workspace=true")
                || flat.starts_with("version={workspace=true"))
        {
            return true;
        }
    }
    false
}

/// `/files/{*path}` → `/files/{path}`: OpenAPI has no catch-all.
fn openapi_path(pattern: &str) -> String {
    pattern.replace("{*", "{")
}

/// `/users/{id}/files/{*rest}` → `/users/{}/files/{}`: two patterns with
/// one shape are one OpenAPI path, whatever their parameters are called.
fn shape(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                "{}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The parameter names of an OpenAPI path, in order.
fn names_in(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(str::to_string)
        .collect()
}

/// `get_users_by_id` from `GET /users/{id}`: readable, but not always
/// unique (`/users/by_id` gives the same), so [`document`] adds `_2`, `_3`.
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
/// `id` is the operation's unique id and `names` the parameter names of the
/// OpenAPI path it goes under (another route of the same shape may have
/// named them first).
fn operation(r: &Route, base: &str, id: &str, names: &[String]) -> Value {
    let mut op = Map::new();
    op.insert("operationId".into(), json!(id));
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
        .map(|(i, own)| {
            let last = i + 1 == r.params.len();
            let name = names.get(i).unwrap_or(own);
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
            if name != own {
                p["x-giong-name"] = json!(own);
            }
            p
        })
        .collect();
    if !params.is_empty() {
        op.insert("parameters".into(), json!(params));
    }
    if names.len() == r.params.len() && names.iter().zip(&r.params).any(|(a, b)| a != b) {
        op.insert("x-giong-pattern".into(), json!(r.pattern));
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
    const STANDARD: [&str; 8] = [
        "GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE",
    ];
    let mut paths: Map<String, Value> = Map::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut by_shape: std::collections::HashMap<String, String> = Default::default();
    let mut ids = std::collections::HashSet::new();
    for (_, routes) in apps {
        for r in routes {
            // Workers serving several apps can list one route twice.
            if !seen.insert((r.method.clone(), r.pattern.clone())) {
                continue;
            }
            // `/users/{id}` and `/users/{uid}` go under the path seen first.
            let key = by_shape
                .entry(shape(&r.pattern))
                .or_insert_with(|| openapi_path(&r.pattern))
                .clone();
            let names = names_in(&key);
            let base_id = operation_id(r);
            let mut id = base_id.clone();
            let mut n = 1;
            while !ids.insert(id.clone()) {
                n += 1;
                id = format!("{base_id}_{n}");
            }
            let op = operation(r, base, &id, &names);
            let path = paths
                .entry(key)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("object");
            // giong-web's methods are case-sensitive: only the standard
            // upper-case ones are OpenAPI operations, and one taken twice
            // (same shape, other names) is kept beside them.
            let slot = r.method.to_ascii_lowercase();
            if STANDARD.contains(&r.method.as_str()) && !path.contains_key(&slot) {
                path.insert(slot, op);
            } else {
                // OpenAPI 3.1 has no other methods: keep them visible.
                let other = path
                    .entry("x-giong-other-methods")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .expect("object");
                let mut name = r.method.clone();
                if other.contains_key(&name) {
                    name = format!("{} {}", r.method, r.pattern);
                }
                other.insert(name, op);
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
                .filter_map(Value::as_object)
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| match k.as_str() {
                            "x-giong-other-methods" => v.as_object().map_or(0, Map::len),
                            _ => 1,
                        })
                        .sum::<usize>()
                })
                .sum()
        })
        .unwrap_or(0);
    let text = serde_json::to_string_pretty(&doc).expect("JSON");
    let mut r = Report::new("openapi");
    match args.value("out") {
        Some(out) => {
            let path = PathBuf::from(out);
            // A committed folder link on the way (docs -> ~/.config) would
            // put the file outside the project: refused, never followed.
            if let Some(link) = service::link_on_the_way(&path) {
                return Err(Failure::new(
                    Code::USAGE,
                    format!(
                        "{} is a symbolic link, which giong openapi does not follow (it \
                         could name a folder outside the project)",
                        link.display()
                    ),
                    "remove the link, or give --out a path with no link in it",
                ));
            }
            // Written aside, then renamed over it: a committed link at
            // openapi.json is replaced, never followed.
            crate::state::replace(&path, format!("{text}\n").as_bytes()).map_err(|e| {
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
    fn operation_ids_are_readable() {
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

    /// Every operation in the document, with its path and method.
    fn operations(doc: &Value) -> Vec<(String, String, Value)> {
        let mut out = Vec::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                if method == "x-giong-other-methods" {
                    for (m, op) in op.as_object().unwrap() {
                        out.push((path.clone(), m.clone(), op.clone()));
                    }
                } else {
                    out.push((path.clone(), method.clone(), op.clone()));
                }
            }
        }
        out
    }

    #[test]
    fn operation_ids_are_unique_in_the_document() {
        let apps = vec![(
            vec![0],
            vec![
                route("GET", "/users/by_id", &[]),
                route("GET", "/users/{id}", &["id"]),
                route("GET", "/a-b", &[]),
                route("GET", "/a_b", &[]),
                route("GET", "/a.b", &[]),
                route("get", "/a_b", &[]),
            ],
        )];
        let doc = document(&apps, "api", "1", "http://h");
        let ops = operations(&doc);
        assert_eq!(ops.len(), 6);
        let ids: std::collections::BTreeSet<_> = ops
            .iter()
            .map(|(_, _, op)| op["operationId"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 6, "{ids:?}");
        assert_eq!(
            doc["paths"]["/users/by_id"]["get"]["operationId"],
            "get_users_by_id"
        );
        assert_eq!(
            doc["paths"]["/users/{id}"]["get"]["operationId"],
            "get_users_by_id_2"
        );
        assert_eq!(doc["paths"]["/a_b"]["get"]["operationId"], "get_a_b_2");
        assert_eq!(doc["paths"]["/a.b"]["get"]["operationId"], "get_a_b_3");
    }

    #[test]
    fn methods_keep_their_case() {
        // giong-web routes "get" and "GET" apart: neither replaces the other.
        for order in [["get", "GET"], ["GET", "get"]] {
            let apps = vec![(vec![0], order.iter().map(|m| route(m, "/x", &[])).collect())];
            let doc = document(&apps, "api", "1", "http://h");
            let x = &doc["paths"]["/x"];
            assert_eq!(x["get"]["summary"], "GET /x", "{order:?}");
            assert_eq!(
                x["x-giong-other-methods"]["get"]["summary"], "get /x",
                "{order:?}"
            );
            assert_eq!(operations(&doc).len(), 2);
        }
    }

    #[test]
    fn one_shape_is_one_path_whatever_the_names() {
        let apps = vec![(
            vec![0],
            vec![
                route("GET", "/users/{id}", &["id"]),
                route("DELETE", "/users/{uid}", &["uid"]),
                route("GET", "/users/{uid}/posts/{pid}", &["uid", "pid"]),
                route("PUT", "/users/{id}/posts/{post}", &["id", "post"]),
                route("POST", "/users/{user}", &["user"]),
                route("POST", "/users/{other}", &["other"]),
            ],
        )];
        let doc = document(&apps, "api", "1", "http://h");
        let paths = doc["paths"].as_object().unwrap();
        assert_eq!(
            paths.keys().collect::<Vec<_>>(),
            ["/users/{id}", "/users/{uid}/posts/{pid}"]
        );
        // Every operation names exactly the parameters of its path.
        for (path, method, op) in operations(&doc) {
            let want = names_in(&path);
            let got: Vec<_> = op["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(got, want, "{method} {path}");
        }
        let del = &doc["paths"]["/users/{id}"]["delete"];
        assert_eq!(del["x-giong-pattern"], "/users/{uid}");
        assert_eq!(del["parameters"][0]["x-giong-name"], "uid");
        let put = &doc["paths"]["/users/{uid}/posts/{pid}"]["put"];
        assert_eq!(put["parameters"][1]["x-giong-name"], "post");
        assert!(doc["paths"]["/users/{id}"]["get"]
            .get("x-giong-pattern")
            .is_none());
        // Two POSTs of one shape: the second is kept beside, not dropped.
        let users = &doc["paths"]["/users/{id}"];
        assert_eq!(users["post"]["x-giong-pattern"], "/users/{user}");
        assert_eq!(
            users["x-giong-other-methods"]["POST"]["x-giong-pattern"],
            "/users/{other}"
        );
    }

    #[test]
    fn package_names_and_versions_are_read_as_cargo_writes_them() {
        let dir = std::env::temp_dir().join(format!("giong-openapi-pkg-{}", std::process::id()));
        let member = dir.join("api");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"api\"]\n\n[workspace.package]\nversion = \"2.3.4\" # pinned\n",
        )
        .unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"my-api\" # the service\nversion.workspace = true\n\n[dependencies]\nname = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            package_at(&member),
            Some(("my-api".to_string(), "2.3.4".to_string()))
        );
        std::fs::write(
            member.join("Cargo.toml"),
            "[package] # here\nname = 'quoted'\nversion = \"0.4.2\"\n",
        )
        .unwrap();
        assert_eq!(
            package_at(&member),
            Some(("quoted".to_string(), "0.4.2".to_string()))
        );
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"bare\"\n").unwrap();
        assert_eq!(
            package_at(&member),
            Some(("bare".to_string(), "0.0.0".to_string()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_service_gives_an_empty_but_valid_document() {
        let doc = document(&[], "x", "0.1.0", "http://h");
        assert_eq!(doc["paths"], json!({}));
        assert_eq!(doc["servers"][0]["url"], "http://h");
    }
}
