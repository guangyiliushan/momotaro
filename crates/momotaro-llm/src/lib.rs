//! Stateless OpenAI-compatible LLM client.
//!
//! The crate answers exactly one question — given `model / messages / tools /
//! max_tokens`, what did the provider return? — and reports `text / usage /
//! finish_reason / raw_ref` or a typed failure. It holds no session state, no
//! vault, no SQLite, and no provider registry.
//!
//! Three invariants the tests pin down:
//!
//! * **The hash mirrors the wire.** [`LlmRequest::request_hash`] covers the
//!   canonical serialization — the exact bytes [`complete`] sends — so tool
//!   order participates in the hash for the same reason it participates in the
//!   provider's prompt cache: the provider's cache key *is* the serialization
//!   order of the tools JSON.
//! * **Retry is bounded by output.** At most one retry, and only while nothing
//!   is observable yet: a failure that lands after a 2xx body exists is
//!   reported as-is, never replayed.
//! * **Usage is recorded, not invented.** Token figures the provider omits
//!   stay `None` (= NULL in `llm_turns`); a reported zero stays `Some(0)`.
//!
//! Zero-network guarantee: no test in this workspace opens a socket. Every
//! exchange in tests goes through the injected [`ProviderTransport`] seam with
//! the `fake::FakeLlm` rig (behind the `test-util` feature for downstream
//! crates; built-in for this crate's own tests). [`http`] is the one production
//! implementation of that seam; it is built and configured by tests, never
//! called, so CI cannot reach the network.
//!
//! # Examples
//!
//! Build one request, then read the canonical body a transport sends and the
//! hash that identifies it:
//!
//! ```
//! use momotaro_llm::{LlmRequest, Message, Role};
//!
//! let request = LlmRequest {
//!     model: "gpt-4o-mini".to_string(),
//!     messages: vec![Message {
//!         role: Role::User,
//!         content: "hello".to_string(),
//!     }],
//!     tools: vec![],
//!     max_tokens: 256,
//! };
//!
//! // Object keys come out lexicographic at every level; arrays keep the
//! // caller's order.
//! assert_eq!(
//!     request.canonical_json(),
//!     concat!(
//!         r#"{"max_tokens":256,"messages":[{"content":"hello","role":"user"}],"#,
//!         r#""model":"gpt-4o-mini","tools":[]}"#
//!     )
//! );
//! assert_eq!(
//!     request.request_hash(),
//!     "b990e32c882ad832ae7264023fb7febcfcb9ac6df49dc2abe49f84404ce9ebe7"
//! );
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![warn(rustdoc::unescaped_backticks)]

pub mod error;
#[cfg(any(test, feature = "test-util"))]
pub mod fake;
pub mod http;
pub mod request;
pub mod response;
pub mod transport;

pub use error::{LlmError, LlmErrorKind, is_unprintable, sanitize};
pub use http::{HttpTransport, TransportConfigError};
pub use request::{LlmRequest, Message, Role, ToolDef};
pub use response::{FinishReason, LlmOutput, Usage};
pub use transport::{ProviderTransport, TransportError, TransportReply};

use response::parse_response;

/// Send one request and decode one completion.
///
/// Retry policy (retry only before observable output): at most one
/// retry per call, and only for a failure that happened while no provider
/// output was observable yet. Output becomes observable the moment a 2xx body
/// exists — even one we cannot read — so a failure from that point on is
/// returned as-is, never replayed. The retry resends the identical bytes.
///
/// Both outcomes record how many sends they took ([`LlmOutput::attempts`] /
/// [`LlmError::attempts`]). When a permitted retry also failed, the reported
/// failure is the last attempt's and `attempts` is what records the retry.
/// One call is one `llm_turns` row: a silent retry is not a second turn.
///
/// # Errors
///
/// The last attempt's [`LlmError`], its [`LlmErrorKind`] classified from
/// the provider reply — or from the [`TransportError`] when no reply
/// arrived at all:
///
/// - `LlmErrorKind::Auth` when the provider answered 401/403.
/// - `LlmErrorKind::RateLimit` when the provider answered 429.
/// - `LlmErrorKind::ContextOverflow` when the reply is a 413, or any 4xx
///   carrying the `context_length_exceeded` code.
/// - `LlmErrorKind::Rejected` when the reply is any other 4xx, a 501, or a
///   status outside the wire contract.
/// - `LlmErrorKind::Provider` when the reply is a 408 or most 5xx, or when
///   the transport failed before any reply existed.
/// - `LlmErrorKind::Malformed` when a 2xx body is not a readable
///   completion.
///
/// # Examples
///
/// Any [`ProviderTransport`] can be handed in, so the exchange is scriptable:
///
/// ```
/// use momotaro_llm::{
///     complete, FinishReason, LlmRequest, Message, ProviderTransport, Role, TransportError,
///     TransportReply,
/// };
///
/// /// The seam's simplest implementation: one canned reply, no socket.
/// struct Stub;
///
/// impl ProviderTransport for Stub {
///     fn send(&self, _body: &str) -> Result<TransportReply, TransportError> {
///         Ok(TransportReply {
///             status: 200,
///             body: serde_json::json!({
///                 "id": "chatcmpl-1",
///                 "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
///                 "usage": {"prompt_tokens": 3, "completion_tokens": 5},
///             })
///             .to_string(),
///             retry_after: None,
///         })
///     }
/// }
///
/// let request = LlmRequest {
///     model: "test-model".to_string(),
///     messages: vec![Message {
///         role: Role::User,
///         content: "hello".to_string(),
///     }],
///     tools: vec![],
///     max_tokens: 64,
/// };
///
/// let output = complete(&Stub, &request).expect("the stub answers");
/// assert_eq!(output.text, "hi");
/// assert_eq!(output.finish_reason, FinishReason::Stop);
/// assert_eq!(output.usage.input_tokens, Some(3));
/// assert_eq!(output.usage.output_tokens, Some(5));
/// assert_eq!(output.attempts, 1, "one send, so no retry was needed");
/// ```
pub fn complete<T: ProviderTransport + ?Sized>(
    transport: &T,
    request: &LlmRequest,
) -> Result<LlmOutput, LlmError> {
    let body = request.canonical_json();
    // One identity for every attempt: the hash already covers exactly the
    // canonical bytes below, so a provider can tell a retry from a new ask.
    let idempotency_key = request.request_hash();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut retry_after = None;
        let mut error = match transport.send_with_idempotency_key(&idempotency_key, &body) {
            Ok(reply) if (200..300).contains(&reply.status) => {
                return parse_response(&reply.body)
                    .map(|mut output| {
                        output.attempts = attempt;
                        output
                    })
                    .map_err(|mut error| {
                        error.status = Some(reply.status);
                        error.attempts = attempt;
                        error
                    });
            }
            Ok(reply) => {
                retry_after = reply.retry_after;
                error::classify(reply.status, &reply.body, &request.model)
            }
            Err(failure) => error::transport_failure(failure, &request.model),
        };
        error.attempts = attempt;
        if attempt < 2 && !error.output_observed && error.kind.same_bytes_retryable() {
            // The backoff belongs only to a throttled replay: 5xx is retried
            // immediately, and the caller-facing failure stays unchanged.
            if error.kind == LlmErrorKind::RateLimit {
                std::thread::sleep(error::rate_limit_backoff(retry_after));
            }
            continue;
        }
        return Err(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeLlm;
    use serde_json::json;
    use std::time::Duration;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: "hi".to_string(),
            }],
            tools: vec![],
            max_tokens: 64,
        }
    }

    fn completion_body(text: &str) -> String {
        json!({
            "id": "chatcmpl-1",
            "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 5},
        })
        .to_string()
    }

    #[test]
    fn a_successful_call_sends_the_canonical_bytes_exactly_once() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(200, completion_body("answer"));

        let output = complete(&fake, &request()).expect("succeeds");
        assert_eq!(output.text, "answer");
        assert_eq!(output.attempts, 1, "a clean call is one send");
        assert_eq!(fake.call_count(), 1);
        // The bytes on the wire are the bytes the hash covers.
        assert_eq!(fake.sent_body(0), request().canonical_json());
    }

    #[test]
    fn a_retryable_failure_before_output_is_retried_once_then_succeeds() {
        let fake = FakeLlm::new();
        fake.enqueue_failure(false, "connection reset");
        fake.enqueue_reply(200, completion_body("answer"));

        let output = complete(&fake, &request()).expect("retry succeeds");
        assert_eq!(output.text, "answer");
        assert_eq!(fake.call_count(), 2);
        // The retry resends the identical bytes — the hash covers exactly
        // what travels, on every attempt.
        assert_eq!(fake.sent_body(0), fake.sent_body(1));
    }

    #[test]
    fn the_retry_stops_at_one_even_when_the_second_attempt_also_fails() {
        let fake = FakeLlm::new();
        fake.enqueue_failure(false, "connection reset");
        fake.enqueue_failure(false, "connection reset again");
        // A third scripted reply must never be consumed.
        fake.enqueue_reply(200, completion_body("never"));

        let error = complete(&fake, &request()).expect_err("gives up");
        assert_eq!(error.kind, LlmErrorKind::Provider);
        assert_eq!(fake.call_count(), 2, "one retry, then done");
    }

    #[test]
    fn a_failure_after_output_was_observed_is_reported_failed_without_retrying() {
        // The transport saw output begin and then died: tokens are spent and
        // a replay could double-bill, so the failure must be reported as-is.
        let fake = FakeLlm::new();
        fake.enqueue_failure(true, "stream died mid-body");
        fake.enqueue_reply(200, completion_body("never"));

        let error = complete(&fake, &request()).expect_err("honestly failed");
        assert_eq!(error.kind, LlmErrorKind::Provider);
        assert!(error.output_observed);
        assert_eq!(fake.call_count(), 1, "no retry after output");
    }

    #[test]
    fn an_unreadable_2xx_body_is_failed_without_retrying() {
        // A 2xx body is output, even when we cannot read it.
        let fake = FakeLlm::new();
        fake.enqueue_reply(200, "not json at all");
        fake.enqueue_reply(200, completion_body("never"));

        let error = complete(&fake, &request()).expect_err("honestly failed");
        assert_eq!(error.kind, LlmErrorKind::Malformed);
        assert!(error.output_observed);
        // The reply arrived, so the failure carries its status for the record.
        assert_eq!(error.status, Some(200));
        assert_eq!(fake.call_count(), 1, "no retry after output");
    }

    #[test]
    fn a_transient_status_is_retried_once_end_to_end() {
        // 5xx and 408 ride the classify → retryable → resend path (the
        // transport-failure and 429 paths have their own tests).
        for status in [500, 408] {
            let fake = FakeLlm::new();
            fake.enqueue_reply(status, r#"{"error": {"message": "boom"}}"#);
            fake.enqueue_reply(200, completion_body("answer"));

            let output = complete(&fake, &request()).expect("retry succeeds");
            assert_eq!(output.text, "answer");
            assert_eq!(output.attempts, 2, "status {status}");
            assert_eq!(fake.call_count(), 2);
        }
    }

    #[test]
    fn any_2xx_status_counts_as_a_successful_reply() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(201, completion_body("created"));

        let output = complete(&fake, &request()).expect("succeeds");
        assert_eq!(output.text, "created");
        assert_eq!(fake.call_count(), 1);
    }

    #[test]
    fn when_a_permitted_retry_also_fails_the_reported_failure_is_the_second_attempt() {
        // One exchange, one account: the terminal failure is the second
        // attempt's, and `attempts` is what records that a retry happened
        // (a silent retry is not a second turn).
        let fake = FakeLlm::new();
        fake.enqueue_reply(500, r#"{"error": {"message": "first"}}"#);
        fake.enqueue_reply(429, r#"{"error": {"message": "second"}}"#);

        let error = complete(&fake, &request()).expect_err("gives up");
        assert_eq!(error.kind, LlmErrorKind::RateLimit);
        assert_eq!(error.status, Some(429));
        assert_eq!(error.attempts, 2);
        assert_eq!(fake.call_count(), 2);
    }

    #[test]
    fn a_rate_limit_is_retried_once() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(429, r#"{"error": {"message": "slow down"}}"#);
        fake.enqueue_reply(200, completion_body("answer"));

        let output = complete(&fake, &request()).expect("retry succeeds");
        assert_eq!(output.text, "answer");
        assert_eq!(fake.call_count(), 2);
    }

    #[test]
    fn an_auth_rejection_is_not_retried() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(401, r#"{"error": {"message": "bad key"}}"#);
        fake.enqueue_reply(200, completion_body("never"));

        let error = complete(&fake, &request()).expect_err("fails");
        assert_eq!(error.kind, LlmErrorKind::Auth);
        assert_eq!(fake.call_count(), 1);
    }

    #[test]
    fn a_context_overflow_is_not_retried() {
        let fake = FakeLlm::new();
        fake.enqueue_reply(
            400,
            r#"{"error": {"code": "context_length_exceeded", "message": "too big"}}"#,
        );
        fake.enqueue_reply(200, completion_body("never"));

        let error = complete(&fake, &request()).expect_err("fails");
        assert_eq!(error.kind, LlmErrorKind::ContextOverflow);
        assert_eq!(fake.call_count(), 1);
    }

    #[test]
    fn retries_carry_the_request_hash_as_a_stable_idempotency_key() {
        // The header is the provider-side twin of the wire-byte invariant:
        // same canonical body, same key, even across the one permitted retry.
        let fake = FakeLlm::new();
        fake.enqueue_failure(false, "connection reset");
        fake.enqueue_reply(200, completion_body("answer"));

        let output = complete(&fake, &request()).expect("retry succeeds");
        assert_eq!(output.attempts, 2);
        let expected = request().request_hash();
        assert_eq!(fake.sent_idempotency_key(0), expected);
        assert_eq!(fake.sent_idempotency_key(1), expected);
    }

    #[test]
    fn a_rate_limit_uses_the_provider_retry_after_before_replaying() {
        // A zero delay keeps the test instant while still proving `complete`
        // consumes the reply's delay before its replay (the fallback value is
        // pinned by `error.rs`).
        let fake = FakeLlm::new();
        fake.enqueue_reply_after(
            429,
            r#"{"error": {"message": "slow down"}}"#,
            Some(Duration::ZERO),
        );
        fake.enqueue_reply(200, completion_body("answer"));

        let output = complete(&fake, &request()).expect("retry succeeds");
        assert_eq!(output.text, "answer");
        assert_eq!(fake.call_count(), 2);
    }
}
