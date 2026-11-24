//! Request tracing (`giong::observe::RequestTrace`) through giong-http's
//! server: HTTP/1.1 and HTTP/2 requests are followed from start to end,
//! grouped by route, and a request whose client goes away counts as
//! abandoned. One giong worker, client and server on it.

use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::observe::{self, Route, RouteStats, Wait};
use giong::time::sleep;
use giong_http::client::Client;
use giong_http::{Request, Response, Server, Version};

static FAST: Route = Route::from_static("GET /fast");
static SLOW: Route = Route::from_static("GET /slow");

/// Starts the server on this worker; returns its base URL.
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move {
        Server::new()
            .serve(listener, |req: Request| async move {
                if req.target() == "/slow" {
                    observe::set_route(&SLOW);
                    sleep(Duration::from_millis(300)).await;
                } else {
                    observe::set_route(&FAST);
                    let _upstream = observe::wait(Wait::Upstream, "test");
                    sleep(Duration::from_millis(2)).await;
                }
                Response::text("ok")
            })
            .await
    });
    format!("http://{addr}")
}

/// The report's statistics for `route`, all protocols together.
fn stats(report: &observe::RequestReport, route: &Route) -> RouteStats {
    let mut all: Option<RouteStats> = None;
    for r in report
        .routes
        .iter()
        .filter(|r| r.route.as_ref() == Some(route))
    {
        match &mut all {
            Some(all) => all.merge(r),
            None => all = Some(r.clone()),
        }
    }
    all.unwrap_or_else(|| panic!("no requests for {route}"))
}

#[test]
fn http1_and_http2_requests_are_traced_and_abandoned_ones_counted() {
    giong::block_on(async {
        let base = serve();
        let h = giong::handle();
        h.set_request_tracing(true);
        giong::yield_now().await;
        let _ = observe::take_requests();

        let h1 = Client::new();
        let h2 = Client::builder().http2_prior_knowledge(true).build();
        for client in [&h1, &h2] {
            for _ in 0..5 {
                let resp = client.get(&format!("{base}/fast")).send().await.unwrap();
                assert_eq!(resp.status(), 200);
            }
        }
        let resp = h2.get(&format!("{base}/fast")).send().await.unwrap();
        assert_eq!(resp.version(), Version::Http2);

        // Clients that give up while the handler runs. Over HTTP/2 the
        // server drops the request when the connection goes; over HTTP/1.1
        // it only reads again after the handler, and its answer may still
        // be written into the closed socket's buffer, so that request can
        // count either way.
        for client in [
            Client::new(),
            Client::builder().http2_prior_knowledge(true).build(),
        ] {
            let gave_up = giong::time::timeout(
                Duration::from_millis(30),
                client.get(&format!("{base}/slow")).send(),
            )
            .await;
            assert!(gave_up.is_err());
            drop(client);
        }

        // The server notices the clients that went away.
        let start = Instant::now();
        let mut report;
        let mut slow_abandoned = 0;
        let mut slow_answered = 0;
        let mut fast = None;
        loop {
            sleep(Duration::from_millis(10)).await;
            report = observe::take_requests();
            if report
                .routes
                .iter()
                .any(|r| r.route.as_ref() == Some(&FAST))
            {
                let s = stats(&report, &FAST);
                match &mut fast {
                    None => fast = Some(s),
                    Some(f) => f.merge(&s),
                }
            }
            for r in report
                .routes
                .iter()
                .filter(|r| r.route.as_ref() == Some(&SLOW))
            {
                slow_abandoned += r.abandoned;
                slow_answered += r.requests;
            }
            if slow_abandoned + slow_answered >= 2 || start.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        h.set_request_tracing(false);

        assert_eq!(
            slow_abandoned + slow_answered,
            2,
            "both clients that gave up"
        );
        assert!(slow_abandoned >= 1, "the HTTP/2 request is abandoned");
        let fast = fast.expect("requests to /fast");
        assert_eq!(fast.requests, 11);
        assert_eq!(fast.abandoned, 0);
        let all = fast.breakdown(0.0, 1.0);
        assert_eq!(all.requests, 11);
        // Each handler waited 2 ms for its "upstream".
        assert!(all.upstream >= Duration::from_millis(2), "{all:?}");
        assert!(all.total >= all.upstream, "{all:?}");
        for r in &fast.slowest {
            assert!(r.upstream.count == 1, "{r:?}");
            assert!(r.protocol == "http/1.1" || r.protocol == "h2", "{r:?}");
        }
        let protocols: std::collections::HashSet<_> =
            fast.slowest.iter().map(|r| r.protocol).collect();
        assert_eq!(
            protocols.len(),
            2,
            "both protocols among the slowest: {protocols:?}"
        );
    });
}
