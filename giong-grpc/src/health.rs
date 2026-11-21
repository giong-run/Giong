//! The gRPC health checking protocol (`grpc.health.v1.Health`): what load
//! balancers, Kubernetes probes and `grpc_health_probe` ask to learn
//! whether a server takes calls.
//!
//! A server adds the service and says how each of its services is doing:
//!
//! ```no_run
//! use giong_grpc::health::Health;
//!
//! # fn main() {
//! let health = Health::new();
//! health.set("helloworld.Greeter", true);
//! let server = giong_grpc::Server::new().add(health.service());
//! // Later, from anywhere, when the database behind Greeter is gone:
//! health.set("helloworld.Greeter", false);
//! # let _ = server;
//! # }
//! ```
//!
//! [`Health`] is `Send` and `Sync`: the servers of every worker thread can
//! share one, so a change is seen whichever worker a probe reaches.
//!
//! A client asks with [`check`] or follows changes with [`watch`].

use std::collections::BTreeMap;
use std::fmt;
use std::future::poll_fn;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};

use crate::{CallOptions, Channel, Code, Reply, Request, ResponseStream, Service, Sink, Status};

#[allow(missing_docs, unused, unreachable_pub, clippy::all)]
mod pb {
    include!("generated/health.rs");
}

use pb::grpc::health::v1 as proto;
use proto::health::{HealthServer, CHECK, WATCH};
use proto::health_check_response::ServingStatus as Wire;

/// The `.proto` file of the health service, as an encoded
/// `google.protobuf.FileDescriptorSet`: give it to
/// [`Reflection::new`](crate::reflection::Reflection::new) for the service
/// to be listed and described by reflection.
pub const FILE_DESCRIPTOR_SET: &[u8] = pb::FILE_DESCRIPTOR_SET;

/// What a server answers about one of its services.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ServingStatus {
    /// The server did not say (the protocol's `UNKNOWN`), or said
    /// something newer than this version of the protocol.
    Unknown,
    /// The service takes calls.
    Serving,
    /// The service is there and does not take calls now.
    NotServing,
    /// The server has no service of that name. Only [`watch`] reports it;
    /// [`check`] fails with `NOT_FOUND` instead, as the protocol says.
    ServiceUnknown,
}

impl ServingStatus {
    fn from_wire(status: giong_proto::Enum<Wire>) -> Self {
        match status.known() {
            Some(Wire::Serving) => Self::Serving,
            Some(Wire::NotServing) => Self::NotServing,
            Some(Wire::ServiceUnknown) => Self::ServiceUnknown,
            Some(Wire::Unknown) | None => Self::Unknown,
        }
    }
}

#[derive(Default)]
struct State {
    /// Whether each known service takes calls.
    services: BTreeMap<String, bool>,
    /// Counts the changes.
    version: u64,
    /// The `Watch` calls waiting for the next change, by number: a call
    /// that ends takes its entry out.
    watchers: Vec<(u64, Waker)>,
    /// The number of the next waiting call.
    next_watcher: u64,
}

/// The health of a server's services, by name: set by the program, served
/// by [`service`](Self::service).
///
/// The name `""` stands for the server as a whole and is serving from the
/// start; every other service is unknown until [`set`](Self::set) names
/// it. Clones share the state.
#[derive(Clone)]
pub struct Health {
    state: Arc<Mutex<State>>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    /// Health with one entry: the server as a whole (`""`), serving.
    pub fn new() -> Self {
        let mut state = State::default();
        state.services.insert(String::new(), true);
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // The state is a map and a counter: a panic elsewhere while it was
        // locked leaves nothing half done.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Says whether `service` (its full name, `package.Service`, or `""`
    /// for the whole server) takes calls. `Watch` calls on that service
    /// are told when the answer changes.
    pub fn set(&self, service: &str, serving: bool) {
        self.change(|services| services.insert(service.to_owned(), serving) != Some(serving));
    }

    /// Forgets `service`: `Check` answers `NOT_FOUND` for it again and
    /// `Watch` reports `SERVICE_UNKNOWN`.
    pub fn remove(&self, service: &str) {
        self.change(|services| services.remove(service).is_some());
    }

    /// Marks every known service as not serving: for the start of a
    /// graceful shutdown, so that load balancers stop sending calls while
    /// the open ones finish.
    pub fn set_all_not_serving(&self) {
        self.change(|services| {
            let mut changed = false;
            for serving in services.values_mut() {
                changed |= std::mem::replace(serving, false);
            }
            changed
        });
    }

    /// Whether `service` takes calls; `None` for a service never
    /// [`set`](Self::set).
    pub fn get(&self, service: &str) -> Option<bool> {
        self.state().services.get(service).copied()
    }

    fn change(&self, change: impl FnOnce(&mut BTreeMap<String, bool>) -> bool) {
        let woken = {
            let mut state = self.state();
            if !change(&mut state.services) {
                return;
            }
            state.version += 1;
            std::mem::take(&mut state.watchers)
        };
        // Outside the lock: a waker may run code of its own.
        for (_, waker) in woken {
            waker.wake();
        }
    }

    /// The service to add to a [`Server`](crate::Server). Each server (one
    /// per worker thread) gets its own; all of them answer from this
    /// state.
    pub fn service(&self) -> impl Service {
        HealthServer::new(self.clone())
    }

    /// The status of `service` now and the change count it was read at.
    fn snapshot(&self, service: &str) -> (Option<bool>, u64) {
        let state = self.state();
        (state.services.get(service).copied(), state.version)
    }

    /// Waits until the state is no longer at change count `seen`.
    async fn changed(&self, seen: u64) {
        /// Takes the waker out when the wait ends without a change, as
        /// when the client cancels the call.
        struct Waiting<'a> {
            health: &'a Health,
            number: Option<u64>,
        }
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                if let Some(number) = self.number {
                    self.health.state().watchers.retain(|(n, _)| *n != number);
                }
            }
        }
        let mut waiting = Waiting {
            health: self,
            number: None,
        };
        poll_fn(|cx| {
            let mut state = self.state();
            if state.version != seen {
                return Poll::Ready(());
            }
            let number = match waiting.number {
                Some(number) => number,
                None => {
                    let number = state.next_watcher;
                    state.next_watcher += 1;
                    waiting.number = Some(number);
                    number
                }
            };
            match state.watchers.iter_mut().find(|(n, _)| *n == number) {
                Some((_, waker)) => waker.clone_from(cx.waker()),
                // New, or taken by a change that this poll does not see
                // yet.
                None => state.watchers.push((number, cx.waker().clone())),
            }
            Poll::Pending
        })
        .await;
    }
}

impl fmt::Debug for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.state().services.iter()).finish()
    }
}

fn wire(serving: Option<bool>) -> Wire {
    match serving {
        Some(true) => Wire::Serving,
        Some(false) => Wire::NotServing,
        None => Wire::ServiceUnknown,
    }
}

fn response(status: Wire) -> proto::HealthCheckResponse<'static> {
    proto::HealthCheckResponse {
        status: status.into(),
        ..Default::default()
    }
}

impl proto::health::Health for Health {
    async fn check(
        &self,
        request: Request<proto::HealthCheckRequest<'static>>,
    ) -> Result<Reply<proto::HealthCheckResponse<'static>>, Status> {
        let message = request.message()?;
        match self.get(message.service()) {
            Some(serving) => Reply::new(&response(wire(Some(serving)))),
            None => Err(Status::new(
                Code::NotFound,
                "the health of this service is not known to the server",
            )),
        }
    }

    async fn list(
        &self,
        _request: Request<proto::HealthListRequest<'static>>,
    ) -> Result<Reply<proto::HealthListResponse<'static>>, Status> {
        let services = self.state().services.clone();
        let statuses: Vec<_> = services
            .iter()
            .map(|(name, &serving)| (name.as_str(), response(wire(Some(serving)))))
            .collect();
        Reply::new(&proto::HealthListResponse {
            statuses: &statuses,
            ..Default::default()
        })
    }

    async fn watch(
        &self,
        request: Request<proto::HealthCheckRequest<'static>>,
        mut responses: Sink<proto::HealthCheckResponse<'static>>,
    ) -> Result<(), Status> {
        let service = request.message()?.service().to_owned();
        let mut sent = None;
        // Until the client ends the call, which drops this future, or the
        // call's deadline.
        loop {
            let (serving, version) = self.snapshot(&service);
            if sent != Some(serving) {
                responses.send(&response(wire(serving))).await?;
                sent = Some(serving);
            }
            self.changed(version).await;
        }
    }
}

/// Asks the server behind `channel` whether `service` (its full name, or
/// `""` for the server as a whole) takes calls.
///
/// Cancel safe: dropping the future cancels the call.
///
/// # Errors
///
/// `NOT_FOUND` if the server does not know the service, `UNIMPLEMENTED` if
/// it has no health service, and what any unary call can fail with.
pub async fn check(
    channel: &Channel,
    service: &str,
    options: CallOptions,
) -> Result<ServingStatus, Status> {
    let request = proto::HealthCheckRequest {
        service,
        ..Default::default()
    };
    let reply = channel
        .unary::<proto::HealthCheckRequest<'static>, proto::HealthCheckResponse<'static>>(
            CHECK.path, &request, options,
        )
        .await?;
    let status = reply.message()?.status();
    Ok(ServingStatus::from_wire(status))
}

/// Follows the health of `service` on the server behind `channel`: the
/// [`Watch`] returned gives its status now and each status after a change.
/// Give the call a timeout that fits how long it should watch: the
/// channel's default applies otherwise.
///
/// Cancel safe: dropping the future cancels the call.
///
/// # Errors
///
/// What any server-streaming call can fail with when it starts.
pub async fn watch(
    channel: &Channel,
    service: &str,
    options: CallOptions,
) -> Result<Watch, Status> {
    let request = proto::HealthCheckRequest {
        service,
        ..Default::default()
    };
    let responses = channel
        .server_streaming(WATCH.path, &request, options)
        .await?;
    Ok(Watch { responses })
}

/// The statuses of one service as they change. Made by [`watch`]; dropping
/// it ends the call.
#[derive(Debug)]
pub struct Watch {
    responses: ResponseStream<proto::HealthCheckResponse<'static>>,
}

impl Watch {
    /// The next status: the current one first, then one after each change.
    /// `None` if the server ended the call without an error.
    ///
    /// Cancel safe: a status is only taken when it is returned.
    ///
    /// # Errors
    ///
    /// The status the call ended with: `DEADLINE_EXCEEDED` when its
    /// timeout passed, `UNIMPLEMENTED` from a server without the health
    /// service, `UNAVAILABLE` when the connection broke.
    pub async fn next(&mut self) -> Result<Option<ServingStatus>, Status> {
        match self.responses.message().await? {
            Some(received) => Ok(Some(ServingStatus::from_wire(received.message()?.status()))),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn serve(health: &Health) -> Channel {
        let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server::new().add(health.service());
        giong::spawn(async move { giong_http::Server::new().serve(listener, server).await });
        Channel::new(&format!("http://{addr}")).unwrap()
    }

    #[test]
    fn check_answers_what_was_set() {
        giong::block_on(async {
            let health = Health::new();
            let channel = serve(&health);
            let ask = |service: &'static str| check(&channel, service, CallOptions::new());
            assert_eq!(ask("").await.unwrap(), ServingStatus::Serving);
            let unknown = ask("a.B").await.unwrap_err();
            assert_eq!(unknown.code(), Code::NotFound);
            health.set("a.B", true);
            assert_eq!(ask("a.B").await.unwrap(), ServingStatus::Serving);
            health.set("a.B", false);
            assert_eq!(ask("a.B").await.unwrap(), ServingStatus::NotServing);
            assert_eq!(health.get("a.B"), Some(false));
            health.remove("a.B");
            assert_eq!(ask("a.B").await.unwrap_err().code(), Code::NotFound);
            health.set_all_not_serving();
            assert_eq!(ask("").await.unwrap(), ServingStatus::NotServing);
        });
    }

    #[test]
    fn list_gives_every_service() {
        giong::block_on(async {
            let health = Health::new();
            health.set("a.B", false);
            let channel = serve(&health);
            let reply = proto::health::HealthClient::new(channel)
                .list(&proto::HealthListRequest::default(), CallOptions::new())
                .await
                .unwrap();
            let message = reply.message().unwrap();
            let statuses: Vec<_> = message
                .statuses()
                .iter()
                .map(|(name, response)| (name, response.status().known()))
                .collect();
            assert_eq!(
                statuses,
                [("", Some(Wire::Serving)), ("a.B", Some(Wire::NotServing))]
            );
        });
    }

    #[test]
    fn watch_follows_the_changes_from_any_thread() {
        giong::block_on(async {
            let health = Health::new();
            let channel = serve(&health);
            let options = CallOptions::new().timeout(Duration::from_secs(20));
            let mut watch = watch(&channel, "a.B", options).await.unwrap();
            assert_eq!(
                watch.next().await.unwrap(),
                Some(ServingStatus::ServiceUnknown)
            );
            // Setting what is already so is not a change.
            health.set("", true);
            health.set("other", true);
            let remote = health.clone();
            std::thread::spawn(move || remote.set("a.B", true))
                .join()
                .unwrap();
            assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::Serving));
            health.set("a.B", false);
            assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::NotServing));
            health.remove("a.B");
            assert_eq!(
                watch.next().await.unwrap(),
                Some(ServingStatus::ServiceUnknown)
            );
            // Ending the call frees the watcher on the server.
            drop(watch);
            giong::time::sleep(Duration::from_millis(50)).await;
            health.set("a.B", true);
            assert!(health.state().watchers.is_empty());
        });
    }

    #[test]
    fn cancelled_watches_leave_nothing_behind() {
        giong::block_on(async {
            let health = Health::new();
            let channel = serve(&health);
            // Calls that end while nothing changes, as clients that watch
            // for a while and go: none of them stays among the watchers.
            for _ in 0..3 {
                let mut watches = Vec::new();
                for _ in 0..20 {
                    let options = CallOptions::new().timeout(Duration::from_secs(20));
                    let mut watch = watch(&channel, "", options).await.unwrap();
                    assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::Serving));
                    watches.push(watch);
                }
                giong::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(health.state().watchers.len(), 20);
                drop(watches);
                giong::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(health.state().watchers.len(), 0);
            }
            // The ones still open hear the next change.
            let options = CallOptions::new().timeout(Duration::from_secs(20));
            let mut watch = watch(&channel, "", options).await.unwrap();
            assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::Serving));
            health.set("", false);
            assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::NotServing));
        });
    }

    #[test]
    fn a_watch_ends_at_its_deadline() {
        giong::block_on(async {
            let health = Health::new();
            let channel = serve(&health);
            let options = CallOptions::new().timeout(Duration::from_millis(100));
            let mut watch = watch(&channel, "", options).await.unwrap();
            assert_eq!(watch.next().await.unwrap(), Some(ServingStatus::Serving));
            let ended = watch.next().await.unwrap_err();
            assert_eq!(ended.code(), Code::DeadlineExceeded);
        });
    }
}
