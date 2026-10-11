//! What a judgement concludes.

use crate::status::{VerificationKind, VerificationStatus, VerifyDetail};

/// One independent verification result.
///
/// A verification is an independent result produced by deterministic
/// code. Nothing here is a model's opinion — this crate holds
/// no LLM slot and spends no tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verification {
    /// What was verified.
    pub kind: VerificationKind,
    /// The outcome.
    pub status: VerificationStatus,
    /// Which judgement fired, absent for a clean `passed`.
    pub detail: Option<VerifyDetail>,
}

impl Verification {
    /// A clean pass.
    pub(crate) fn passed(kind: VerificationKind) -> Self {
        Self {
            kind,
            status: VerificationStatus::Passed,
            detail: None,
        }
    }

    /// An explicit failure.
    pub(crate) fn failed(kind: VerificationKind, detail: VerifyDetail) -> Self {
        Self {
            kind,
            status: VerificationStatus::Failed,
            detail: Some(detail),
        }
    }

    /// This build could not verify the target.
    pub(crate) fn unsupported(kind: VerificationKind, detail: VerifyDetail) -> Self {
        Self {
            kind,
            status: VerificationStatus::Unsupported,
            detail: Some(detail),
        }
    }

    /// Verification was turned off.
    pub(crate) fn skipped(kind: VerificationKind, detail: VerifyDetail) -> Self {
        Self {
            kind,
            status: VerificationStatus::Skipped,
            detail: Some(detail),
        }
    }
}

/// Everything one answer's citations concluded.
///
/// `citations` has one entry per input citation, in input order. `integrity`
/// has one entry per distinct `(source_key, revision_hash)` named by those
/// citations, also in first-seen order — two citations of the same revision
/// are one fact about the store, not two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerVerdict {
    /// One verdict per citation, in input order.
    pub citations: Vec<Verification>,
    /// One verdict per distinct source revision named by those citations.
    pub integrity: Vec<Verification>,
}

impl AnswerVerdict {
    /// Whether the answer's evidence holds up.
    ///
    /// Four conditions, all of them required:
    ///
    /// 1. there is at least one citation — "verified" is a positive claim,
    ///    and the same rule applies to sparks: "Empty citations make the card
    ///    `unverified`";
    /// 2. every citation reported `passed` — an `unsupported` or `skipped`
    ///    citation has not been verified, and an answer with a failed
    ///    citation is not fully verified;
    /// 3. no integrity check reported `failed` — bytes that are gone make the
    ///    answer unverifiable even though its citations still resolve against
    ///    the stored chunk text;
    /// 4. no integrity check reported `source_bytes_unavailable` — a read that
    ///    was attempted and did not run means the answer was never checked,
    ///    whatever stopped it (an offline server, a permission, a path shape
    ///    the OS refused).
    ///
    /// The two `unsupported` states that do **not** veto are the two where
    /// nothing was ever going to be read:
    ///
    /// * `no_local_bytes` — no store row, no `local_path`, or a stored path
    ///   this crate *refuses* by form or by containment. Imports are a trust
    ///   boundary; a `local_path` that escapes the workspace
    ///   is bad data, and bad data is not evidence about the answer. The
    ///   citation itself still verified against the store.
    /// * `byte_hash_unchecked` — the bytes *were* read and only the hash
    ///   could not be recomputed.
    ///
    /// The distinction is evidence, not effort: a *missing* file is an
    /// evidence signal (the bytes are gone) and vetoes; a *refused* path is a
    /// data-quality signal and does not. For a paper source: *search,
    /// citations, and trace still work from chunk text; opening the PDF is a
    /// local `source_missing` / `source_integrity` failure.* That is a
    /// statement about the *citation*, not a licence to call an unread answer
    /// verified.
    ///
    /// Derived, never stored: there is one place this can be true or false.
    ///
    /// # Examples
    ///
    /// ```
    /// use momotaro_verify::{
    ///     AnswerVerdict, Verification, VerificationKind, VerificationStatus, VerifyDetail,
    /// };
    ///
    /// let passed = Verification {
    ///     kind: VerificationKind::Citation,
    ///     status: VerificationStatus::Passed,
    ///     detail: None,
    /// };
    /// let verified = AnswerVerdict {
    ///     citations: vec![passed],
    ///     integrity: Vec::new(),
    /// };
    /// assert!(verified.fully_verified());
    ///
    /// // No citations is no positive claim.
    /// let empty = AnswerVerdict {
    ///     citations: Vec::new(),
    ///     integrity: Vec::new(),
    /// };
    /// assert!(!empty.fully_verified());
    ///
    /// // A failed integrity check vetoes the answer even though every citation
    /// // still passed.
    /// let vetoed = AnswerVerdict {
    ///     citations: vec![passed],
    ///     integrity: vec![Verification {
    ///         kind: VerificationKind::SourceIntegrity,
    ///         status: VerificationStatus::Failed,
    ///         detail: Some(VerifyDetail::SourceMissing),
    ///     }],
    /// };
    /// assert!(!vetoed.fully_verified());
    /// ```
    pub fn fully_verified(&self) -> bool {
        !self.citations.is_empty()
            && self
                .citations
                .iter()
                .all(|v| v.status == VerificationStatus::Passed)
            && !self.integrity.iter().any(|v| {
                v.status == VerificationStatus::Failed
                    || v.detail == Some(VerifyDetail::SourceBytesUnavailable)
            })
    }
}
