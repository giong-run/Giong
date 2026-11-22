//! Descriptions of gRPC methods, generated for each service in a `.proto`.

/// How many messages each side of a call sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// One request, one response.
    Unary,
    /// A stream of requests, one response.
    ClientStreaming,
    /// One request, a stream of responses.
    ServerStreaming,
    /// Streams both ways.
    Bidi,
}

/// One method of a gRPC service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method {
    /// The HTTP/2 `:path` of the method: `/package.Service/Method`.
    pub path: &'static str,
    /// How many messages each side sends.
    pub kind: MethodKind,
    /// Full name of the request message.
    pub input: &'static str,
    /// Full name of the response message.
    pub output: &'static str,
}
