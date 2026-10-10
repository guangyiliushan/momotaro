//! Typed failure and the decidable classification behind it.
//!
//! Classification is a pure function of the HTTP status and the provider's
//! structured `error.code` — never of free-text message content, so a provider
//! whose message blathers about context length while reporting another code
//! must not land in [`LlmErrorKind::ContextOverflow`]. Downstream code branches
//! on [`LlmErrorKind`], never on [`LlmError::message`] text.

use std::time::Duration;

use thiserror::Error as ThisError;

/// Why one provider call failed.
///
/// Every kind rolls up to its `docs/docs/architecture.md` §18 category via
/// [`Self::category`], so upstream `run_events` can record a category for
/// every failure without inventing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorKind {
    /// Credentials missing or rejected (HTTP 401/403). Category `llm_auth`.
    Auth,
    /// Provider throttling (HTTP 429). Category `llm_rate_limit`.
    RateLimit,
    /// The request exceeds the model window (HTTP 413, or any 4xx with the
    /// structured code `context_length_exceeded`). Category `llm_context_overflow`.
    ContextOverflow,
    /// The request as sent cannot be answered: any other 4xx, a redirect the
    /// transport did not follow (3xx), or any status outside the wire contract.
    /// Resending the same bytes cannot fix it. Category `llm_rejected`.
    Rejected,
    /// Transient provider-side trouble: most 5xx statuses except 501, an HTTP 408, or a
    /// transport failure (a refused connection, a body stream that died before
    /// or during output). Category `llm_provider`; the stream-death timing
    /// lives in [`LlmError::output_observed`], not in this kind.
    Provider,
    /// A 2xx response arrived but is not a readable completion.
    /// Category `llm_malformed`.
    Malformed,
}

/// The structured `error.code` that marks a context overflow, matched by exact
/// equality on the raw code only — sanitization never participates in the
/// comparison, so a code spliced with invisible characters cannot be
/// normalized into a hit.
const CONTEXT_OVERFLOW_CODE: &str = "context_length_exceeded";

/// Fallback pause before replaying a throttled request.
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_millis(100);

/// RFC 1123 month spellings used by HTTP-date `Retry-After` values.
const HTTP_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl LlmErrorKind {
    /// The `docs/docs/architecture.md` §18 error-taxonomy category this kind
    /// rolls up to, spelled exactly as that table spells it.
    pub fn category(self) -> &'static str {
        match self {
            Self::Auth => "llm_auth",
            Self::RateLimit => "llm_rate_limit",
            Self::ContextOverflow => "llm_context_overflow",
            Self::Provider => "llm_provider",
            Self::Rejected => "llm_rejected",
            Self::Malformed => "llm_malformed",
        }
    }

    /// Whether resending the identical request could plausibly succeed.
    ///
    /// Deliberately narrower than — and named apart from — §18's Retryable
    /// column: that column answers "may the run recover?" (overflow = maybe,
    /// by re-asking with less context), while this answers "may the *same
    /// bytes* succeed on a second send?" — which is the only retry
    /// [`crate::complete`] ever performs. Upstream `run_events.retryable` must
    /// be read from the §18 table via [`Self::category`], never from this
    /// method.
    pub fn same_bytes_retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Provider)
    }
}

/// One typed provider failure.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
#[error("{message}")]
pub struct LlmError {
    /// Decidable failure class; branch on this, never on `message`.
    pub kind: LlmErrorKind,
    /// HTTP status, when a response arrived at all.
    pub status: Option<u16>,
    /// The provider's structured `error.code`, when it sent one — already
    /// sanitized like [`Self::message`]. Classification matched the **raw**
    /// spelling, so this display copy can look like a canonical code that
    /// [`Self::kind`] did not select: branch on `kind`, never on this field.
    pub provider_code: Option<String>,
    /// Human-readable account of what failed, with provider-supplied detail
    /// already sanitized.
    pub message: String,
    /// Whether provider output was already observable when the failure hit
    /// (a 2xx body arrived, or a transport died mid-body). Output spent is not
    /// retryable and is worth recording on the failed turn.
    pub output_observed: bool,
    /// How many provider sends this outcome took (1 = no retry). Stamped by
    /// [`crate::complete`]; the classification helpers report 1. When a
    /// permitted retry also failed, this is what records the retry — see
    /// [`crate::complete`] for the one-exchange policy.
    pub attempts: u8,
    /// Token usage the provider managed to report even though the exchange
    /// failed (a 2xx body that carried usage but was not a readable
    /// completion). `None` when no usage object was reported. Boxed because
    /// `Usage` is 80 bytes of `Option<u64>`s and clippy's
    /// `result_large_err` (128-byte threshold) fires on the bare form —
    /// the box keeps `Result<LlmOutput, LlmError>` compact; read it with
    /// `as_deref().copied()`.
    pub usage: Option<Box<crate::response::Usage>>,
}

/// Decode one provider error body into its structured fields: `error.code` and
/// `error.message`, verbatim. Both are optional; a non-JSON body simply yields
/// `None`s. Both are attacker-controlled strings: [`classify`] matches the
/// code **raw** and sanitizes both fields on the way into the record.
fn parse_error_envelope(body: &str) -> (Option<String>, Option<String>) {
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    let error = &envelope["error"];
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    (code, message)
}

/// Classify one non-2xx provider response. The kind is a pure function of the
/// status code and the structured `error.code` (matched raw — exact equality
/// only, so normalization cannot smuggle a near-miss spelling into a bucket);
/// `provider_message` only ever decorates `message`.
pub(crate) fn classify(status: u16, body: &str, model: &str) -> LlmError {
    let (provider_code, provider_message) = parse_error_envelope(body);
    let kind = match status {
        401 | 403 => LlmErrorKind::Auth,
        429 => LlmErrorKind::RateLimit,
        413 => LlmErrorKind::ContextOverflow,
        408 => LlmErrorKind::Provider,
        400..=499 if provider_code.as_deref() == Some(CONTEXT_OVERFLOW_CODE) => {
            LlmErrorKind::ContextOverflow
        }
        400..=499 => LlmErrorKind::Rejected,
        // Not implemented is a contract mismatch, not a transient outage:
        // resending the same bytes can only produce the same answer.
        501 => LlmErrorKind::Rejected,
        500..=599 => LlmErrorKind::Provider,
        _ => LlmErrorKind::Rejected,
    };

    let mut message = format!("the provider call for model {model} failed: HTTP {status}");
    if let Some(code) = &provider_code {
        message.push_str(&format!(" (code {code})"));
    }
    if let Some(detail) = &provider_message {
        message.push_str(&format!(" — {detail}"));
    }

    LlmError {
        kind,
        status: Some(status),
        provider_code: provider_code.map(|code| sanitize(&code)),
        // The one sanitize at the end covers the provider detail too.
        message: sanitize(&message),
        output_observed: false,
        attempts: 1,
        usage: None,
    }
}

/// Build the failure for a transport-level error (no HTTP response).
pub(crate) fn transport_failure(
    failure: crate::transport::TransportError,
    model: &str,
) -> LlmError {
    let when = if failure.output_observed {
        "died after output had begun"
    } else {
        "failed before any output"
    };
    LlmError {
        kind: LlmErrorKind::Provider,
        status: None,
        provider_code: None,
        message: sanitize(&format!(
            "the provider call for model {model} {when}: {}",
            failure.message
        )),
        output_observed: failure.output_observed,
        attempts: 1,
        usage: None,
    }
}

/// Chooses the pause before replaying a throttled reply.
///
/// A decodable provider hint wins; otherwise the small constant keeps a retry
/// from hammering a peer that has already said no. `zero` is intentional for
/// tests and means "the provider said retry now".
pub(crate) fn rate_limit_backoff(retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF)
}

/// Decodes an HTTP `Retry-After` value at one exchange's boundary.
///
/// Seconds are the cheap, exact form. HTTP-date is deliberately limited to
/// RFC 1123 (`Wed, 25 Feb 2026 01:02:03 GMT`), the spelling the RFC names and
/// providers send; anything else is treated as absent rather than guessed.
pub(crate) fn retry_after(value: Option<&str>, now: std::time::SystemTime) -> Option<Duration> {
    let value = value?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let now_epoch = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let retry_epoch = http_date_epoch(value)?;
    Some(Duration::from_secs(retry_epoch.saturating_sub(now_epoch)))
}

/// Reads the fixed RFC 1123 HTTP-date shape into whole seconds since Unix.
fn http_date_epoch(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let _day_name = parts.next()?;
    let day = parts.next()?;
    let month = parts.next()?;
    let year = parts.next()?;
    let clock = parts.next()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }

    let month_number = HTTP_MONTHS
        .iter()
        .position(|spelling| spelling.eq_ignore_ascii_case(month))?
        + 1;
    let (hour, minute, second) = {
        let mut numbers = clock.split(':');
        let hour = numbers.next()?.parse::<u64>().ok()?;
        let minute = numbers.next()?.parse::<u64>().ok()?;
        let second = numbers.next()?.parse::<u64>().ok()?;
        if numbers.next().is_some() {
            return None;
        }
        (hour, minute, second)
    };
    if day.len() != 2 || year.len() != 4 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let day = day.parse::<i64>().ok()?;
    let year = year.parse::<i64>().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }

    // Howard Hinnant's civil-date formula: enough to decode an HTTP-date
    // without inventing a calendar dependency for one optional header.
    let adjusted_year = if month_number <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year.rem_euclid(400);
    let month_number = month_number as i64;
    let day_of_year =
        (153 * (month_number + if month_number > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    u64::try_from(seconds).ok()
}

/// Fold foreign text into a terminal-safe line: line breaks become spaces and
/// unprintable characters are dropped — C0/C1 controls can drive the terminal,
/// and the bidi / zero-width format characters can hide or reorder the rest of
/// the line. Same rule as `momotaro-run`'s private `sanitize`.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !is_unprintable(*c))
        .collect()
}

/// Whether `c` must never reach a terminal: C0/C1 controls plus the format
/// characters that reorder or hide text.
pub fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'                    // SOFT HYPHEN
                | '\u{061c}'              // ARABIC LETTER MARK
                | '\u{180e}'              // MONGOLIAN VOWEL SEPARATOR
                | '\u{200b}'..='\u{200f}' // ZWSP, ZWNJ, ZWJ, LRM, RLM
                | '\u{202a}'..='\u{202e}' // bidi embedding / override
                | '\u{2060}'..='\u{206f}' // word joiner .. invisible operators
                | '\u{feff}'              // BOM / zero width no-break space
                | '\u{e0000}'..='\u{e007f}' // tag characters
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_with(code: &str, message: &str) -> String {
        serde_json::json!({"error": {"message": message, "code": code}}).to_string()
    }

    #[test]
    fn classification_is_decidable_from_status_and_structured_code() {
        for (status, code, expected) in [
            (401, "", LlmErrorKind::Auth),
            (403, "", LlmErrorKind::Auth),
            (429, "", LlmErrorKind::RateLimit),
            (413, "", LlmErrorKind::ContextOverflow),
            (400, CONTEXT_OVERFLOW_CODE, LlmErrorKind::ContextOverflow),
            (400, "invalid_api_key", LlmErrorKind::Rejected),
            (404, "model_not_found", LlmErrorKind::Rejected),
            (408, "", LlmErrorKind::Provider),
            (500, "", LlmErrorKind::Provider),
            (503, "", LlmErrorKind::Provider),
            // A redirect the transport did not follow is not a provider
            // outage: resending the same bytes to it cannot help.
            (302, "", LlmErrorKind::Rejected),
        ] {
            let error = classify(status, &body_with(code, "whatever"), "m");
            assert_eq!(error.kind, expected, "status {status} code {code:?}");
            assert_eq!(error.status, Some(status));
        }
    }

    #[test]
    fn a_context_length_message_with_another_code_is_not_an_overflow() {
        let body = body_with(
            "billing_hard_limit",
            "context length exceeded for this model!",
        );
        let error = classify(400, &body, "m");
        assert_eq!(error.kind, LlmErrorKind::Rejected);
    }

    #[test]
    fn an_overflow_code_with_an_unrelated_message_is_an_overflow() {
        let body = body_with(CONTEXT_OVERFLOW_CODE, "quota exhausted");
        let error = classify(400, &body, "m");
        assert_eq!(error.kind, LlmErrorKind::ContextOverflow);
    }

    #[test]
    fn near_miss_codes_are_not_context_overflows() {
        // Exact equality only (module doc): case drift or a version suffix
        // must not smuggle a request into the overflow bucket.
        for code in ["Context_Length_Exceeded", "context_length_exceeded_v2"] {
            let error = classify(400, &body_with(code, "too big"), "m");
            assert_eq!(error.kind, LlmErrorKind::Rejected, "code {code:?}");
        }
    }

    #[test]
    fn a_code_spliced_with_invisible_characters_is_not_normalized_into_an_overflow() {
        // The comparison runs on the RAW code (module doc): the ZWSP-spliced
        // spelling is a near miss like any other, and sanitizing before
        // comparing would smuggle it into the overflow bucket.
        let spliced = "context_length_\u{200b}exceeded";
        let error = classify(400, &body_with(spliced, "too big"), "m");
        assert_eq!(error.kind, LlmErrorKind::Rejected);
        // The stored copy is display-sanitized and can look like the overflow
        // spelling — branch on `kind`, never on this field.
        assert_eq!(error.provider_code.as_deref(), Some(CONTEXT_OVERFLOW_CODE));
    }

    #[test]
    fn the_overflow_code_wins_on_any_4xx_not_just_400() {
        // Contract decision (variant doc): the structured code is the
        // evidence, the exact 4xx status is not pinned to 400.
        let error = classify(404, &body_with(CONTEXT_OVERFLOW_CODE, "?"), "m");
        assert_eq!(error.kind, LlmErrorKind::ContextOverflow);
    }

    #[test]
    fn the_status_arms_win_over_the_provider_error_code() {
        // A 429 body that happens to carry the overflow code is still a
        // throttle: the status arms are checked before the code-guarded 400
        // arm, and reordering them must not flip this.
        let error = classify(429, &body_with(CONTEXT_OVERFLOW_CODE, "slow down"), "m");
        assert_eq!(error.kind, LlmErrorKind::RateLimit);
    }

    #[test]
    fn a_body_without_a_structured_code_falls_back_to_the_status() {
        let error = classify(429, "plain text, not json", "m");
        assert_eq!(error.kind, LlmErrorKind::RateLimit);
        assert_eq!(error.provider_code, None);
    }

    #[test]
    fn the_taxonomy_categories_are_the_architecture_spellings() {
        // Flipped together with the §18 rows that gave the last three kinds
        // their categories; every failure must land in the table.
        for (kind, expected) in [
            (LlmErrorKind::Auth, "llm_auth"),
            (LlmErrorKind::RateLimit, "llm_rate_limit"),
            (LlmErrorKind::ContextOverflow, "llm_context_overflow"),
            (LlmErrorKind::Provider, "llm_provider"),
            (LlmErrorKind::Rejected, "llm_rejected"),
            (LlmErrorKind::Malformed, "llm_malformed"),
        ] {
            assert_eq!(kind.category(), expected);
        }
    }

    #[test]
    fn same_request_retryability_follows_the_taxonomy_where_it_speaks() {
        assert!(!LlmErrorKind::Auth.same_bytes_retryable());
        assert!(LlmErrorKind::RateLimit.same_bytes_retryable());
        // §18 says "maybe" for run-level recovery (re-ask smaller); the same
        // bytes would overflow again, so the client never resends.
        assert!(!LlmErrorKind::ContextOverflow.same_bytes_retryable());
        // None of the remaining kinds are retryable either: a refused request
        // and an unreadable body would fail identically on a second send.
        assert!(!LlmErrorKind::Rejected.same_bytes_retryable());
        assert!(!LlmErrorKind::Malformed.same_bytes_retryable());
        // The run-level Retryable column is the §18 table's, not this method's:
        // overflow disagrees with it on purpose (maybe vs false).
        assert!(LlmErrorKind::Provider.same_bytes_retryable());
    }

    #[test]
    fn provider_detail_in_the_message_is_sanitized() {
        let hostile = "bad\u{202e}gnp.exe\u{200b}x\nline\u{1b}[2J";
        let error = classify(400, &body_with("whatever", hostile), "m");
        for forbidden in ['\u{202e}', '\u{200b}', '\u{1b}', '\n'] {
            assert!(
                !error.message.contains(forbidden),
                "message still carries {forbidden:?}: {}",
                error.message
            );
        }
        // The readable words survive, folded onto one line — the whitespace is
        // folded to a space, not dropped ("x line", not "xline").
        assert!(error.message.contains("bad"));
        assert!(error.message.contains("gnp.exe"));
        assert!(error.message.contains("x line"));
    }

    #[test]
    fn the_structured_provider_code_is_sanitized_too() {
        // The code is a diagnostic field downstream may print; it gets the
        // same terminal-safe folding as the message.
        let hostile = "co\u{202e}de\u{1b}[2J";
        let error = classify(400, &body_with(hostile, "meh"), "m");
        let code = error.provider_code.expect("code recorded");
        for forbidden in ['\u{202e}', '\u{1b}'] {
            assert!(
                !code.contains(forbidden),
                "code still carries {forbidden:?}"
            );
        }
        assert!(error.message.contains("code"), "message cites the code");
    }

    #[test]
    fn every_unprintable_class_is_neutralized() {
        // One representative per listed class, so deleting a whole range from
        // `is_unprintable` goes red here. Format characters and controls
        // vanish entirely.
        for hostile in [
            "\u{00ad}",  // SOFT HYPHEN
            "\u{061c}",  // ARABIC LETTER MARK
            "\u{180e}",  // MONGOLIAN VOWEL SEPARATOR
            "\u{200b}",  // ZWSP
            "\u{200f}",  // RLM (top of 200b..=200f)
            "\u{202a}",  // LRE
            "\u{202e}",  // RLO
            "\u{2060}",  // word joiner
            "\u{2066}",  // LRI isolate (inside 2060..=206f)
            "\u{feff}",  // BOM
            "\u{e0001}", // tag character
            "\u{1b}",    // ESC (C0)
            "\u{9f}",    // APC (pure C1, no ASCII — an `is_ascii_control`
                         // narrowing would silently keep this class alive)
        ] {
            assert_eq!(sanitize(hostile), "", "kept {hostile:?}");
        }
        // NEL is whitespace before it is a control (like `\n`), so it folds
        // to a single space — neutralized by folding, not by dropping.
        assert_eq!(sanitize("a\u{85}b"), "a b");
    }

    #[test]
    fn a_transport_failure_that_already_saw_output_says_so() {
        use crate::transport::TransportError;

        let error = transport_failure(
            TransportError {
                output_observed: true,
                // Foreign text on the diagnosis path: the fold must eat the
                // terminal controls while keeping the readable words.
                message: "reset by peer \u{202e}gnp.exe\u{1b}[2J".to_string(),
            },
            "m",
        );
        assert_eq!(error.kind, LlmErrorKind::Provider);
        assert!(error.output_observed);
        assert!(error.message.contains("died after output had begun"));
        for forbidden in ['\u{202e}', '\u{001b}'] {
            assert!(
                !error.message.contains(forbidden),
                "message still carries {forbidden:?}: {}",
                error.message
            );
        }
        assert!(error.message.contains("reset by peer"));
    }

    #[test]
    fn not_implemented_is_not_a_transient_provider_failure() {
        let error = classify(501, &body_with("", "no such endpoint"), "m");
        assert_eq!(error.kind, LlmErrorKind::Rejected);
        assert!(!error.kind.same_bytes_retryable());
    }

    #[test]
    fn rate_limit_backoff_prefers_the_provider_hint_and_has_a_small_fallback() {
        assert_eq!(
            rate_limit_backoff(Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(rate_limit_backoff(None), Duration::from_millis(100));
    }

    #[test]
    fn retry_after_decodes_seconds_and_http_dates() {
        let now = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        assert_eq!(retry_after(Some("7"), now), Some(Duration::from_secs(7)));
        assert_eq!(
            retry_after(Some("Wed, 01 Jan 2020 00:01:41 GMT"), now),
            Some(Duration::from_secs(101))
        );
        assert_eq!(retry_after(None, now), None);
        assert_eq!(retry_after(Some("not a date"), now), None);
    }
}
