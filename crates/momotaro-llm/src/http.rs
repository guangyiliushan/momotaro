//! The production HTTP transport behind [`ProviderTransport`].
//!
//! `momotaro-llm` builds and parses provider traffic; this module is the one
//! place that actually opens a socket. Everything else in the crate — and every
//! test in the workspace — still goes through the injected seam
//! (`fake::FakeLlm`), so CI never touches the network. Construction is a pure
//! function of the endpoint and the credential: it performs no I/O and is what
//! the offline tests drive.
//!
//! Three properties the tests pin down (grill ruling Q3, 2026-09-30):
//!
//! * **4xx/5xx are replies, not errors.** `http_status_as_error(false)` keeps a
//!   provider's status and body visible so `error::classify` can turn them into
//!   a typed [`crate::LlmErrorKind`]. With ureq's default (`true`) a 401 would
//!   arrive as `Err(Error::StatusCode)` and lose the body that names the
//!   failure.
//! * **The trust root is the platform's.** `RootCerts::PlatformVerifier` reads
//!   the OS certificate store (enterprise CAs, revocation) instead of ureq's
//!   default static Mozilla bundle. That is why the feature set is
//!   `rustls-no-provider` + `_ring` + `platform-verifier`: ureq's plain `rustls`
//!   feature also pulls `webpki-roots`, which this build refuses to carry.
//! * **A failure message never carries the request.** The trait's trusted-
//!   component rule forbids credentials, headers and secret-bearing URLs in
//!   [`TransportError::message`]; the `describe` helper below therefore only
//!   forwards payloads it has inspected.
//!
//! Body reads are bounded by ureq's 10 MiB default limit (`Body::read_to_string`),
//! which is what [`crate::transport::TransportReply`] delegates to the transport.
//!
//! **What no test in this repo exercises: the I/O body of `send`.** Opening a
//! socket in a test would break the zero-network guarantee the crate is built
//! around, so the covered surface is everything up to it — the two config
//! properties above, the connect timeout, the endpoint/key refusals, and the
//! per-variant message mapping. Three things live *inside* `send` and a mistake
//! there would change behaviour silently: which endpoint is POSTed to, which
//! phase a failure reports as having spent output, and where the status comes
//! from. All three are pinned structurally, by `send`'s own source order — see
//! `send_is_a_composition_of_three_things_a_test_can_reach`.

use std::time::Duration;

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};

use crate::transport::{ProviderTransport, TransportError, TransportReply};

/// Appended to the configured base URL: this build speaks the OpenAI-compatible
/// chat-completions protocol (`docs/docs/architecture.md` §7, `[model]`).
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// How long establishing the connection (including the TLS handshake) may take.
///
/// A transport constant, not a user knob: a TCP+TLS session cannot legitimately
/// take longer than this, whereas a *generation* can — so only the connect phase
/// is bounded here. Leaving the reply unbounded is deliberate: an arbitrary
/// global deadline would cut off a slow but perfectly valid long answer, and
/// picking a number for that belongs to the model/timeout contract, not to the
/// transport. Known remaining face: a peer that accepts the connection and then
/// stalls mid-reply hangs the call until the user cancels it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an endpoint could not be turned into a transport.
///
/// Decidable on purpose: each variant names the shape that was refused without
/// echoing the value, because the value may carry the credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportConfigError {
    /// The endpoint is not an absolute `http://` / `https://` URL.
    #[error("the provider endpoint must be an absolute http:// or https:// URL")]
    EndpointNotHttp,
    /// The URL has no host to connect to (`https:///path`, `https://`).
    #[error("the provider endpoint has no host")]
    EndpointWithoutHost,
    /// The endpoint carries userinfo, which would put the credential in the
    /// request line (and in anything that echoes a URL).
    #[error("the provider endpoint must not embed a credential; use the api key parameter")]
    EndpointWithUserInfo,
    /// The API key is not printable ASCII without spaces, so it cannot go into
    /// an HTTP header here — the refusal belongs at the configuration seam,
    /// before any request is attempted, not at the socket.
    #[error("the API key must be printable ASCII without spaces, so it can go into an HTTP header")]
    UnusableApiKey,
}

/// A blocking, OpenAI-compatible HTTP transport.
///
/// Built once per command and reused: the underlying `Agent` holds a
/// connection pool. What that pool is worth depends on *which* retry fires,
/// and `complete` allows exactly one (`attempt < 2 && !output_observed &&
/// same_bytes_retryable`, so a `RateLimit` or a `Provider` error):
///
/// - a **429, a 5xx or an HTTP 408** reads the reply in full, `classify`
///   records it as no output observed, and — unless ureq marked the connection
///   `must_close` — it goes back to the pool, so attempt two gets that socket
///   and no new handshake;
/// - a **connect-phase failure** (refused, connect timeout, TLS) never
///   reached a socket that could be pooled, so attempt two pays a fresh
///   handshake like any first request would.
///
/// Either way one command is one process here, so "expand then `ask`" is not
/// a case this pool ever sees.
pub struct HttpTransport {
    agent: Agent,
    /// Full URL the request body is POSTed to.
    endpoint: String,
    /// `scheme://host[:port]` — userinfo, path, query and fragment stripped —
    /// the only part of the endpoint that may appear in a message.
    origin: String,
    /// `Some` when a key was supplied; sent as `Authorization: Bearer …`.
    api_key: Option<String>,
}

impl HttpTransport {
    /// Builds a transport for `base_url` (`docs/docs/architecture.md` §7), e.g.
    /// `https://api.example.com/v1`.
    ///
    /// Performs no I/O. The key is optional — a local endpoint may need none —
    /// and an empty key is treated as no key rather than as `Bearer `.
    pub fn new(base_url: &str, api_key: Option<&str>) -> Result<Self, TransportConfigError> {
        let origin = endpoint_origin(base_url)?;
        let api_key = api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| {
                // A key becomes an HTTP header value. The rule is deliberately
                // narrower than what `http` would merely *tolerate*: printable
                // ASCII with no spaces. Anything else is refused here so the
                // refusal happens at the configuration seam — no request is
                // ever attempted with a header we cannot vouch for — instead of
                // surfacing as a send-time error.
                if key.chars().any(|c| !c.is_ascii_graphic()) {
                    Err(TransportConfigError::UnusableApiKey)
                } else {
                    Ok(key.to_owned())
                }
            })
            .transpose()?;

        let endpoint = format!("{}{CHAT_COMPLETIONS_PATH}", base_url.trim_end_matches('/'));
        let agent = Agent::config_builder()
            // The seam answers with a raw status + body (TransportReply), so a
            // 4xx/5xx must stay a reply for `error::classify` to read.
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();

        Ok(Self {
            agent,
            endpoint,
            origin,
            api_key,
        })
    }

    /// The URL requests are POSTed to — the configured base plus the
    /// chat-completions path.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The endpoint as it may be quoted in a message: scheme and host only.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether this transport carries a credential.
    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// The headers every exchange carries.
    ///
    /// Split out of [`ProviderTransport::send`] because `send`'s I/O body
    /// cannot run in a test while this can — and this is the only place that
    /// knows the credential travels as a header at all.
    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("content-type", "application/json".to_owned()),
            ("accept", "application/json".to_owned()),
        ];
        if let Some(key) = &self.api_key {
            headers.push(("authorization", format!("Bearer {key}")));
        }
        headers
    }

    /// One transport-level failure: which phase failed, and the sentence it is
    /// allowed to carry.
    ///
    /// Built from [`Self::origin`], never from [`Self::endpoint`] — the
    /// endpoint is allowed to hold a path and a query, and
    /// [`ProviderTransport`]'s trusted-component rule says neither may reach a
    /// message.
    fn failure(&self, error: ureq::Error, output_observed: bool) -> TransportError {
        TransportError {
            output_observed,
            message: message(&self.origin, &error),
        }
    }
}

impl std::fmt::Debug for HttpTransport {
    /// The credential never reaches a formatter: `{:?}` on a transport must not
    /// be a way to print it. Hand-written rather than derived for exactly that.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("endpoint", &self.endpoint)
            .field("origin", &self.origin)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ProviderTransport for HttpTransport {
    /// The three decisions this body makes are each a call into something
    /// testable — `headers()`, `failure()`, and `response.status()` — because
    /// the body itself never runs under test (the module docs say so, and why).
    /// The structural test pins that composition; changing any one of them in
    /// here takes it red.
    fn send(&self, body: &str) -> Result<TransportReply, TransportError> {
        let mut request = self.agent.post(&self.endpoint);
        for (name, value) in self.headers() {
            request = request.header(name, value);
        }

        // A failure here means no reply arrived at all: nothing was observable
        // yet, so the client's one permitted retry still applies.
        let mut response = request
            .send(body)
            .map_err(|error| self.failure(error, false))?;
        let status = response.status().as_u16();

        // From here the provider has answered: whatever goes wrong while the
        // body is being read is output that has already been spent, so the
        // client must never replay the request.
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| self.failure(error, true))?;

        Ok(TransportReply { status, body: text })
    }
}

/// The one sentence a transport failure is allowed to carry: the origin (which
/// has no credential, path or query) plus a detail [`describe`] has inspected.
fn message(origin: &str, error: &ureq::Error) -> String {
    format!("{origin}: {}", describe(error))
}

/// Renders one ureq failure.
///
/// Only payloads this module has looked at reach the text. Everything else
/// collapses to a static sentence, because `ProviderTransport`'s trusted-
/// component rule says a message must never carry credentials, headers, or a
/// URL with secrets — and variants such as `BadUri` / `RequireHttpsOnly` echo
/// the endpoint verbatim while `Other` wraps text nobody here has inspected.
fn describe(error: &ureq::Error) -> String {
    use ureq::Error;

    match error {
        Error::StatusCode(code) => format!("HTTP {code}"),
        Error::BodyExceedsLimit(limit) => {
            format!("the response body exceeded the {limit}-byte read limit")
        }
        Error::HostNotFound => "the host could not be resolved".to_owned(),
        Error::ConnectionFailed => "the connection could not be established".to_owned(),
        Error::Timeout(when) => format!("the {when} timeout elapsed"),
        Error::Tls(reason) => format!("TLS: {reason}"),
        Error::Rustls(reason) => format!("TLS: {reason}"),
        // An `io::Error` from the socket layer names the syscall or the OS
        // reason, never the request bytes.
        Error::Io(source) => source.to_string(),
        _ => "the exchange ended without a complete reply".to_owned(),
    }
}

/// Splits an endpoint into its scheme and its `host[:port]`, refusing the
/// shapes this transport will not speak.
fn endpoint_origin(url: &str) -> Result<String, TransportConfigError> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(TransportConfigError::EndpointNotHttp);
    };

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Err(TransportConfigError::EndpointWithUserInfo);
    }
    if authority.is_empty() {
        return Err(TransportConfigError::EndpointWithoutHost);
    }

    Ok(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base: &str) -> String {
        HttpTransport::new(base, None)
            .expect("a plain endpoint is accepted")
            .endpoint()
            .to_owned()
    }

    #[test]
    fn the_base_url_gains_the_chat_completions_path_once() {
        assert_eq!(
            endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/chat/completions"
        );
        // A trailing separator must not double up into `//chat/completions`.
        assert_eq!(
            endpoint("https://api.example.com/v1/"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/chat/completions"
        );
    }

    #[test]
    fn the_agent_reads_a_4xx_or_5xx_as_a_reply_not_an_error() {
        // Without this, a provider's 401 body never reaches `classify` and the
        // failure loses the structured code that decides its category.
        let transport = HttpTransport::new("https://api.example.com/v1", None).expect("transport");
        assert!(
            !transport.agent.config().http_status_as_error(),
            "http_status_as_error must be off for the seam to see the status"
        );
    }

    #[test]
    fn the_agent_verifies_against_the_platform_certificate_store() {
        // The whole point of grill ruling Q3: no static Mozilla bundle.
        let transport = HttpTransport::new("https://api.example.com/v1", None).expect("transport");
        let tls = transport.agent.config().tls_config();
        assert!(
            matches!(tls.root_certs(), RootCerts::PlatformVerifier),
            "roots come from the platform, not webpki-roots: {:?}",
            tls.root_certs()
        );
        assert!(!tls.disable_verification(), "verification stays on");
    }

    #[test]
    fn establishing_a_connection_is_bounded() {
        let transport = HttpTransport::new("https://api.example.com/v1", None).expect("transport");
        // The literal, not `CONNECT_TIMEOUT`: a test that compares the config
        // to the constant it was built from lets *any* value through,
        // including one that would make every real dial time out.
        assert_eq!(
            transport.agent.config().timeouts().connect,
            Some(Duration::from_secs(10)),
            "a dead network must not hang the call in DNS/TCP"
        );
        assert_eq!(
            CONNECT_TIMEOUT,
            Duration::from_secs(10),
            "and the constant the transport builds from agrees with it"
        );
    }

    #[test]
    fn an_endpoint_that_cannot_be_spoken_is_refused_by_shape() {
        for (url, expected) in [
            ("api.example.com/v1", TransportConfigError::EndpointNotHttp),
            (
                "ftp://api.example.com",
                TransportConfigError::EndpointNotHttp,
            ),
            ("https://", TransportConfigError::EndpointWithoutHost),
            ("https:///v1", TransportConfigError::EndpointWithoutHost),
            (
                "https://user:secret@api.example.com/v1",
                TransportConfigError::EndpointWithUserInfo,
            ),
        ] {
            let error = HttpTransport::new(url, None).expect_err("must be refused");
            assert_eq!(error, expected, "endpoint {url:?}");
            // The refusal must not echo the value: it can hold the credential.
            assert!(
                !error.to_string().contains("secret"),
                "the message leaked the endpoint: {error}"
            );
        }
    }

    #[test]
    fn a_key_that_cannot_be_a_header_is_refused_without_echoing_it() {
        let error = HttpTransport::new(
            "https://api.example.com/v1",
            Some("sk-DO-NOT-ECHO\u{0007}tail"),
        )
        .expect_err("a control character cannot go into a header");
        assert_eq!(error, TransportConfigError::UnusableApiKey);
        // The refusal names the shape, never the value: the value is the
        // credential.
        assert!(
            !error.to_string().contains("DO-NOT-ECHO"),
            "the key is echoed: {error}"
        );

        // Every shape the rule rejects, not only the control character: a
        // narrower rule would still pass the probe above while letting a
        // space or a non-ASCII byte reach the socket.
        for (label, key) in [
            ("a space", "sk-a b"),
            ("a non-ASCII letter", "sk-caf\u{e9}"),
            ("a newline", "sk-a\nb"),
        ] {
            let error = HttpTransport::new("https://api.example.com/v1", Some(key))
                .expect_err(&format!("{label} must be refused"));
            assert_eq!(error, TransportConfigError::UnusableApiKey, "{label}");
        }

        // ASCII-graphic keys are exactly the token68 shape providers use.
        HttpTransport::new("https://api.example.com/v1", Some("sk-abc_123.XYZ+/="))
            .expect("a printable key is accepted");
    }

    #[test]
    fn an_absent_or_empty_key_means_no_credential_at_all() {
        let without_key =
            HttpTransport::new("https://api.example.com/v1", None).expect("transport");
        assert!(!without_key.has_api_key());
        // An empty key must not become the header `Bearer `.
        let empty =
            HttpTransport::new("https://api.example.com/v1", Some("  ")).expect("transport");
        assert!(!empty.has_api_key());

        let with_key =
            HttpTransport::new("https://api.example.com/v1", Some("sk-123")).expect("transport");
        assert!(with_key.has_api_key());
    }

    #[test]
    fn the_origin_quoted_in_messages_carries_no_path_query_or_userinfo() {
        let transport =
            HttpTransport::new("https://user:pass@api.example.com/v1?api-key=zzz", None);
        // userinfo is refused outright, so the remaining leaks are path/query.
        assert_eq!(
            transport.expect_err("userinfo is refused"),
            TransportConfigError::EndpointWithUserInfo
        );

        let transport = HttpTransport::new("https://api.example.com:8443/v1?api-key=zzz", None)
            .expect("a query is allowed on the endpoint itself");
        assert_eq!(transport.origin(), "https://api.example.com:8443");
        assert!(!transport.origin().contains("api-key"));
        assert!(!transport.origin().contains("/v1"));
    }

    #[test]
    fn a_failure_message_names_the_origin_and_never_the_request() {
        // `BadUri` echoes the URL it rejected verbatim, `RequireHttpsOnly`
        // echoes it too, and `Http` can echo a header value — none of them may
        // reach a message.
        let rejected_header = ureq::http::HeaderValue::from_bytes(b"bad\r\nvalue")
            .expect_err("a control byte is not a header value");
        for error in [
            ureq::Error::BadUri("https://api.example.com/v1?api-key=zzz".to_owned()),
            ureq::Error::RequireHttpsOnly("https://api.example.com/v1?api-key=zzz".to_owned()),
            ureq::Error::Http(rejected_header.into()),
            ureq::Error::HostNotFound,
            ureq::Error::ConnectionFailed,
            ureq::Error::Timeout(ureq::Timeout::Connect),
            ureq::Error::Tls("certificate has expired"),
            ureq::Error::StatusCode(503),
            ureq::Error::BodyExceedsLimit(10 * 1024 * 1024),
            ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            )),
        ] {
            let text = describe(&error);
            assert!(
                !text.contains("api-key") && !text.contains("zzz"),
                "{error:?} leaked the URL into {text:?}"
            );
            assert!(!text.is_empty(), "{error:?} rendered empty");
        }

        // The origin is the diagnostic half and it is the safe half.
        let full = message("https://api.example.com", &ureq::Error::HostNotFound);
        assert_eq!(
            full,
            "https://api.example.com: the host could not be resolved"
        );
    }

    #[test]
    fn an_uninspected_failure_kind_still_says_something_true() {
        // The catch-all must not invent detail it does not have — and it must
        // not echo the `Box<dyn Error>` that `Other` wraps.
        let opaque: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("https://api.example.com?api-key=zzz"));
        let text = describe(&ureq::Error::Other(opaque));
        assert_eq!(text, "the exchange ended without a complete reply");
    }

    #[test]
    fn send_is_a_composition_of_three_things_a_test_can_reach() {
        // `send`'s I/O body never runs under test (see the module docs), so
        // every decision it makes has to live where a test *can* reach it:
        // `headers()` for what goes on the wire, `failure()` for what a
        // failure may say and whether output was spent, and `response.status()`
        // for the status the classifier will see. This pins that `send` is a
        // composition of those three, in that order — moving a decision back
        // inline, or hard-coding a status, takes it red.
        let source = include_str!("http.rs");
        let body = source
            .split("fn send")
            .nth(1)
            .expect("send exists")
            .split("\n}")
            .next()
            .expect("send body");

        let target = body
            .find(".post(&self.endpoint)")
            .expect("the POST is aimed at the endpoint");
        assert!(
            !body.contains(".post(&self.origin)"),
            "and never at the origin, which carries no path: {body}"
        );
        let headers = body.find("self.headers()").expect("the headers are built");
        let call = body.find(".send(body)").expect("the call is made");
        assert!(
            target < headers,
            "the request is aimed before it is headed: {body}"
        );
        let before_output = body
            .find("self.failure(error, false)")
            .expect("a pre-reply failure reports no output");
        let status = body
            .find("response.status()")
            .expect("the real status reaches TransportReply");
        let read = body.find("read_to_string()").expect("the body is read");
        let after_output = body
            .find("self.failure(error, true)")
            .expect("a mid-body failure reports spent output");

        assert!(headers < call, "headers are attached first: {body}");
        assert!(
            call < before_output,
            "the call's arm is the one that reports no output: {body}"
        );
        assert!(
            before_output < status,
            "only the call's arm may report no output: {body}"
        );
        assert!(status < read, "the status is read before the body: {body}");
        assert!(
            read < after_output,
            "the body's arm is the one that reports spent output: {body}"
        );
        assert_eq!(
            body.matches("self.failure(").count(),
            2,
            "exactly two failure sites, and no inline TransportError: {body}"
        );
    }

    #[test]
    fn the_credential_travels_as_an_authorization_header_only_when_there_is_one() {
        // `has_api_key()` is only a field read; this is the wire shape, and
        // the only proof that the credential is attached at all.
        let with_key =
            HttpTransport::new("https://api.example.com/v1", Some("sk-abc")).expect("transport");
        assert_eq!(
            with_key.headers(),
            vec![
                ("content-type", "application/json".to_owned()),
                ("accept", "application/json".to_owned()),
                ("authorization", "Bearer sk-abc".to_owned()),
            ],
            "an authenticated exchange asks for it explicitly"
        );

        for (label, key) in [("no key", None), ("an empty key", Some("  "))] {
            let transport =
                HttpTransport::new("https://api.example.com/v1", key).expect("transport");
            assert!(
                transport
                    .headers()
                    .iter()
                    .all(|(name, _)| *name != "authorization"),
                "{label} must not send an Authorization header"
            );
        }
    }

    #[test]
    fn a_debug_rendering_never_prints_the_credential() {
        // The Debug impl is hand-written rather than derived for this reason,
        // so it has to be asserted — a derived one, or one that returns
        // nothing, would both still satisfy every other test here.
        let transport = HttpTransport::new("https://api.example.com/v1", Some("sk-SECRET-VALUE"))
            .expect("transport");
        let rendered = format!("{transport:?}");

        assert!(
            rendered.contains("https://api.example.com/v1"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(
            !rendered.contains("sk-SECRET-VALUE"),
            "the credential leaked into a formatter: {rendered}"
        );
    }

    #[test]
    fn a_failure_sentence_is_built_from_the_origin_and_never_the_endpoint() {
        // The endpoint may carry a path and a query — a provider that takes a
        // query-string credential is a real shape — and neither may reach a
        // message (the trait's trusted-component rule).
        let transport = HttpTransport::new("https://api.example.com:8443/v1?api-key=zzz", None)
            .expect("a query is allowed on the endpoint");

        let failure = transport.failure(ureq::Error::HostNotFound, false);
        assert!(!failure.output_observed, "no reply arrived yet");
        assert!(
            failure.message.starts_with("https://api.example.com:8443:"),
            "the origin leads the sentence: {}",
            failure.message
        );
        for forbidden in ["api-key", "zzz", "/v1", "/chat/completions"] {
            assert!(
                !failure.message.contains(forbidden),
                "the message carries {forbidden:?}: {}",
                failure.message
            );
        }

        // …and the phase is still reported: a body that dies mid-read has
        // spent output and must never be replayed.
        let spent = transport.failure(ureq::Error::ConnectionFailed, true);
        assert!(spent.output_observed, "the body arm reports spent output");
        assert!(spent.message.starts_with("https://api.example.com:8443:"));
    }

    #[test]
    fn a_status_the_provider_answered_with_stays_in_the_sentence() {
        // Deleting the `StatusCode` arm would send a real 4xx/5xx to the
        // catch-all, and the leak-check below would not notice.
        assert_eq!(
            describe(&ureq::Error::StatusCode(503)),
            "HTTP 503",
            "the status the provider answered with must survive into the message"
        );
    }

    #[test]
    fn every_inspected_variant_renders_detail_of_its_own() {
        // Presence of an arm proves nothing about what it renders: an arm
        // rewritten to `String::new()` would pass the structural check below
        // and quietly cost every failure of that variant its diagnosis.
        assert_eq!(
            describe(&ureq::Error::StatusCode(429)),
            "HTTP 429",
            "the status survives"
        );
        assert_eq!(
            describe(&ureq::Error::HostNotFound),
            "the host could not be resolved"
        );
        assert_eq!(
            describe(&ureq::Error::ConnectionFailed),
            "the connection could not be established"
        );
        assert_eq!(
            describe(&ureq::Error::Timeout(ureq::Timeout::Connect)),
            "the connect timeout elapsed"
        );
        assert_eq!(
            describe(&ureq::Error::Tls("certificate has expired")),
            "TLS: certificate has expired"
        );
        assert_eq!(
            describe(&ureq::Error::BodyExceedsLimit(1024)),
            "the response body exceeded the 1024-byte read limit"
        );
        assert_eq!(
            describe(&ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused"
            ))),
            "refused"
        );
    }

    #[test]
    fn the_inspected_arms_are_the_ones_that_reach_a_message() {
        // `describe`'s contract is two-sided: nothing uninspected is forwarded
        // (pinned below), *and* each variant it does inspect keeps the detail
        // it was inspected for. Deleting an arm degrades every failure of that
        // variant into the generic sentence, and the leak check alone would
        // never notice — it only proves the message stays safe, not useful.
        let source = include_str!("http.rs");
        let body = source
            .split("fn describe")
            .nth(1)
            .expect("describe exists")
            .split("\n}")
            .next()
            .expect("describe body");

        for arm in [
            "Error::StatusCode(code) =>",
            "Error::BodyExceedsLimit(limit) =>",
            "Error::HostNotFound =>",
            "Error::ConnectionFailed =>",
            "Error::Timeout(when) =>",
            "Error::Tls(reason) =>",
            "Error::Rustls(reason) =>",
            "Error::Io(source) =>",
        ] {
            assert!(
                body.contains(arm),
                "`describe` must keep the `{arm}` arm: {body}"
            );
        }
    }

    #[test]
    fn the_message_rule_holds_for_the_ureq_variants_that_echo_the_request() {
        // Structural, because `describe` is fed real `ureq::Error`s only from
        // `send`: the property under test is *which variants it is willing to
        // render*, and a variant that interpolates its payload is what would
        // leak the endpoint (or the credential inside it) into a message.
        let source = include_str!("http.rs");
        let body = source
            .split("fn describe")
            .nth(1)
            .expect("describe exists")
            .split("\n}")
            .next()
            .expect("describe body");

        // The catch-all says something true and carries no payload. The needles
        // live in this test, which the slice below cannot reach: `body` ends at
        // the first column-0 `}` after `fn describe`, and the tests module is
        // further down the file.
        assert!(
            body.contains("_ => \"the exchange ended without a complete reply\""),
            "the catch-all must be the static sentence: {body}"
        );

        // Any variant whose Display echoes the endpoint or a header value must
        // not appear as an arm at all — absence is what puts it in the
        // catch-all. Adding `Error::BadUri(v) => format!(…)` goes red here.
        for echoing in [
            "BadUri",
            "RequireHttpsOnly",
            "ConnectProxyFailed",
            "InvalidProxyUrl",
            "Http(",
            "Other(",
        ] {
            assert!(
                !body.contains(echoing),
                "{echoing} would forward request material into a message: {body}"
            );
        }
    }

    #[test]
    fn the_feature_surface_carries_no_static_root_bundle() {
        // Grill ruling Q3: ureq's `webpki-roots` must not be in this build.
        // The lockfile is the artefact that proves it — a feature flip lands
        // here as a red test instead of as a silent extra crate.
        //
        // `webpki-root-certs` is deliberately not asserted against: it enters
        // Cargo.lock through rustls-platform-verifier's wasm32-only branch and
        // is never compiled on any target this project builds for.
        let lock = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("Cargo.lock");
        let text = std::fs::read_to_string(&lock).expect("read Cargo.lock");
        assert!(
            !text.contains(concat!("name = ", "\"webpki-roots\"")),
            "webpki-roots entered the graph; ureq's feature set must stay on \
             rustls-no-provider + platform-verifier"
        );
        assert!(
            text.contains(concat!("name = ", "\"ureq\"")),
            "ureq itself is expected in the lock"
        );
        assert!(
            text.contains(concat!("name = ", "\"rustls-platform-verifier\"")),
            "and it must be carrying the platform verifier"
        );
    }
}
