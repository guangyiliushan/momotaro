//! Output types and the decoder for one OpenAI-compatible completion body.
//!
//! Recording rule (architecture §16): the provider reports what it reports and
//! we write it down; a token field the provider omits stays `None` (= NULL in
//! `llm_turns`) and is never faked as zero.

use serde::Deserialize;

use crate::error::{LlmError, LlmErrorKind};

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model ran out of things to say.
    Stop,
    /// The model hit `max_tokens`.
    Length,
    /// The model asked to call a tool.
    ToolCalls,
    /// The provider filtered the content.
    ContentFilter,
    /// A provider-specific spelling we do not model, preserved verbatim.
    /// Constructed only by [`Self::from_wire`] — building `Other("stop")` by
    /// hand yields a non-canonical twin of [`Self::Stop`]. Escaping is the
    /// display point's job, not the record's.
    Other(String),
}

impl FinishReason {
    /// Parse the wire spelling.
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_string()),
        }
    }

    /// The wire spelling, as sent or as received.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Other(raw) => raw,
        }
    }
}

/// Token accounting for one turn. `None` means the provider did not report the
/// figure — the `llm_turns` column stays NULL. A reported zero is `Some(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Tokens billed as input.
    pub input_tokens: Option<u64>,
    /// Tokens billed as output.
    pub output_tokens: Option<u64>,
    /// Input tokens the provider reports as cache hits.
    pub cached_tokens: Option<u64>,
    /// The provider's actual billing basis (after cache discounts), when it
    /// reports one.
    pub billable_tokens: Option<u64>,
    /// Thinking tokens inside the output (billed at output rates, D26),
    /// reported as `usage.completion_tokens_details.reasoning_tokens`.
    pub reasoning_tokens: Option<u64>,
}

/// What one provider call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmOutput {
    /// The assistant message content; empty when the model returned none
    /// (e.g. a `tool_calls` turn, signalled by `finish_reason`). Recorded
    /// verbatim; escaping is the display point's job (same rule as
    /// [`Self::raw_ref`]).
    pub text: String,
    /// Token accounting exactly as reported.
    pub usage: Usage,
    /// Why the model stopped.
    pub finish_reason: FinishReason,
    /// The provider's response id, when it exposes one. Recorded verbatim —
    /// escaping is the display point's job, not the record's.
    pub raw_ref: Option<String>,
    /// How many provider sends this outcome took (1 = no retry). Stamped by
    /// [`crate::complete`]; a directly decoded body reports 1.
    pub attempts: u8,
}

#[derive(Deserialize)]
struct WireResponse {
    id: Option<String>,
    choices: Vec<WireChoice>,
    usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct WireMessage {
    content: Option<String>,
}

/// Decode one 2xx provider body into an [`LlmOutput`].
///
/// Anything that is not a readable completion — bad JSON, no choices, no
/// message — becomes a typed [`LlmErrorKind::Malformed`] failure, never a
/// panic. A body existed, so these failures carry `output_observed: true`:
/// whatever the provider billed is spent, and a retry is off the table. Any
/// usage the body still lets us read is kept on the failure (§16: the
/// provider reports, we record — a half-readable body is no reason to drop
/// the numbers it did report).
///
/// Wire field sources: `text` ← `choices[0].message.content`,
/// `finish_reason` ← `choices[0].finish_reason`, `raw_ref` ← `id`, and the
/// usage fields ← `usage.{prompt_tokens,completion_tokens,billable_tokens}`.
/// `cached_tokens` is read from `usage.cached_tokens`, falling back to
/// `usage.prompt_tokens_details.cached_tokens` (where OpenAI-compatible
/// providers report it); `reasoning_tokens` is read from
/// `usage.completion_tokens_details.reasoning_tokens`. Each is recorded only
/// when reported (read field-wise by [`usage_from_value`]).
pub(crate) fn parse_response(body: &str) -> Result<LlmOutput, LlmError> {
    let wire: WireResponse = serde_json::from_str(body).map_err(|error| {
        malformed(
            body,
            format!("the provider body is not a completion: {error}"),
        )
    })?;
    let choice = wire
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| malformed(body, "the provider body carries no choices".to_string()))?;

    Ok(LlmOutput {
        text: choice.message.content.unwrap_or_default(),
        usage: wire
            .usage
            .as_ref()
            .map(usage_from_value)
            .unwrap_or_default(),
        finish_reason: FinishReason::from_wire(&choice.finish_reason),
        raw_ref: wire.id,
        attempts: 1,
    })
}

/// The failure for a 2xx body that is not a usable completion.
///
/// The composed message is terminal-safe by construction (fixed prose plus
/// serde's Debug-escaped error text — no input can put a raw hostile byte in
/// it), so the `sanitize` pass below is defense-in-depth with **no
/// instrumented face**: this face of the "message is sanitized" promise has
/// no killable test, and the load-bearing ones live on `classify` and
/// `transport_failure`, where foreign text really enters.
fn malformed(body: &str, message: String) -> LlmError {
    LlmError {
        kind: LlmErrorKind::Malformed,
        status: None,
        provider_code: None,
        message: crate::error::sanitize(&message),
        output_observed: true,
        attempts: 1,
        usage: best_effort_usage(body).map(Box::new),
    }
}

/// Salvage the `usage` object from a body that is not a readable completion,
/// so a provider that reported numbers still gets them recorded. A `usage`
/// that is not an object is not a usage record and yields `None`.
fn best_effort_usage(body: &str) -> Option<Usage> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let usage = value.get("usage").filter(|u| u.is_object())?;
    Some(usage_from_value(usage))
}

/// Read one `usage` object field-wise — the single wire reader for both the
/// completion path and the salvage path. A field the provider omitted or
/// mistyped stays `None`: a half-readable usage object still reports its
/// readable numbers, and nothing is invented (§16: provider reports, we
/// record — 不报为 NULL, a reported zero is `Some(0)`).
fn usage_from_value(usage: &serde_json::Value) -> Usage {
    fn number(object: &serde_json::Value, key: &str) -> Option<u64> {
        object.get(key).and_then(serde_json::Value::as_u64)
    }
    let nested_cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| number(details, "cached_tokens"));
    Usage {
        input_tokens: number(usage, "prompt_tokens"),
        output_tokens: number(usage, "completion_tokens"),
        cached_tokens: number(usage, "cached_tokens").or(nested_cached),
        billable_tokens: number(usage, "billable_tokens"),
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|details| number(details, "reasoning_tokens")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_accounting_keeps_all_four_cached_billable_states() {
        // The four states of (cached reported?, billable reported?). Distinct
        // values on every field so any field swap dies here too.
        let cases = [
            (Some(5), Some(8)),
            (Some(5), None),
            (None, Some(8)),
            (None, None),
        ];
        for (cached, billable) in cases {
            let mut usage = json!({"prompt_tokens": 10, "completion_tokens": 20});
            if let Some(value) = cached {
                usage["cached_tokens"] = json!(value);
            }
            if let Some(value) = billable {
                usage["billable_tokens"] = json!(value);
            }
            let body = json!({
                "id": "chatcmpl-1",
                "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                "usage": usage,
            })
            .to_string();

            let output = parse_response(&body).expect("reads");
            assert_eq!(
                output.usage,
                Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(20),
                    cached_tokens: cached,
                    billable_tokens: billable,
                    reasoning_tokens: None,
                },
                "state cached={cached:?} billable={billable:?}"
            );
        }
    }

    #[test]
    fn a_usage_object_missing_an_input_or_output_number_keeps_it_null() {
        // "不报为 NULL" covers the input/output columns too: a half-present
        // usage must not be papered over with `unwrap_or(0)`.
        for (usage_json, expected) in [
            (
                json!({"completion_tokens": 5}),
                Usage {
                    input_tokens: None,
                    output_tokens: Some(5),
                    ..Usage::default()
                },
            ),
            (
                json!({"prompt_tokens": 10}),
                Usage {
                    input_tokens: Some(10),
                    output_tokens: None,
                    ..Usage::default()
                },
            ),
        ] {
            let body = json!({
                "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                "usage": usage_json,
            })
            .to_string();
            let output = parse_response(&body).expect("reads");
            assert_eq!(output.usage, expected);
        }
    }

    #[test]
    fn a_reported_zero_is_recorded_as_some_zero_not_null() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "cached_tokens": 0, "billable_tokens": 0,
                       "completion_tokens_details": {"reasoning_tokens": 0}},
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        assert_eq!(
            output.usage,
            Usage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cached_tokens: Some(0),
                billable_tokens: Some(0),
                reasoning_tokens: Some(0),
            }
        );
    }

    #[test]
    fn cached_tokens_also_reads_from_prompt_tokens_details() {
        // OpenAI-compatible providers report cache hits nested; recording
        // NULL there would contradict "provider 报什么记什么".
        let nested = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20,
                       "prompt_tokens_details": {"cached_tokens": 5}},
        })
        .to_string();
        let output = parse_response(&nested).expect("reads");
        assert_eq!(output.usage.cached_tokens, Some(5));

        // A flat `cached_tokens` wins when both spellings are present.
        let both = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "cached_tokens": 3,
                       "prompt_tokens_details": {"cached_tokens": 5}},
        })
        .to_string();
        let output = parse_response(&both).expect("reads");
        assert_eq!(output.usage.cached_tokens, Some(3));
    }

    #[test]
    fn an_absent_usage_object_keeps_every_column_null() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        // Literal expectations, field by field — `Usage::default()` here
        // would be an identity assertion against the type under test.
        assert_eq!(
            output.usage,
            Usage {
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                billable_tokens: None,
                reasoning_tokens: None,
            }
        );
    }

    #[test]
    fn text_finish_reason_and_raw_ref_come_back_from_the_wire() {
        let body = json!({
            "id": "chatcmpl-abc",
            "choices": [{"message": {"content": "answer"}, "finish_reason": "length"}],
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        assert_eq!(output.text, "answer");
        assert_eq!(output.finish_reason, FinishReason::Length);
        assert_eq!(output.raw_ref.as_deref(), Some("chatcmpl-abc"));
        // "a directly decoded body reports 1" (LlmOutput::attempts doc).
        assert_eq!(output.attempts, 1);
    }

    #[test]
    fn a_missing_id_is_recorded_as_none() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        assert_eq!(output.raw_ref, None);
    }

    #[test]
    fn a_tool_calls_turn_reads_as_empty_text_with_the_reason_preserved() {
        let body = json!({
            "choices": [{"message": {"content": null, "tool_calls": [{"id": "c1"}]}, "finish_reason": "tool_calls"}],
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        assert_eq!(output.text, "");
        assert_eq!(output.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn an_unknown_finish_reason_spelling_is_kept_verbatim() {
        for raw in ["function_call", "banana"] {
            let body = json!({
                "choices": [{"message": {"content": "x"}, "finish_reason": raw}],
            })
            .to_string();
            let output = parse_response(&body).expect("reads");
            assert_eq!(output.finish_reason, FinishReason::Other(raw.to_string()));
            assert_eq!(output.finish_reason.as_str(), raw);
        }
    }

    #[test]
    fn known_finish_reason_spellings_round_trip() {
        for (raw, typed) in [
            ("stop", FinishReason::Stop),
            ("length", FinishReason::Length),
            ("tool_calls", FinishReason::ToolCalls),
            ("content_filter", FinishReason::ContentFilter),
        ] {
            assert_eq!(FinishReason::from_wire(raw), typed);
            assert_eq!(typed.as_str(), raw);
        }
    }

    #[test]
    fn an_unreadable_body_is_a_malformed_failure_not_a_panic() {
        for body in [
            "not json at all",
            "{}",
            r#"{"choices": []}"#,
            r#"{"choices": [{"finish_reason": "stop"}]}"#,
        ] {
            let error = parse_response(body).expect_err("must fail");
            assert_eq!(error.kind, LlmErrorKind::Malformed);
            // The body existed: this failure is post-output and must never
            // be retried (architecture §16, retry only before output).
            assert!(error.output_observed);
        }
    }

    #[test]
    fn a_null_or_missing_finish_reason_is_not_a_readable_completion() {
        // Intentional strictness, pinned: the terminal status is a recorded
        // field (reference.md, LLM turn), so a completion without one is not
        // a completion. Tolerating null would be a contract change.
        for body in [
            r#"{"choices": [{"message": {"content": "hi"}, "finish_reason": null}]}"#,
            r#"{"choices": [{"message": {"content": "hi"}}]}"#,
        ] {
            let error = parse_response(body).expect_err("must fail");
            assert_eq!(error.kind, LlmErrorKind::Malformed);
            assert!(error.output_observed);
        }
    }

    #[test]
    fn only_the_first_choice_is_read() {
        let body = json!({
            "choices": [
                {"message": {"content": "first"}, "finish_reason": "stop"},
                {"message": {"content": "second"}, "finish_reason": "length"},
            ],
        })
        .to_string();
        let output = parse_response(&body).expect("reads");
        assert_eq!(output.text, "first");
        assert_eq!(output.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn cached_tokens_keeps_a_reported_zero_even_when_nested_reports_one() {
        // "报 0 记 Some(0)" holds on both spellings: no value is dropped or
        // upgraded just because it is zero.
        let flat_zero = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "cached_tokens": 0,
                       "prompt_tokens_details": {"cached_tokens": 5}},
        })
        .to_string();
        assert_eq!(
            parse_response(&flat_zero)
                .expect("reads")
                .usage
                .cached_tokens,
            Some(0),
            "the flat spelling wins even at zero"
        );

        let nested_zero = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20,
                       "prompt_tokens_details": {"cached_tokens": 0}},
        })
        .to_string();
        assert_eq!(
            parse_response(&nested_zero)
                .expect("reads")
                .usage
                .cached_tokens,
            Some(0),
            "a reported zero is not turned into NULL"
        );
    }

    #[test]
    fn reasoning_tokens_are_recorded_only_when_reported() {
        let reported = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20,
                       "completion_tokens_details": {"reasoning_tokens": 7}},
        })
        .to_string();
        assert_eq!(
            parse_response(&reported)
                .expect("reads")
                .usage
                .reasoning_tokens,
            Some(7)
        );

        let absent = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20},
        })
        .to_string();
        assert_eq!(
            parse_response(&absent)
                .expect("reads")
                .usage
                .reasoning_tokens,
            None
        );
    }

    #[test]
    fn a_malformed_body_keeps_the_usage_it_still_lets_us_read() {
        // §16: provider reports, we record — "no choices" is a failure, but
        // the numbers the body carries are spent money and must not vanish.
        let body = json!({
            "usage": {"prompt_tokens": 100, "completion_tokens": 3},
        })
        .to_string();
        let error = parse_response(&body).expect_err("not a completion");
        assert_eq!(error.kind, LlmErrorKind::Malformed);
        assert_eq!(
            error.usage.as_deref().copied(),
            Some(Usage {
                input_tokens: Some(100),
                output_tokens: Some(3),
                ..Usage::default()
            })
        );

        // Nothing readable (not even JSON) means nothing to salvage.
        let error = parse_response("not json at all").expect_err("not a completion");
        assert_eq!(error.usage, None);
    }

    #[test]
    fn a_usage_object_with_one_unreadable_field_keeps_the_numbers_it_can_still_read() {
        // Field-wise salvage: `completion_tokens` is not a number, but the 10
        // the provider did report is spent money and must not vanish. An
        // all-or-nothing salvage drops it — this test is what pins the
        // difference.
        let body = json!({
            "usage": {"prompt_tokens": 10, "completion_tokens": "x"},
            "choices": [],
        })
        .to_string();
        let error = parse_response(&body).expect_err("not a completion");
        assert_eq!(
            error.usage.as_deref().copied(),
            Some(Usage {
                input_tokens: Some(10),
                output_tokens: None,
                ..Usage::default()
            })
        );

        // A `usage` that is not an object is not a usage record at all.
        let body = json!({"usage": "junk", "choices": []}).to_string();
        let error = parse_response(&body).expect_err("not a completion");
        assert_eq!(error.usage, None);
    }

    #[test]
    fn a_malformed_message_never_carries_raw_hostile_body_text() {
        // The message is machine-composed: fixed prose plus serde's
        // Debug-escaped error text. A hostile body can only reach the message
        // in escaped form — embedding the raw body instead must fail here.
        // (The `sanitize` pass in `malformed` is defense-in-depth beyond this
        // and is not observable through any input — see its doc.)
        let hostile = "{\"choices\": \"evil\u{202e}x\u{1b}[2J\"}";
        let error = parse_response(hostile).expect_err("not a completion");
        for forbidden in ['\u{202e}', '\u{001b}'] {
            assert!(!error.message.contains(forbidden));
        }
        assert!(
            error
                .message
                .contains("the provider body is not a completion")
        );
    }
}
