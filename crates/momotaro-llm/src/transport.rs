//! The provider-call seam.
//!
//! The client itself never opens a socket: every exchange goes through a
//! [`ProviderTransport`]. Tests inject `fake::FakeLlm`; production uses this
//! crate's own [`crate::http`] implementation. The zero-network guarantee is
//! about the **test suite** — no test in this workspace builds or sends a
//! request, so CI never touches the network — and not about the crate being
//! incapable of one.
//!
//! Two boundary rules for production implementations:
//!
//! * **Send exactly the bytes you were given.** [`crate::LlmRequest::request_hash`]
//!   covers precisely the body passed to [`ProviderTransport::send`]; a
//!   transport that rewrites the body (say, mapping a thinking tier onto
//!   provider-specific parameters) breaks "hash mirrors the wire" and must
//!   key any cache on what it actually sent. Parameter mappings that matter
//!   to identity belong in the request body before hashing, not in the
//!   transport.
//! * **The transport is a trusted component.** It sees the request body and
//!   holds the credentials; the zero-network guarantee covers this crate's
//!   tests, not the transport. [`TransportError::message`] must never
//!   carry credentials, headers, or URLs with secrets — the client's sanitize
//!   pass strips control characters, not secrets.

use std::time::Duration;

/// One complete provider reply: an HTTP status plus the raw body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportReply {
    /// HTTP status code the provider answered with.
    pub status: u16,
    /// Raw response body. Size limits are the transport's responsibility —
    /// the client decodes whatever it is handed.
    pub body: String,
    /// The provider's `Retry-After` delay, when it sent one and it was
    /// decodable. This belongs to the reply, not [`crate::LlmError`], because
    /// only a permitted retry can act on it.
    pub retry_after: Option<Duration>,
}

/// A transport-level failure: no complete reply came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// Whether provider output was already observable when the failure hit
    /// (for example a body stream that died mid-way). Once output exists the
    /// client must never retry — `docs/docs/architecture.md` §16: retry only
    /// before observable output.
    pub output_observed: bool,
    /// What failed, kept for diagnosis — the module's trusted-component rule
    /// above governs its content.
    pub message: String,
}

/// The one path a request can take to a provider.
///
/// `send` receives the exact canonical request bytes (the same bytes
/// [`crate::LlmRequest::request_hash`] covers) and answers with either one
/// complete reply or a transport failure. See the module docs for the two
/// boundary rules a production implementation inherits with this trait.
pub trait ProviderTransport {
    /// Perform one provider exchange.
    fn send(&self, body: &str) -> Result<TransportReply, TransportError>;

    /// Perform one exchange carrying the request's idempotency key.
    ///
    /// The default drops the key so existing transports keep compiling; the
    /// production transport and the test rig override this. `complete` always
    /// calls it with [`crate::LlmRequest::request_hash`], and every retry
    /// passes the same value.
    fn send_with_idempotency_key(
        &self,
        idempotency_key: &str,
        body: &str,
    ) -> Result<TransportReply, TransportError> {
        let _ = idempotency_key;
        self.send(body)
    }
}
