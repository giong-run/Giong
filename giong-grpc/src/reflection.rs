//! The gRPC server reflection protocol
//! (`grpc.reflection.v1.ServerReflection`): lets `grpcurl`, `grpcui`,
//! Postman and the like list a server's services and call them without the
//! `.proto` files.
//!
//! The code giong-grpc-build generates carries its `.proto` files as a
//! constant, `FILE_DESCRIPTOR_SET`. A server hands those to [`Reflection`]
//! and adds its services:
//!
//! ```no_run
//! use giong_grpc::reflection::Reflection;
//! # mod pb { pub const FILE_DESCRIPTOR_SET: &[u8] = &[]; }
//!
//! # fn main() -> Result<(), giong_proto::Error> {
//! let reflection = Reflection::new(&[pb::FILE_DESCRIPTOR_SET])?;
//! let server = giong_grpc::Server::new()
//!     // .add(GreeterServer::new(greeter))
//!     .add(reflection.v1())
//!     .add(reflection.v1alpha());
//! # let _ = server;
//! # Ok(())
//! # }
//! ```
//!
//! Then `grpcurl -plaintext localhost:50051 list` shows them. Current
//! tools ask `v1` first; older ones only know `v1alpha`, which is the same
//! protocol under its earlier name. Add both unless every client is known.
//!
//! Reflection tells anyone who can reach the port what the server's API
//! is. Leave it off a server whose API is not public, or serve it on an
//! internal port.
//!
//! The descriptors do not carry the comments of the `.proto` files, so
//! `grpcurl describe` shows the declarations without them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use giong_proto::wire::{Reader, WireType};

use crate::server::{Call, CallFuture};
use crate::{Code, RequestStream, Service, Sink, Status};

#[allow(missing_docs, unused, unreachable_pub, clippy::all)]
mod pb {
    include!("generated/reflection.rs");
}

use pb::grpc::reflection::v1 as proto;
use proto::server_reflection_request::{MessageRequest, MessageRequestView};
use proto::server_reflection_response::MessageResponse;

/// The `.proto` files of the reflection services themselves (`v1` and
/// `v1alpha`), as an encoded `google.protobuf.FileDescriptorSet`.
/// [`Reflection`] already serves them.
pub const FILE_DESCRIPTOR_SET: &[u8] = pb::FILE_DESCRIPTOR_SET;

const V1: &str = "grpc.reflection.v1.ServerReflection";
const V1ALPHA: &str = "grpc.reflection.v1alpha.ServerReflection";

/// How deep messages may nest in a descriptor: more than protoc accepts.
const MAX_DEPTH: u32 = 64;

struct File {
    /// The encoded `FileDescriptorProto`.
    bytes: Vec<u8>,
    /// The names of the files it imports.
    imports: Vec<String>,
}

#[derive(Default)]
struct Index {
    files: Vec<File>,
    by_name: HashMap<String, usize>,
    /// Every fully-qualified name declared, to the file that declares it.
    symbols: HashMap<String, usize>,
    /// The services of the files given to `Reflection::new`.
    services: BTreeSet<String>,
    /// For each extended message, the extension numbers and their files.
    extensions: HashMap<String, BTreeMap<i32, usize>>,
}

impl Index {
    /// Adds the files of an encoded `FileDescriptorSet`. A file of a name
    /// already there is skipped: sets generated apart share their imports.
    fn add_set(&mut self, set: &[u8], listed: bool) -> Result<(), giong_proto::Error> {
        let mut reader = Reader::new(set);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WireType::Len) => self.add_file(reader.bytes()?, listed)?,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        Ok(())
    }

    fn add_file(&mut self, bytes: &[u8], listed: bool) -> Result<(), giong_proto::Error> {
        // First pass: the name and the package, which the names of
        // everything else start with.
        let (mut name, mut package) = ("", "");
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WireType::Len) => name = reader.str()?,
                (2, WireType::Len) => package = reader.str()?,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        if self.by_name.contains_key(name) {
            return Ok(());
        }
        let id = self.files.len();
        let mut imports = Vec::new();
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (3, WireType::Len) => imports.push(reader.str()?.to_owned()),
                (4, WireType::Len) => self.add_message(reader.bytes()?, package, id, MAX_DEPTH)?,
                (5, WireType::Len) => self.add_enum(reader.bytes()?, package, id)?,
                (6, WireType::Len) => self.add_service(reader.bytes()?, package, id, listed)?,
                (7, WireType::Len) => self.add_field(reader.bytes()?, package, id)?,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        self.by_name.insert(name.to_owned(), id);
        self.files.push(File {
            bytes: bytes.to_vec(),
            imports,
        });
        Ok(())
    }

    fn declare(&mut self, scope: &str, name: &str, file: usize) -> String {
        let full = if scope.is_empty() {
            name.to_owned()
        } else {
            format!("{scope}.{name}")
        };
        self.symbols.entry(full.clone()).or_insert(file);
        full
    }

    /// The `name` (field 1) of a descriptor of any kind.
    fn name_of(bytes: &[u8]) -> Result<&str, giong_proto::Error> {
        let mut name = "";
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WireType::Len) => name = reader.str()?,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        Ok(name)
    }

    fn add_message(
        &mut self,
        bytes: &[u8],
        scope: &str,
        file: usize,
        depth: u32,
    ) -> Result<(), giong_proto::Error> {
        let mut reader = Reader::new(bytes);
        if depth == 0 {
            return Err(reader.error(giong_proto::ErrorKind::TooDeep));
        }
        let full = self.declare(scope, Self::name_of(bytes)?, file);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                // Fields and oneofs: named inside the message.
                (2 | 8, WireType::Len) => {
                    let name = Self::name_of(reader.bytes()?)?;
                    self.declare(&full, name, file);
                }
                (3, WireType::Len) => self.add_message(reader.bytes()?, &full, file, depth - 1)?,
                (4, WireType::Len) => self.add_enum(reader.bytes()?, &full, file)?,
                (6, WireType::Len) => self.add_field(reader.bytes()?, &full, file)?,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        Ok(())
    }

    fn add_enum(
        &mut self,
        bytes: &[u8],
        scope: &str,
        file: usize,
    ) -> Result<(), giong_proto::Error> {
        self.declare(scope, Self::name_of(bytes)?, file);
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                // The values of an enum are named beside it, not inside.
                (2, WireType::Len) => {
                    let name = Self::name_of(reader.bytes()?)?;
                    self.declare(scope, name, file);
                }
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        Ok(())
    }

    fn add_service(
        &mut self,
        bytes: &[u8],
        scope: &str,
        file: usize,
        listed: bool,
    ) -> Result<(), giong_proto::Error> {
        let full = self.declare(scope, Self::name_of(bytes)?, file);
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (2, WireType::Len) => {
                    let name = Self::name_of(reader.bytes()?)?;
                    self.declare(&full, name, file);
                }
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        if listed {
            self.services.insert(full);
        }
        Ok(())
    }

    /// An extension: a field declared outside the message it extends.
    fn add_field(
        &mut self,
        bytes: &[u8],
        scope: &str,
        file: usize,
    ) -> Result<(), giong_proto::Error> {
        let (mut name, mut extendee, mut number) = ("", "", 0);
        let mut reader = Reader::new(bytes);
        while let Some((field, wire)) = reader.tag()? {
            match (field, wire) {
                (1, WireType::Len) => name = reader.str()?,
                (2, WireType::Len) => extendee = reader.str()?,
                // An int32: the low 32 bits of the varint.
                (3, WireType::Varint) => number = reader.varint()? as i32,
                _ => reader.skip(field, wire, MAX_DEPTH)?,
            }
        }
        self.declare(scope, name, file);
        // protoc writes the extended type fully qualified: ".pkg.Message".
        let extendee = extendee.strip_prefix('.').unwrap_or(extendee);
        self.extensions
            .entry(extendee.to_owned())
            .or_default()
            .entry(number)
            .or_insert(file);
        Ok(())
    }

    /// The file and the files it imports, directly or not: the file
    /// first, then those the client was not sent yet on this call.
    fn with_imports<'a>(&'a self, file: usize, sent: &mut HashSet<usize>) -> Vec<&'a [u8]> {
        let mut out = vec![self.files[file].bytes.as_slice()];
        sent.insert(file);
        let mut queue: Vec<usize> = vec![file];
        let mut seen: HashSet<usize> = HashSet::from([file]);
        while let Some(current) = queue.pop() {
            for import in &self.files[current].imports {
                // An import that was not in the set cannot be sent.
                let Some(&id) = self.by_name.get(import) else {
                    continue;
                };
                if seen.insert(id) {
                    queue.push(id);
                    if sent.insert(id) {
                        out.push(&self.files[id].bytes);
                    }
                }
            }
        }
        out
    }
}

/// What a server's reflection services answer from: the `.proto` files of
/// its services. See the [module](self) for how to use it.
///
/// `Send` and `Sync`, and cheap to clone: the descriptors are indexed once
/// and shared, so one `Reflection` made at startup serves the servers of
/// every worker thread.
#[derive(Clone)]
pub struct Reflection {
    index: Arc<Index>,
    /// Which of the two reflection services were made, to list them.
    v1: Arc<AtomicBool>,
    v1alpha: Arc<AtomicBool>,
}

impl Reflection {
    /// Indexes the files of `sets`: each an encoded
    /// `google.protobuf.FileDescriptorSet`, as in the
    /// `FILE_DESCRIPTOR_SET` constant of generated code (and
    /// [`health::FILE_DESCRIPTOR_SET`](crate::health::FILE_DESCRIPTOR_SET)
    /// for the health service). Every service in them is listed; a file
    /// that several sets carry (a shared import) counts once.
    ///
    /// # Errors
    ///
    /// The giong-proto error (`GIONG-E40xx`) for a set that is not an
    /// encoded `FileDescriptorSet`.
    pub fn new(sets: &[&[u8]]) -> Result<Self, giong_proto::Error> {
        let mut index = Index::default();
        for set in sets {
            index.add_set(set, true)?;
        }
        // Described, and listed once added to a server.
        index.add_set(FILE_DESCRIPTOR_SET, false)?;
        Ok(Self {
            index: Arc::new(index),
            v1: Arc::default(),
            v1alpha: Arc::default(),
        })
    }

    /// The `grpc.reflection.v1.ServerReflection` service, to add to a
    /// [`Server`](crate::Server).
    pub fn v1(&self) -> impl Service {
        self.v1.store(true, Ordering::Relaxed);
        Reflector {
            reflection: self.clone(),
            name: V1,
        }
    }

    /// The `grpc.reflection.v1alpha.ServerReflection` service: the same
    /// protocol under the name older tools ask for.
    pub fn v1alpha(&self) -> impl Service {
        self.v1alpha.store(true, Ordering::Relaxed);
        Reflector {
            reflection: self.clone(),
            name: V1ALPHA,
        }
    }

    /// The full names of the services listed to clients, sorted.
    pub fn service_names(&self) -> Vec<String> {
        let mut names = self.index.services.clone();
        if self.v1.load(Ordering::Relaxed) {
            names.insert(V1.to_owned());
        }
        if self.v1alpha.load(Ordering::Relaxed) {
            names.insert(V1ALPHA.to_owned());
        }
        names.into_iter().collect()
    }
}

impl fmt::Debug for Reflection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reflection")
            .field("services", &self.service_names())
            .field("files", &self.index.files.len())
            .finish()
    }
}

struct Reflector {
    reflection: Reflection,
    name: &'static str,
}

impl Service for Reflector {
    fn name(&self) -> &'static str {
        self.name
    }

    fn call(&self, method: &str, call: Call) -> CallFuture {
        if method != "ServerReflectionInfo" {
            return call.unimplemented();
        }
        let reflection = self.reflection.clone();
        // v1 and v1alpha have the same messages, field for field: the v1
        // types read and write both.
        call.bidi::<proto::ServerReflectionRequest<'static>, proto::ServerReflectionResponse<'static>, _, _>(
            move |requests, responses| answer(reflection, requests, responses),
        )
    }
}

/// One response for each request, in order, until the client ends its
/// requests.
async fn answer(
    reflection: Reflection,
    mut requests: RequestStream<proto::ServerReflectionRequest<'static>>,
    mut responses: Sink<proto::ServerReflectionResponse<'static>>,
) -> Result<(), Status> {
    let index = &*reflection.index;
    // The files this client already has (see `with_imports`).
    let mut sent = HashSet::new();
    while let Some(received) = requests.message().await? {
        let request = received.message()?;
        let Some(asked) = request.message_request() else {
            return Err(Status::new(
                Code::InvalidArgument,
                "the reflection request asks for nothing this server knows: none of the five \
                 kinds of request is set",
            ));
        };
        // What the response points to.
        let (files, message);
        let numbers: Vec<i32>;
        let names;
        let services: Vec<proto::ServiceResponse<'_>>;
        let (found, extensions, list, error);
        let extension_request;
        let mut file_of = |file: Option<usize>, what: String| match file {
            Some(file) => Ok(index.with_imports(file, &mut sent)),
            None => Err(what),
        };
        let (echo, outcome) = match asked {
            MessageRequestView::FileByFilename(name) => (
                MessageRequest::FileByFilename(name),
                file_of(
                    index.by_name.get(*name).copied(),
                    format!("no file named {name:?} is known to this server"),
                ),
            ),
            MessageRequestView::FileContainingSymbol(symbol) => (
                MessageRequest::FileContainingSymbol(symbol),
                file_of(
                    index.symbols.get(*symbol).copied(),
                    format!("no service, method, message, enum or field named {symbol:?} is known to this server"),
                ),
            ),
            MessageRequestView::FileContainingExtension(extension) => {
                extension_request = proto::ExtensionRequest {
                    containing_type: extension.containing_type(),
                    extension_number: extension.extension_number(),
                    ..Default::default()
                };
                (
                    MessageRequest::FileContainingExtension(&extension_request),
                    file_of(
                        index
                            .extensions
                            .get(extension.containing_type())
                            .and_then(|numbers| numbers.get(&extension.extension_number()))
                            .copied(),
                        format!(
                            "no extension {} of {:?} is known to this server",
                            extension.extension_number(),
                            extension.containing_type()
                        ),
                    ),
                )
            }
            MessageRequestView::AllExtensionNumbersOfType(name) => {
                let echo = MessageRequest::AllExtensionNumbersOfType(name);
                match (index.extensions.get(*name), index.symbols.contains_key(*name)) {
                    (None, false) => (
                        echo,
                        Err(format!("no message named {name:?} is known to this server")),
                    ),
                    (known, _) => {
                        numbers = known
                            .map(|numbers| numbers.keys().copied().collect())
                            .unwrap_or_default();
                        extensions = proto::ExtensionNumberResponse {
                            base_type_name: name,
                            extension_number: &numbers,
                            ..Default::default()
                        };
                        let response = MessageResponse::AllExtensionNumbersResponse(&extensions);
                        send(&mut responses, &request, echo, response).await?;
                        continue;
                    }
                }
            }
            MessageRequestView::ListServices(content) => {
                names = reflection.service_names();
                services = names
                    .iter()
                    .map(|name| proto::ServiceResponse {
                        name,
                        ..Default::default()
                    })
                    .collect();
                list = proto::ListServiceResponse {
                    service: &services,
                    ..Default::default()
                };
                let echo = MessageRequest::ListServices(content);
                let response = MessageResponse::ListServicesResponse(&list);
                send(&mut responses, &request, echo, response).await?;
                continue;
            }
            MessageRequestView::__Phantom(_) => continue,
        };
        let response = match outcome {
            Ok(list) => {
                files = list;
                found = proto::FileDescriptorResponse {
                    file_descriptor_proto: &files,
                    ..Default::default()
                };
                MessageResponse::FileDescriptorResponse(&found)
            }
            Err(what) => {
                message = what;
                error = proto::ErrorResponse {
                    error_code: Code::NotFound.number() as i32,
                    error_message: &message,
                    ..Default::default()
                };
                MessageResponse::ErrorResponse(&error)
            }
        };
        send(&mut responses, &request, echo, response).await?;
    }
    Ok(())
}

async fn send(
    responses: &mut Sink<proto::ServerReflectionResponse<'static>>,
    request: &proto::ServerReflectionRequestView<'_>,
    echo: MessageRequest<'_>,
    response: MessageResponse<'_>,
) -> Result<(), Status> {
    let original = proto::ServerReflectionRequest {
        host: request.host(),
        message_request: Some(echo),
        ..Default::default()
    };
    responses
        .send(&proto::ServerReflectionResponse {
            valid_host: request.host(),
            original_request: Some(&original),
            message_response: Some(response),
            ..Default::default()
        })
        .await
}

#[cfg(test)]
mod tests {
    use giong_proto::wire::{put_bytes, put_tag, put_varint};

    use super::*;
    use crate::{CallOptions, Channel, RequestSink, ResponseStream};

    type Requests = RequestSink<proto::ServerReflectionRequest<'static>>;
    type Responses = ResponseStream<proto::ServerReflectionResponse<'static>>;

    /// A proto2 file with a message that has extension ranges, an
    /// extension of it at the top level and one declared inside a message.
    fn extended() -> Vec<u8> {
        let field = |name: &str, number: u64, extendee: Option<&str>| {
            let mut f = Vec::new();
            put_bytes(&mut f, 1, name.as_bytes());
            if let Some(extendee) = extendee {
                put_bytes(&mut f, 2, extendee.as_bytes());
            }
            put_tag(&mut f, 3, WireType::Varint);
            put_varint(&mut f, number);
            f
        };
        let mut base = Vec::new();
        put_bytes(&mut base, 1, b"Base");
        put_bytes(&mut base, 2, &field("id", 1, None));
        let mut holder = Vec::new();
        put_bytes(&mut holder, 1, b"Holder");
        put_bytes(&mut holder, 6, &field("inner", 200, Some(".ext.Base")));
        let mut colour = Vec::new();
        put_bytes(&mut colour, 1, b"Colour");
        let mut red = Vec::new();
        put_bytes(&mut red, 1, b"RED");
        put_bytes(&mut colour, 2, &red);
        put_bytes(&mut holder, 4, &colour);
        let mut file = Vec::new();
        put_bytes(&mut file, 1, b"ext.proto");
        put_bytes(&mut file, 2, b"ext");
        put_bytes(&mut file, 3, b"missing.proto");
        put_bytes(&mut file, 4, &base);
        put_bytes(&mut file, 4, &holder);
        put_bytes(&mut file, 7, &field("outer", 100, Some(".ext.Base")));
        let mut set = Vec::new();
        put_bytes(&mut set, 1, &file);
        set
    }

    fn serve(reflection: &Reflection) -> Channel {
        let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = crate::Server::new()
            .add(reflection.v1())
            .add(reflection.v1alpha());
        giong::spawn(async move { giong_http::Server::new().serve(listener, server).await });
        Channel::new(&format!("http://{addr}")).unwrap()
    }

    /// Sends one request and gives what the answer holds, as text.
    async fn ask(
        requests: &mut Requests,
        responses: &mut Responses,
        request: MessageRequest<'_>,
    ) -> String {
        use proto::server_reflection_response::MessageResponseView as R;

        let request = proto::ServerReflectionRequest {
            host: "here",
            message_request: Some(request),
            ..Default::default()
        };
        requests.send(&request).await.unwrap();
        let received = responses.message().await.unwrap().unwrap();
        let response = received.message().unwrap();
        assert_eq!(response.valid_host(), "here");
        let original = response.original_request().unwrap();
        assert_eq!(original.host(), "here");
        assert!(original.message_request().is_some());
        match response.message_response().unwrap() {
            R::FileDescriptorResponse(files) => files
                .file_descriptor_proto()
                .iter()
                .map(|file| Index::name_of(file).unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(" "),
            R::AllExtensionNumbersResponse(numbers) => format!(
                "{} {:?}",
                numbers.base_type_name(),
                numbers.extension_number().iter().collect::<Vec<_>>()
            ),
            R::ListServicesResponse(list) => list
                .service()
                .iter()
                .map(|service| service.name().to_owned())
                .collect::<Vec<_>>()
                .join(" "),
            R::ErrorResponse(error) => {
                format!("error {} {}", error.error_code(), error.error_message())
            }
            R::__Phantom(_) => unreachable!(),
        }
    }

    #[test]
    fn answers_each_kind_of_request() {
        giong::block_on(async {
            let extended = extended();
            let reflection =
                Reflection::new(&[crate::health::FILE_DESCRIPTOR_SET, &extended]).unwrap();
            let channel = serve(&reflection);
            for service in [V1, V1ALPHA] {
                let path = format!("/{service}/ServerReflectionInfo");
                let mut owned: (Requests, Responses) = channel.bidi(&path, CallOptions::new());
                let (tx, rx) = (&mut owned.0, &mut owned.1);
                assert_eq!(
                    ask(tx, rx, MessageRequest::ListServices("*")).await,
                    "grpc.health.v1.Health grpc.reflection.v1.ServerReflection \
                     grpc.reflection.v1alpha.ServerReflection"
                );
                let health = "grpc/health/v1/health.proto";
                for symbol in [
                    "grpc.health.v1.Health",
                    "grpc.health.v1.Health.Watch",
                    "grpc.health.v1.HealthCheckResponse",
                    "grpc.health.v1.HealthCheckResponse.status",
                    "grpc.health.v1.HealthCheckResponse.ServingStatus",
                    "grpc.health.v1.HealthCheckResponse.SERVING",
                ] {
                    let request = MessageRequest::FileContainingSymbol(symbol);
                    assert_eq!(ask(tx, rx, request).await, health, "{symbol}");
                }
                assert_eq!(
                    ask(tx, rx, MessageRequest::FileByFilename(health)).await,
                    health
                );
                assert_eq!(
                    ask(tx, rx, MessageRequest::FileContainingSymbol(V1ALPHA)).await,
                    "grpc/reflection/v1alpha/reflection.proto"
                );
                for symbol in [
                    "ext.Holder.Colour",
                    "ext.Holder.RED",
                    "ext.outer",
                    "ext.Holder.inner",
                ] {
                    let request = MessageRequest::FileContainingSymbol(symbol);
                    assert_eq!(ask(tx, rx, request).await, "ext.proto", "{symbol}");
                }
                for (number, found) in [(100, true), (200, true), (300, false)] {
                    let extension = proto::ExtensionRequest {
                        containing_type: "ext.Base",
                        extension_number: number,
                        ..Default::default()
                    };
                    let answer =
                        ask(tx, rx, MessageRequest::FileContainingExtension(&extension)).await;
                    assert_eq!(answer == "ext.proto", found, "{answer}");
                    assert_eq!(answer.starts_with("error 5 "), !found, "{answer}");
                }
                assert_eq!(
                    ask(
                        tx,
                        rx,
                        MessageRequest::AllExtensionNumbersOfType("ext.Base")
                    )
                    .await,
                    "ext.Base [100, 200]"
                );
                assert_eq!(
                    ask(
                        tx,
                        rx,
                        MessageRequest::AllExtensionNumbersOfType("ext.Holder")
                    )
                    .await,
                    "ext.Holder []"
                );
                for request in [
                    MessageRequest::AllExtensionNumbersOfType("ext.Nope"),
                    MessageRequest::FileContainingSymbol("grpc.health.v1.Nope"),
                    MessageRequest::FileContainingSymbol(".grpc.health.v1.Health"),
                    MessageRequest::FileByFilename("nope.proto"),
                ] {
                    let answer = ask(tx, rx, request.clone()).await;
                    assert!(answer.starts_with("error 5 no "), "{request:?}: {answer}");
                }
                let _ = (tx, rx);
                owned.0.finish();
                assert!(owned.1.message().await.unwrap().is_none());
            }
        });
    }

    #[test]
    fn imports_are_sent_once_for_each_call() {
        let mut index = Index::default();
        let file = |name: &str, imports: &[&str]| {
            let mut file = Vec::new();
            put_bytes(&mut file, 1, name.as_bytes());
            for import in imports {
                put_bytes(&mut file, 3, import.as_bytes());
            }
            let mut set = Vec::new();
            put_bytes(&mut set, 1, &file);
            set
        };
        // a imports b and c, b imports c, c imports a file not given.
        for (name, imports) in [
            ("c", &["gone"][..]),
            ("b", &["c"]),
            ("a", &["b", "c"]),
            ("a", &["twice"]),
        ] {
            index.add_set(&file(name, imports), true).unwrap();
        }
        let names = |index: &Index, file: &str, sent: &mut HashSet<usize>| {
            index
                .with_imports(index.by_name[file], sent)
                .iter()
                .map(|bytes| Index::name_of(bytes).unwrap().to_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut sent = HashSet::new();
        assert_eq!(names(&index, "b", &mut sent), "b c");
        assert_eq!(names(&index, "a", &mut sent), "a");
        assert_eq!(names(&index, "c", &mut sent), "c");
        let mut sent = HashSet::new();
        let all = names(&index, "a", &mut sent);
        assert!(all == "a c b" || all == "a b c", "{all}");
    }

    #[test]
    fn a_request_for_nothing_is_invalid() {
        giong::block_on(async {
            let reflection = Reflection::new(&[]).unwrap();
            assert_eq!(reflection.service_names(), Vec::<String>::new());
            let channel = serve(&reflection);
            assert_eq!(reflection.service_names(), [V1, V1ALPHA]);
            let path = format!("/{V1}/ServerReflectionInfo");
            let (mut tx, mut rx): (Requests, Responses) = channel.bidi(&path, CallOptions::new());
            tx.send(&proto::ServerReflectionRequest::default())
                .await
                .unwrap();
            let status = rx.message().await.unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument);
            let status = channel
                .unary::<proto::ExtensionRequest<'static>, proto::ExtensionRequest<'static>>(
                    &format!("/{V1}/Nope"),
                    &proto::ExtensionRequest::default(),
                    CallOptions::new(),
                )
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::Unimplemented);
        });
    }

    #[test]
    fn a_set_that_is_not_one_is_refused() {
        for bad in [&[0x0a, 0x05, 1][..], &[0x0a, 0x02, 0x0a, 0x09], &[0xff; 12]] {
            assert!(Reflection::new(&[bad]).is_err(), "{bad:?}");
        }
        // Wrong wire types where names are expected are skipped, not read
        // as text.
        let reflection = Reflection::new(&[&[0x08, 0x01], &[0x0a, 0x02, 0x08, 0x07]]).unwrap();
        assert_eq!(reflection.index.files.len(), 3);
    }
}
