//! The server's TLS session, with the one check of RFC 9001 that rustls
//! leaves open: see [`crate::native::hello`].

use std::any::Any;
use std::sync::Arc;

use quinn_proto::crypto::{
    self, ExportKeyingMaterialError, HeaderKey, KeyPair, Keys, PacketKey, Session,
    UnsupportedVersion,
};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectionId, Side, TransportError, TransportErrorCode};

use crate::native::hello::{self, Params};

/// QUIC version 1 (RFC 9000). The drafts quinn-proto also speaks carry
/// their parameters under the draft codepoint, rightly.
const VERSION_1: u32 = 1;

/// The `missing_extension` alert (RFC 8446, section 6.2).
const MISSING_EXTENSION: u8 = 109;

/// A server's TLS configuration whose sessions refuse a version 1
/// ClientHello that has its transport parameters only under the codepoint
/// of the drafts.
pub(crate) struct ServerCrypto<C>(pub(crate) Arc<C>);

impl<C: crypto::ServerConfig + 'static> crypto::ServerConfig for ServerCrypto<C> {
    fn initial_keys(
        &self,
        version: u32,
        dst_cid: &ConnectionId,
    ) -> Result<Keys, UnsupportedVersion> {
        self.0.initial_keys(version, dst_cid)
    }

    fn retry_tag(&self, version: u32, orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        self.0.retry_tag(version, orig_dst_cid, packet)
    }

    fn start_session(
        self: Arc<Self>,
        version: u32,
        params: &TransportParameters,
    ) -> Box<dyn Session> {
        let inner = self.0.clone().start_session(version, params);
        if version != VERSION_1 {
            return inner;
        }
        Box::new(Checked {
            inner,
            checking: true,
            held: Vec::new(),
        })
    }
}

struct Checked {
    inner: Box<dyn Session>,
    /// The ClientHello has not been seen whole yet.
    checking: bool,
    /// Its bytes so far, when it came in more than one piece.
    held: Vec<u8>,
}

impl Checked {
    fn check(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        let found = if self.held.is_empty() {
            hello::params(buf)
        } else {
            self.held.extend_from_slice(buf);
            hello::params(&self.held)
        };
        match found {
            Params::Incomplete => {
                if self.held.is_empty() {
                    self.held.extend_from_slice(buf);
                }
                // `params` judges a message at `MAX_HELLO` and four bytes
                // at the latest, so this is all that is ever held.
                debug_assert!(self.held.len() < hello::MAX_HELLO + 4);
                return Ok(());
            }
            Params::DraftOnly => {
                return Err(TransportError {
                    code: TransportErrorCode::crypto(MISSING_EXTENSION),
                    frame: None,
                    reason: "the ClientHello has no quic_transport_parameters extension, only \
                             the one of the QUIC drafts"
                        .into(),
                });
            }
            // rustls refuses these with the alert that fits each.
            Params::Final | Params::Neither | Params::Malformed => {}
        }
        self.checking = false;
        self.held = Vec::new();
        Ok(())
    }
}

impl Session for Checked {
    fn initial_keys(&self, dst_cid: &ConnectionId, side: Side) -> Keys {
        self.inner.initial_keys(dst_cid, side)
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        self.inner.handshake_data()
    }

    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        self.inner.peer_identity()
    }

    fn early_crypto(&self) -> Option<(Box<dyn HeaderKey>, Box<dyn PacketKey>)> {
        self.inner.early_crypto()
    }

    fn early_data_accepted(&self) -> Option<bool> {
        self.inner.early_data_accepted()
    }

    fn is_handshaking(&self) -> bool {
        self.inner.is_handshaking()
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        if self.checking {
            self.check(buf)?;
        }
        self.inner.read_handshake(buf)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        self.inner.transport_parameters()
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        self.inner.write_handshake(buf)
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Box<dyn PacketKey>>> {
        self.inner.next_1rtt_keys()
    }

    fn is_valid_retry(&self, orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        self.inner.is_valid_retry(orig_dst_cid, header, payload)
    }

    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        self.inner.export_keying_material(output, label, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn_proto::crypto::rustls::QuicServerConfig;
    use quinn_proto::crypto::ServerConfig as _;
    use rustls::quic::Version;

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn server() -> Arc<ServerCrypto<QuicServerConfig>> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into())
            .unwrap();
        tls.alpn_protocols = vec![b"t".to_vec()];
        Arc::new(ServerCrypto(Arc::new(
            QuicServerConfig::try_from(tls).unwrap(),
        )))
    }

    /// The ClientHello rustls writes for QUIC `version`: the final
    /// codepoint for `V1`, the draft one for `V1Draft`.
    fn client_hello(version: Version) -> Vec<u8> {
        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"t".to_vec()];
        // Transport parameters of a client: just initial_source_connection_id.
        let params = vec![0x0f, 0x00];
        let mut conn = rustls::quic::ClientConnection::new(
            Arc::new(tls),
            version,
            "localhost".try_into().unwrap(),
            params,
        )
        .unwrap();
        let mut out = Vec::new();
        conn.write_hs(&mut out);
        out
    }

    /// Transport parameters for the server to send: all at their defaults.
    fn params() -> TransportParameters {
        TransportParameters::read(Side::Server, &mut &[][..]).unwrap()
    }

    fn feed(hello: &[u8], pieces: usize) -> Result<bool, TransportError> {
        let mut session = server().start_session(VERSION_1, &params());
        let size = hello.len().div_ceil(pieces);
        let mut last = Ok(false);
        for piece in hello.chunks(size) {
            last = session.read_handshake(piece);
            if last.is_err() {
                break;
            }
        }
        last
    }

    #[test]
    fn a_hello_with_only_the_draft_extension_gets_missing_extension() {
        let draft = client_hello(Version::V1Draft);
        assert_eq!(hello::params(&draft), Params::DraftOnly);
        for pieces in [1, 2, 3, 50] {
            let error = feed(&draft, pieces).unwrap_err();
            assert_eq!(
                error.code,
                TransportErrorCode::crypto(MISSING_EXTENSION),
                "{pieces}: {error}"
            );
        }
    }

    #[test]
    fn a_version_1_hello_is_read_as_before() {
        let v1 = client_hello(Version::V1);
        assert_eq!(hello::params(&v1), Params::Final);
        for pieces in [1, 2, 3, 50] {
            feed(&v1, pieces).unwrap_or_else(|e| panic!("{pieces}: {e}"));
        }
    }

    #[test]
    fn a_draft_version_keeps_its_own_codepoint() {
        let draft = client_hello(Version::V1Draft);
        let mut session = server().start_session(0xff00_001d, &params());
        session.read_handshake(&draft).unwrap();
    }
}
