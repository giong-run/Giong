//! Messages as calls carry them: received ones, read through views that
//! borrow from the bytes that arrived; and ones to send, encoded once.

use std::fmt;
use std::marker::PhantomData;

use giong_proto::{MessageOf, MessageType, View};

use crate::codec::{self, Payload};
use crate::metadata::Metadata;
use crate::status::Status;

/// One message of a streaming call, as it arrived.
///
/// The bytes stay in the buffer they were received in;
/// [`message`](Self::message) decodes a view that borrows from them.
/// Messages that arrived together share that buffer (a piece of the
/// stream, 16 KiB or so), which is freed when the last of them is dropped:
/// copy out of a message what is kept for long.
pub struct Received<M> {
    payload: Payload,
    marker: PhantomData<fn() -> M>,
}

impl<M: MessageType> Received<M> {
    pub(crate) fn new(payload: Payload) -> Self {
        Self {
            payload,
            marker: PhantomData,
        }
    }

    /// Decodes the message, checking all of it. The view borrows from this
    /// value: strings, bytes and repeated fields are not copied.
    ///
    /// # Errors
    ///
    /// `INTERNAL` with `GIONG-E4105` if the bytes are not a valid message
    /// of this type; the message carries giong-proto's own code.
    pub fn message(&self) -> Result<M::View<'_>, Status> {
        Ok(M::View::decode(self.payload.as_slice())?)
    }

    /// The message's bytes, as protobuf encoded them.
    pub fn as_bytes(&self) -> &[u8] {
        self.payload.as_slice()
    }

    /// Whether the message arrived compressed.
    pub fn was_compressed(&self) -> bool {
        self.payload.was_compressed()
    }
}

impl<M> Received<M> {
    pub(crate) fn into_payload(self) -> Payload {
        self.payload
    }
}

impl<M> fmt::Debug for Received<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Received")
            .field("bytes", &self.payload.as_slice().len())
            .finish()
    }
}

/// One message to send, encoded when it was given, with the metadata that
/// goes with it: what a unary handler returns, and what a unary call on a
/// client gives back.
///
/// On the server, [`Reply::new`] takes the message to answer with:
///
/// ```ignore
/// async fn get_order(&self, request: Request<GetOrderRequest<'static>>)
///     -> Result<Reply<Order<'static>>, Status>
/// {
///     let id = request.message()?.id();
///     Reply::new(&Order { id, customer: "An", ..Default::default() })
/// }
/// ```
///
/// On the client, [`message`](Self::message) reads the answer as a view
/// that borrows from the bytes received.
pub struct Reply<M> {
    /// The message's bytes, in a frame without compression.
    frame: Payload,
    metadata: Metadata,
    trailers: Metadata,
    /// `None`: as the server or the channel is set.
    compress: Option<bool>,
    marker: PhantomData<fn() -> M>,
}

impl<M: MessageType> Reply<M> {
    /// A reply holding `message`, which is encoded now: what it borrows is
    /// free once this returns.
    ///
    /// # Errors
    ///
    /// `RESOURCE_EXHAUSTED` with `GIONG-E4102` if the message is larger
    /// than 4 GiB less one byte, the most a gRPC frame can hold. The
    /// server's or channel's own limit is checked when the reply is sent.
    pub fn new(message: &impl MessageOf<Type = M>) -> Result<Self, Status> {
        let frame = codec::encode_frame(message, false, u32::MAX as usize)?;
        Ok(Self::framed(frame))
    }

    fn framed(frame: Vec<u8>) -> Self {
        Self {
            frame: Payload::framed(frame),
            metadata: Metadata::new(),
            trailers: Metadata::new(),
            compress: None,
            marker: PhantomData,
        }
    }

    pub(crate) fn received(payload: Payload, metadata: Metadata, trailers: Metadata) -> Self {
        Self {
            frame: payload,
            metadata,
            trailers,
            compress: None,
            marker: PhantomData,
        }
    }

    /// Sets the metadata sent before the message (the response's headers).
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Sets the metadata sent after the message, with the status (the
    /// response's trailers).
    pub fn with_trailers(mut self, trailers: Metadata) -> Self {
        self.trailers = trailers;
        self
    }

    /// Whether the message is compressed with gzip when it is sent,
    /// instead of what the server is set to. A client that does not accept
    /// gzip gets it plain either way.
    pub fn compress(mut self, on: bool) -> Self {
        self.compress = Some(on);
        self
    }

    /// Decodes the message, checking all of it. The view borrows from this
    /// reply.
    ///
    /// # Errors
    ///
    /// `INTERNAL` with `GIONG-E4105` if the bytes are not a valid message
    /// of this type.
    pub fn message(&self) -> Result<M::View<'_>, Status> {
        Ok(M::View::decode(self.frame.as_slice())?)
    }

    /// The message's bytes, as protobuf encoded them.
    pub fn as_bytes(&self) -> &[u8] {
        self.frame.as_slice()
    }

    /// Whether the message arrived compressed: `true` only for a reply a
    /// client received, from a server that compressed it.
    pub fn was_compressed(&self) -> bool {
        self.frame.was_compressed()
    }

    /// The metadata that came (or goes) before the message.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The metadata that came (or goes) after the message.
    pub fn trailers(&self) -> &Metadata {
        &self.trailers
    }

    pub(crate) fn into_parts(self) -> (Payload, Metadata, Metadata, Option<bool>) {
        (self.frame, self.metadata, self.trailers, self.compress)
    }
}

impl<M> fmt::Debug for Reply<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("bytes", &self.frame.as_slice().len())
            .field("metadata", &self.metadata)
            .field("trailers", &self.trailers)
            .finish()
    }
}
