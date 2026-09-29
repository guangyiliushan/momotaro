//! Verification vocabulary: the four states, the three kinds, and the detail
//! codes that say *which* judgement fired.
//!
//! These spellings are a contract, not an implementation detail. The four
//! states are what `verifications.status` stores (`docs/docs/reference.md`
//! "Verification states"); the three kinds are what `verifications.kind`
//! stores (`docs/docs/architecture.md` §8.1 and the `migration.rs` column
//! comment). Nothing in this crate writes those columns — `momotaro-run`
//! does — but every spelling it hands over has to match them byte for byte.
//!
//! [`VerificationKind::Math`] is reserved exactly as architecture §14.2
//! reserves it: no code in this crate ever produces it, because 1.0 has no
//! CAS. It exists so a stored `math` row still reads back as a kind.

use serde::Serialize;

/// The outcome of one verification (`verifications.status`).
///
/// The four states and their meanings come from `docs/docs/reference.md`
/// ("Verification states"): `passed` means verification succeeded, `failed`
/// means it explicitly failed, `unsupported` means this build cannot verify
/// the target, `skipped` means verification was disabled.
///
/// The one rule the whole crate is built around: **only an exact match can
/// produce [`Self::Passed`].** Anything half-checked reports `unsupported`
/// or `failed` — never `passed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Verification succeeded.
    Passed,
    /// Verification explicitly failed.
    Failed,
    /// The system cannot verify this target.
    Unsupported,
    /// Verification was disabled or intentionally omitted.
    Skipped,
}

impl VerificationStatus {
    /// Every state, in declaration order. Witness for "the vocabulary is
    /// exactly these four" — add a variant and this array must be edited,
    /// which is the point.
    pub const ALL: [Self; 4] = [Self::Passed, Self::Failed, Self::Unsupported, Self::Skipped];

    /// The spelling that lands in `verifications.status`.
    ///
    /// This — not a serialization format — is the authoritative column
    /// spelling; a test pins the two against each other.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
            Self::Skipped => "skipped",
        }
    }

    /// Reads a stored spelling back.
    ///
    /// Lenient on the way in: an unrecognised value is `None`, never a guess
    /// and never a silent default. Folding an unknown stored state into one
    /// of these four would invent a fact about a row this crate did not write.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "unsupported" => Some(Self::Unsupported),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// What the verification was against (`verifications.kind`).
///
/// `citation` and `source_integrity` are produced by this crate. `math` is
/// reserved (architecture §14.2) and has no producer in 0.x.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// A citation's five-tuple checked against the store.
    Citation,
    /// Whether the bytes named by a source revision are present.
    SourceIntegrity,
    /// Reserved for a future computer-algebra backend.
    Math,
}

impl VerificationKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 3] = [Self::Citation, Self::SourceIntegrity, Self::Math];

    /// The spelling that lands in `verifications.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Citation => "citation",
            Self::SourceIntegrity => "source_integrity",
            Self::Math => "math",
        }
    }

    /// Reads a stored spelling back; `None` for anything unrecognised.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "citation" => Some(Self::Citation),
            "source_integrity" => Some(Self::SourceIntegrity),
            "math" => Some(Self::Math),
            _ => None,
        }
    }
}

/// Which judgement produced a non-`passed` result.
///
/// Carried as a type rather than a message string so a caller can branch on
/// it without string matching. Its spelling is the payload a caller would
/// put in `verifications.details_json`; `momotaro-verify` never writes that
/// column itself.
///
/// Every variant has at least one reachable input; see the per-criterion
/// tests beside each judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyDetail {
    /// Policy turned verification off.
    DisabledByPolicy,
    /// The citation names a context item this run never handed the model.
    ///
    /// Architecture §14.1 rule 4 marks a fabricated citation
    /// `citation_invalid`. The same spelling sits in §18's error taxonomy,
    /// but that one is a `run_events` field: §18 says those categories are
    /// written into `run_events`, which has no `category` column, so they
    /// live in its `data_json`. This crate reports only the verification's
    /// `details_json` slot — the event-side wiring is `momotaro-run`'s.
    CitationInvalid,
    /// The store could not be read while looking for the chunk.
    ChunkUnreadable,
    /// The cited chunk row is not in the store.
    ChunkMissing,
    /// A five-tuple component disagrees with the stored chunk row.
    HashMismatch,
    /// The cited locator disagrees with the stored chunk row.
    LocatorMismatch,
    /// The quoted text cannot be located inside the chunk's text.
    TextNotLocatable,
    /// The bytes named by the revision are not readable.
    SourceMissing,
    /// The bytes are readable but do not hash to the stored revision hash.
    RevisionHashMismatch,
    /// The bytes are present but this build cannot recompute their hash.
    ByteHashUnchecked,
    /// There are no local bytes to check: no store row, no `local_path`, or a
    /// stored path this crate refuses — by form or because it resolves
    /// outside the workspace (ADR 0024) — not the same as "the bytes are
    /// gone", which is [`Self::SourceMissing`].
    NoLocalBytes,
    /// The source bytes could not be read for a reason other than absence.
    SourceBytesUnavailable,
}

impl VerifyDetail {
    /// Every detail code, in declaration order.
    pub const ALL: [Self; 12] = [
        Self::DisabledByPolicy,
        Self::CitationInvalid,
        Self::ChunkUnreadable,
        Self::ChunkMissing,
        Self::HashMismatch,
        Self::LocatorMismatch,
        Self::TextNotLocatable,
        Self::SourceMissing,
        Self::RevisionHashMismatch,
        Self::ByteHashUnchecked,
        Self::NoLocalBytes,
        Self::SourceBytesUnavailable,
    ];

    /// The spelling a caller would persist in `details_json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisabledByPolicy => "disabled_by_policy",
            Self::CitationInvalid => "citation_invalid",
            Self::ChunkUnreadable => "chunk_unreadable",
            Self::ChunkMissing => "chunk_missing",
            Self::HashMismatch => "hash_mismatch",
            Self::LocatorMismatch => "locator_mismatch",
            Self::TextNotLocatable => "text_not_locatable",
            Self::SourceMissing => "source_missing",
            Self::RevisionHashMismatch => "revision_hash_mismatch",
            Self::ByteHashUnchecked => "byte_hash_unchecked",
            Self::NoLocalBytes => "no_local_bytes",
            Self::SourceBytesUnavailable => "source_bytes_unavailable",
        }
    }

    /// Reads a stored spelling back; `None` for anything unrecognised.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|detail| detail.as_str() == code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four states, spelled exactly as the column comment spells them.
    const STATUS_SPELLINGS: [&str; 4] = ["passed", "failed", "unsupported", "skipped"];

    /// The three kinds, spelled exactly as the column comment spells them.
    const KIND_SPELLINGS: [&str; 3] = ["citation", "source_integrity", "math"];

    /// Every detail code, written out **independently of `VerifyDetail::ALL`**.
    ///
    /// `from_code` is defined as "find the member of `ALL` whose `as_str`
    /// matches", so a test that iterates `ALL` cannot notice a variant being
    /// dropped from it — the variant disappears from every assertion at once
    /// and `from_code` then rejects a spelling this crate itself emits. Two of
    /// these spellings are contract wording, not ours: `citation_invalid` is
    /// architecture §14.1 rule 4 / §18, and `source_missing` is
    /// `adr/note-rename-is-a-new-source.md`, `reference.md` and §18.
    const DETAIL_SPELLINGS: [&str; 12] = [
        "disabled_by_policy",
        "citation_invalid",
        "chunk_unreadable",
        "chunk_missing",
        "hash_mismatch",
        "locator_mismatch",
        "text_not_locatable",
        "source_missing",
        "revision_hash_mismatch",
        "byte_hash_unchecked",
        "no_local_bytes",
        "source_bytes_unavailable",
    ];

    #[test]
    fn every_status_round_trips_through_its_column_spelling() {
        assert_eq!(
            VerificationStatus::ALL.len(),
            STATUS_SPELLINGS.len(),
            "the status vocabulary grew or shrank"
        );
        for spelling in STATUS_SPELLINGS {
            let status = VerificationStatus::from_code(spelling)
                .unwrap_or_else(|| panic!("column spelling {spelling:?} is not a status"));
            assert_eq!(
                status.as_str(),
                spelling,
                "as_str must reproduce the column"
            );
            assert_eq!(
                VerificationStatus::from_code(status.as_str()),
                Some(status),
                "as_str and from_code must be inverses"
            );
        }
    }

    #[test]
    fn the_status_vocabulary_is_exactly_the_four_documented_states() {
        let mut from_all: Vec<&str> = VerificationStatus::ALL.iter().map(|s| s.as_str()).collect();
        from_all.sort_unstable();
        let mut expected = STATUS_SPELLINGS;
        expected.sort_unstable();
        assert_eq!(
            from_all, expected,
            "ALL must enumerate the four states, no more no less"
        );
    }

    #[test]
    fn every_kind_round_trips_through_its_column_spelling() {
        assert_eq!(
            VerificationKind::ALL.len(),
            KIND_SPELLINGS.len(),
            "the kind vocabulary grew or shrank"
        );
        for spelling in KIND_SPELLINGS {
            let kind = VerificationKind::from_code(spelling)
                .unwrap_or_else(|| panic!("column spelling {spelling:?} is not a kind"));
            assert_eq!(kind.as_str(), spelling, "as_str must reproduce the column");
            assert_eq!(
                VerificationKind::from_code(kind.as_str()),
                Some(kind),
                "as_str and from_code must be inverses"
            );
        }
    }

    #[test]
    fn an_unrecognised_spelling_is_none_rather_than_a_guess() {
        // Lenient on the way in (`None`), never a silent default: the caller
        // decides what an unknown stored value means. This crate must not
        // fold it into one of its own states.
        assert_eq!(VerificationStatus::from_code("ok"), None);
        assert_eq!(VerificationStatus::from_code(""), None);
        assert_eq!(VerificationStatus::from_code("Passed"), None);
        assert_eq!(VerificationKind::from_code("citation_missing"), None);
        assert_eq!(VerificationKind::from_code(""), None);
    }

    #[test]
    fn every_detail_code_round_trips_through_its_own_spelling() {
        for detail in VerifyDetail::ALL {
            let code = detail.as_str();
            assert_eq!(
                VerifyDetail::from_code(code),
                Some(detail),
                "as_str must reproduce a spelling from_code accepts: {code:?}"
            );
        }
    }

    #[test]
    fn every_detail_spelling_is_pinned_to_a_literal_list() {
        assert_eq!(
            VerifyDetail::ALL.len(),
            DETAIL_SPELLINGS.len(),
            "the detail vocabulary grew or shrank"
        );
        for spelling in DETAIL_SPELLINGS {
            let detail = VerifyDetail::from_code(spelling)
                .unwrap_or_else(|| panic!("pinned spelling {spelling:?} is not a detail"));
            assert_eq!(
                detail.as_str(),
                spelling,
                "as_str must reproduce the pinned spelling"
            );
        }
    }

    #[test]
    fn detail_spellings_do_not_collide() {
        let mut from_all: Vec<&str> = VerifyDetail::ALL.iter().map(|d| d.as_str()).collect();
        from_all.sort_unstable();
        let mut unique = from_all.clone();
        unique.dedup();
        assert_eq!(
            from_all.len(),
            unique.len(),
            "two detail codes share one spelling — downstream would be unable to tell them apart"
        );
    }

    #[test]
    fn an_unrecognised_detail_code_is_none_rather_than_a_guess() {
        assert_eq!(VerifyDetail::from_code("some_future_code"), None);
        assert_eq!(VerifyDetail::from_code(""), None);
    }

    /// `as_str()` is the column spelling; serde is the JSON spelling. They are
    /// two mechanisms for one contract, so pin them to each other — a serde
    /// behaviour change must not silently fork the two.
    #[test]
    fn the_json_spelling_and_the_column_spelling_are_the_same_string() {
        for status in VerificationStatus::ALL {
            assert_eq!(
                serde_json::to_string(&status).expect("serialize"),
                format!("\"{}\"", status.as_str()),
                "status JSON drifted from the column spelling"
            );
        }
        for kind in VerificationKind::ALL {
            assert_eq!(
                serde_json::to_string(&kind).expect("serialize"),
                format!("\"{}\"", kind.as_str()),
                "kind JSON drifted from the column spelling"
            );
        }
        for detail in VerifyDetail::ALL {
            assert_eq!(
                serde_json::to_string(&detail).expect("serialize"),
                format!("\"{}\"", detail.as_str()),
                "detail JSON drifted from the column spelling"
            );
        }
    }
}
