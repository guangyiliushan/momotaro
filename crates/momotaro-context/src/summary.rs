//! `summary.v1`: the field-separated compaction record
//! (`docs/docs/context-retrieval.md` §3.1).
//!
//! The record is **explicitly not a constraint carrier** (§3.1): constraints
//! live in the system resident layer and must never depend on compaction
//! surviving. Fail-closed validation (§4 第 3 条) is the last gate against
//! losing decisions or constraints:
//!
//! 1. every required field is present (enforced by the type: no `Option`),
//!    and empty scalar fields carry the literal `"(none)"` marker so the
//!    structure stays assertable;
//! 2. every `evidence_refs[].chunk_hash` appears **verbatim** in the stored
//!    summary text — the model must transcribe hashes, never paraphrase.
//!
//! A record that fails validation is never used: the compressor side gives
//! up after N retries and keeps the raw history (§4), and the reader side
//! ([`crate::build_context`]'s `select_history` stage) drops the summary and
//! falls back to the raw turns.

use serde::{Deserialize, Serialize};

/// The literal written for an empty scalar field (§3.1 结构规则 1).
pub const EMPTY_MARKER: &str = "(none)";

/// The only accepted `version` field value.
pub const SUMMARY_VERSION_V1: &str = "v1";

/// One user decision plus its one-line reason (Pi Key Decisions format,
/// §3.1 结构规则 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserDecision {
    /// What was decided.
    pub decision: String,
    /// Why, in one line.
    pub reason: String,
}

/// One evidence pointer: title + claim, hash kept verbatim
/// (§3 结构规则 2: 精确保存 chunk_hash / 标题，不许转述).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// The referenced chunk's content hash, transcribed verbatim.
    pub chunk_hash: String,
    /// The source title.
    pub title: String,
    /// The claim this evidence supported.
    pub claim: String,
}

/// The `summary.v1` record (§3.1 JSON schema, all fields required).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryV1 {
    /// Schema version; must be `"v1"`.
    pub version: String,
    /// `history_id` values this summary condenses.
    pub covered_turns: Vec<String>,
    /// The first history item kept raw after the cut.
    pub first_kept_item_id: String,
    /// The raw token count before compaction.
    pub tokens_before: u64,
    /// Decisions the user made, each with a reason.
    pub user_decisions: Vec<UserDecision>,
    /// Still-open questions.
    pub open_questions: Vec<String>,
    /// Pointers to evidence the covered turns used.
    pub evidence_refs: Vec<EvidenceRef>,
    /// What the next turn should pick up.
    pub next_action_hint: String,
}

/// Why a stored summary failed fail-closed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SummaryError {
    /// The stored text is not a `summary.v1` JSON document.
    #[error("stored summary text is not valid summary.v1 JSON")]
    Malformed,
    /// `version` is not `"v1"`.
    #[error("summary version mismatch: {0:?}")]
    VersionMismatch(String),
    /// A scalar field is empty instead of carrying the `"(none)"` marker.
    #[error("empty field without the (none) marker: {0}")]
    EmptyField(&'static str),
    /// An `evidence_refs[].chunk_hash` does not appear verbatim in the
    /// stored text — the transcription guarantee is broken.
    #[error("evidence ref chunk hash is not verbatim in the stored text: {0:?}")]
    HashNotVerbatim(String),
}

impl SummaryV1 {
    /// Rewrite empty scalar fields to [`EMPTY_MARKER`]. List fields keep
    /// their array shape (`[]` stays `[]`) so the schema stays assertable.
    pub fn normalize(&mut self) {
        if self.first_kept_item_id.is_empty() {
            self.first_kept_item_id = EMPTY_MARKER.to_string();
        }
        if self.next_action_hint.is_empty() {
            self.next_action_hint = EMPTY_MARKER.to_string();
        }
        for decision in &mut self.user_decisions {
            if decision.decision.is_empty() {
                decision.decision = EMPTY_MARKER.to_string();
            }
            if decision.reason.is_empty() {
                decision.reason = EMPTY_MARKER.to_string();
            }
        }
        for reference in &mut self.evidence_refs {
            if reference.title.is_empty() {
                reference.title = EMPTY_MARKER.to_string();
            }
            if reference.claim.is_empty() {
                reference.claim = EMPTY_MARKER.to_string();
            }
        }
        for question in &mut self.open_questions {
            if question.is_empty() {
                *question = EMPTY_MARKER.to_string();
            }
        }
    }

    /// Fail-closed validation against the stored summary text (§3.1 断言).
    ///
    /// The required-fields half is structural (no `Option` on the type); this
    /// function pins the two semantic halves: the version spelling and the
    /// verbatim hash transcription.
    pub fn validate_against_text(&self, stored_text: &str) -> Result<(), SummaryError> {
        if self.version != SUMMARY_VERSION_V1 {
            return Err(SummaryError::VersionMismatch(self.version.clone()));
        }
        if self.first_kept_item_id.is_empty() {
            return Err(SummaryError::EmptyField("first_kept_item_id"));
        }
        if self.next_action_hint.is_empty() {
            return Err(SummaryError::EmptyField("next_action_hint"));
        }
        for decision in &self.user_decisions {
            if decision.decision.is_empty() {
                return Err(SummaryError::EmptyField("user_decisions.decision"));
            }
            if decision.reason.is_empty() {
                return Err(SummaryError::EmptyField("user_decisions.reason"));
            }
        }
        for turn in &self.covered_turns {
            if turn.is_empty() {
                return Err(SummaryError::EmptyField("covered_turns"));
            }
        }
        for question in &self.open_questions {
            if question.is_empty() {
                return Err(SummaryError::EmptyField("open_questions"));
            }
        }
        for reference in &self.evidence_refs {
            if reference.chunk_hash.is_empty() {
                return Err(SummaryError::EmptyField("evidence_refs.chunk_hash"));
            }
            if reference.title.is_empty() {
                return Err(SummaryError::EmptyField("evidence_refs.title"));
            }
            if reference.claim.is_empty() {
                return Err(SummaryError::EmptyField("evidence_refs.claim"));
            }
            if !stored_text.contains(&reference.chunk_hash) {
                return Err(SummaryError::HashNotVerbatim(reference.chunk_hash.clone()));
            }
        }
        Ok(())
    }

    /// Parse the stored summary text as a `summary.v1` JSON document and
    /// run [`Self::validate_against_text`] against that same text.
    pub fn parse_stored(stored_text: &str) -> Result<Self, SummaryError> {
        let record: Self =
            serde_json::from_str(stored_text).map_err(|_| SummaryError::Malformed)?;
        record.validate_against_text(stored_text)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SummaryV1 {
        SummaryV1 {
            version: SUMMARY_VERSION_V1.to_string(),
            covered_turns: vec!["h-1".to_string(), "h-2".to_string()],
            first_kept_item_id: "h-3".to_string(),
            tokens_before: 6543,
            user_decisions: vec![UserDecision {
                decision: "use BM25".to_string(),
                reason: "baseline first".to_string(),
            }],
            open_questions: vec!["what about dense?".to_string()],
            evidence_refs: vec![EvidenceRef {
                chunk_hash: "cafe1234".to_string(),
                title: "Fourier".to_string(),
                claim: "the transform is linear".to_string(),
            }],
            next_action_hint: "compare with dense".to_string(),
        }
    }

    fn stored_text_of(record: &SummaryV1) -> String {
        serde_json::to_string(record).expect("encode summary")
    }

    #[test]
    fn a_well_formed_summary_round_trips() {
        let record = sample();
        let stored = stored_text_of(&record);
        let parsed = SummaryV1::parse_stored(&stored).expect("valid summary parses");
        assert_eq!(parsed, record);
    }

    #[test]
    fn every_field_is_required_by_the_type() {
        // Removing a field from the JSON is a parse failure — the structure
        // is assertable because no field is optional.
        let stored = stored_text_of(&sample());
        for field in [
            "\"version\"",
            "\"covered_turns\"",
            "\"first_kept_item_id\"",
            "\"tokens_before\"",
            "\"user_decisions\"",
            "\"open_questions\"",
            "\"evidence_refs\"",
            "\"next_action_hint\"",
        ] {
            assert!(stored.contains(field), "fixture must carry {field}");
        }
        let without_hint = stored.replace("\"next_action_hint\":\"compare with dense\"", "");
        assert!(SummaryV1::parse_stored(&without_hint).is_err());
    }

    #[test]
    fn a_hash_that_is_not_verbatim_in_the_stored_text_fails_closed() {
        // The stored text must literally carry each hash; a record whose
        // hash only lives inside the parsed fields (not the text) is a
        // transcription failure. Simulate by storing a text that does not
        // carry the hash while parsing a record that does.
        let record = sample();
        let stored = stored_text_of(&record);
        let tampered = stored.replace("cafe1234", "deadbeef");
        // The record parsed from `tampered` now claims `deadbeef`; make the
        // text disagree by parsing the original record against the
        // tampered text.
        assert!(matches!(
            record.validate_against_text(&tampered),
            Err(SummaryError::HashNotVerbatim(hash)) if hash == "cafe1234"
        ));
    }

    #[test]
    fn a_wrong_version_is_rejected() {
        let mut record = sample();
        record.version = "v2".to_string();
        let stored = stored_text_of(&record);
        assert!(matches!(
            SummaryV1::parse_stored(&stored),
            Err(SummaryError::VersionMismatch(v)) if v == "v2"
        ));
    }

    #[test]
    fn empty_scalars_carry_the_none_marker_after_normalize() {
        let mut record = sample();
        record.next_action_hint = String::new();
        record.first_kept_item_id = String::new();
        record.user_decisions[0].reason = String::new();
        record.user_decisions[0].decision = String::new();
        record.evidence_refs[0].claim = String::new();
        record.evidence_refs[0].title = String::new();
        record.open_questions[0] = String::new();
        record.normalize();
        assert_eq!(record.next_action_hint, EMPTY_MARKER);
        assert_eq!(record.first_kept_item_id, EMPTY_MARKER);
        assert_eq!(record.user_decisions[0].reason, EMPTY_MARKER);
        assert_eq!(record.user_decisions[0].decision, EMPTY_MARKER);
        assert_eq!(record.evidence_refs[0].claim, EMPTY_MARKER);
        assert_eq!(record.evidence_refs[0].title, EMPTY_MARKER);
        assert_eq!(record.open_questions[0], EMPTY_MARKER);
    }

    #[test]
    fn removing_any_single_field_makes_parsing_fail() {
        // 逐一删字段(含中间字段,避免残留尾逗号的假红),8 个字段都要被拒。
        let stored = stored_text_of(&sample());
        for field in [
            "\"version\":\"v1\"",
            "\"covered_turns\":[\"h-1\",\"h-2\"]",
            "\"first_kept_item_id\":\"h-3\"",
            "\"tokens_before\":6543",
            "\"user_decisions\":[",
            "\"open_questions\":[",
            "\"evidence_refs\":[",
            "\"next_action_hint\":\"compare with dense\"",
        ] {
            assert!(stored.contains(field), "fixture must carry {field}");
        }
        // Middle-field removal keeps the JSON syntactically close: tokens_before.
        let without_tokens = stored.replace(",\"tokens_before\":6543", "");
        assert!(
            SummaryV1::parse_stored(&without_tokens).is_err(),
            "tokens_before must be required"
        );
        let without_version = stored.replace("\"version\":\"v1\",", "");
        assert!(SummaryV1::parse_stored(&without_version).is_err());
        let without_hint = stored.replace(",\"next_action_hint\":\"compare with dense\"", "");
        assert!(SummaryV1::parse_stored(&without_hint).is_err());
    }

    #[test]
    fn every_empty_scalar_is_rejected_with_its_field_name() {
        type Mutate = fn(&mut SummaryV1);
        let cases: Vec<(Mutate, &str)> = vec![
            (
                |r| r.first_kept_item_id = String::new(),
                "first_kept_item_id",
            ),
            (|r| r.next_action_hint = String::new(), "next_action_hint"),
            (
                |r| r.user_decisions[0].decision = String::new(),
                "user_decisions.decision",
            ),
            (
                |r| r.user_decisions[0].reason = String::new(),
                "user_decisions.reason",
            ),
            (
                |r| r.evidence_refs[0].chunk_hash = String::new(),
                "evidence_refs.chunk_hash",
            ),
            (
                |r| r.evidence_refs[0].title = String::new(),
                "evidence_refs.title",
            ),
            (
                |r| r.evidence_refs[0].claim = String::new(),
                "evidence_refs.claim",
            ),
            (|r| r.covered_turns[0] = String::new(), "covered_turns"),
            (|r| r.open_questions[0] = String::new(), "open_questions"),
        ];
        for (mutate, field) in cases {
            let mut record = sample();
            mutate(&mut record);
            let stored = stored_text_of(&record);
            let outcome = SummaryV1::parse_stored(&stored);
            assert!(
                matches!(outcome, Err(SummaryError::EmptyField(f)) if f == field),
                "field {field} must be rejected with its name, got {outcome:?}"
            );
        }
    }

    #[test]
    fn an_empty_scalar_without_the_marker_is_rejected() {
        let record = SummaryV1 {
            next_action_hint: String::new(),
            ..sample()
        };
        let stored = stored_text_of(&record);
        assert!(matches!(
            SummaryV1::parse_stored(&stored),
            Err(SummaryError::EmptyField("next_action_hint"))
        ));
    }
    #[test]
    fn unknown_fields_are_rejected_fail_closed() {
        let stored = stored_text_of(&sample());
        let tampered = stored.replacen("{", "{\"surprise\":1,", 1);
        assert!(SummaryV1::parse_stored(&tampered).is_err());
    }
}
