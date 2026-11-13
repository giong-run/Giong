//! The `Trace` middleware (feature `tracing`): what reaches a subscriber.

use std::fmt;
use std::sync::{Arc, Mutex};

use giong_web::middleware::{RequestId, Trace};
use giong_web::{App, Error, Req, Request, Response, Router};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// A span or an event as the subscriber saw it.
#[derive(Clone, Debug, Default)]
struct Seen {
    name: String,
    level: Option<Level>,
    /// For an event: its span, explicit or the one entered.
    parent: Option<u64>,
    fields: Vec<(String, String)>,
}

impl Seen {
    fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Log {
    spans: Vec<Seen>,
    events: Vec<Seen>,
    entered: Vec<u64>,
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Log>>);

struct Fields<'a>(&'a mut Vec<(String, String)>);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

impl Subscriber for Recorder {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let mut seen = Seen {
            name: attrs.metadata().name().to_string(),
            level: Some(*attrs.metadata().level()),
            ..Seen::default()
        };
        attrs.record(&mut Fields(&mut seen.fields));
        let mut log = self.0.lock().unwrap();
        log.spans.push(seen);
        Id::from_u64(log.spans.len() as u64)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut log = self.0.lock().unwrap();
        let seen = &mut log.spans[span.into_u64() as usize - 1];
        values.record(&mut Fields(&mut seen.fields));
    }

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut log = self.0.lock().unwrap();
        let parent = match event.parent() {
            Some(id) => Some(id.into_u64()),
            None if event.is_contextual() => log.entered.last().copied(),
            None => None,
        };
        let mut seen = Seen {
            name: event.metadata().name().to_string(),
            level: Some(*event.metadata().level()),
            parent,
            ..Seen::default()
        };
        event.record(&mut Fields(&mut seen.fields));
        log.events.push(seen);
    }

    fn enter(&self, span: &Id) {
        self.0.lock().unwrap().entered.push(span.into_u64());
    }

    fn exit(&self, _: &Id) {
        self.0.lock().unwrap().entered.pop();
    }
}

async fn user(req: Req) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;
    tracing::info!(id, "loading the user");
    Ok(Response::text(format!("user {id}")))
}

async fn broken(_req: Req) -> Result<Response, Error> {
    Err(Error::internal("the database exploded"))
}

fn app() -> App {
    Router::new()
        .get("/users/{id}", user)
        .get("/broken", broken)
        .layer(RequestId::new())
        .layer(Trace::new())
        .build(())
}

/// Answers `target` with a recording subscriber in place: the response and
/// what the subscriber saw.
fn traced(target: &str) -> (Response, Log) {
    let recorder = Recorder::default();
    let raw = format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n");
    let req = Request::from_bytes(raw.as_bytes()).unwrap();
    let resp = tracing::subscriber::with_default(recorder.clone(), || {
        let app = app();
        giong::block_on(app.handle(req))
    });
    let log = std::mem::take(&mut *recorder.0.lock().unwrap());
    (resp, log)
}

#[test]
fn a_request_runs_in_a_span_with_its_route_and_id() {
    let (resp, log) = traced("/users/7");
    assert_eq!(resp.status(), 200);
    let id = resp.get_header("x-request-id").unwrap();

    let span = &log.spans[0];
    assert_eq!(
        (span.name.as_str(), span.level),
        ("request", Some(Level::INFO))
    );
    assert_eq!(span.get("http.request.method"), Some("GET"));
    assert_eq!(span.get("url.path"), Some("/users/7"));
    assert_eq!(span.get("http.route"), Some("/users/{id}"));
    assert_eq!(span.get("request_id"), Some(id));
    assert_eq!(span.get("worker"), Some("0"));
    assert_eq!(span.get("client.address"), None);
    assert_eq!(span.get("http.response.status_code"), Some("200"));

    // The handler's event is inside the span, then the span's own event.
    let [handler, done] = &log.events[..] else {
        panic!("{:?}", log.events);
    };
    assert_eq!(handler.parent, Some(1));
    assert_eq!(handler.get("message"), Some("loading the user"));
    assert_eq!(handler.get("id"), Some("7"));
    assert_eq!((done.parent, done.level), (Some(1), Some(Level::INFO)));
    assert_eq!(done.get("message"), Some("request"));
    assert_eq!(done.get("status"), Some("200"));
    assert!(done.get("duration_us").is_some());
    assert_eq!(done.get("error.code"), None);
}

#[test]
fn errors_carry_their_code_and_a_5xx_its_cause() {
    let (resp, log) = traced("/nothing");
    assert_eq!(resp.status(), 404);
    assert_eq!(log.spans[0].get("http.route"), None);
    let done = log.events.last().unwrap();
    assert_eq!(done.level, Some(Level::INFO));
    assert_eq!(done.get("error.code"), Some("GIONG-E1203"));
    assert_eq!(done.get("error.cause"), None);

    let (resp, log) = traced("/broken");
    assert_eq!(resp.status(), 500);
    assert_eq!(log.spans[0].get("http.response.status_code"), Some("500"));
    let done = log.events.last().unwrap();
    assert_eq!(done.level, Some(Level::ERROR));
    assert_eq!(done.get("message"), Some("request failed"));
    assert_eq!(done.get("error.code"), Some("GIONG-E1210"));
    assert!(
        done.get("error.cause")
            .unwrap()
            .contains("the database exploded"),
        "{done:?}"
    );
}
