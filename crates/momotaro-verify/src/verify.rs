//! The judgement: one citation in, one [`Verification`] out.
//!
//! Judgements run in a fixed order and the first one that fires wins. The
//! order is part of the contract — the citation ID's existence in
//! `run_context_items` comes first — and a test pins which judgement answers
//! when more than one is broken.

use crate::citation::Citation;
use crate::error::VerifyError;
use crate::source::{BytesState, CitationSource};
use crate::status::{VerificationKind, VerifyDetail};
use crate::verdict::{AnswerVerdict, Verification};

/// Whether the judgements run at all.
///
/// This is what makes [`crate::status::VerificationStatus::Skipped`] reachable:
/// a state the vocabulary declares but no code path can produce would be a
/// contract with a dead member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyPolicy {
    enabled: bool,
}

impl VerifyPolicy {
    /// Run every judgement (the default).
    pub fn enabled() -> Self {
        Self { enabled: true }
    }

    /// Report every judgement as `skipped` without reading anything.
    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Whether judgements run.
    pub fn is_enabled(self) -> bool {
        self.enabled
    }
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self::enabled()
    }
}

/// Verifies one citation's five-tuple against the store.
///
/// Judgements run in this order and the first one that fires wins:
///
/// 1. a disabled policy reports `skipped` and touches nothing;
/// 2. the citation must name a context item this run actually handed the
///    model — one that does not is reported as `citation_invalid`;
/// 3. the chunk row must exist;
/// 4. `source_key + revision_hash + chunk_hash` must match it exactly;
/// 5. the locator must match it exactly (the five-tuple's fifth element);
/// 6. the quoted text must be locatable inside that chunk.
///
/// **Where the context-item evidence comes from.** `momotaro-store` exposes no read
/// for `run_context_items`, so the ids arrive as `context_item_ids`: the
/// caller — which has just written that table —
/// hands them over. This is a stub-first stand-in: the predicate is
/// real, it runs first, and it fails closed (an empty slice fails every
/// citation), but the *store-backed* read is a later task. Until it lands,
/// this function's contract is "checked against the ids you passed", not
/// "checked against the table".
///
/// A seam failure is `unsupported`, never `failed`: "I could not read the
/// store" is a different fact from "this citation is not in the store", and
/// collapsing them would report an outage as a fabricated citation.
///
/// Pure and idempotent: same inputs, same verdict, no I/O beyond what the
/// injected seam performs.
///
/// # Examples
///
/// One judgement against a store (the seam's only production implementor):
///
/// ```no_run
/// use momotaro_store::Store;
/// use momotaro_verify::verify::verify_citation;
/// use momotaro_verify::{Citation, StoreSource, VerifyPolicy};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let store = Store::open("ws/.momotaro/momotaro.db")?;
/// let source = StoreSource::new(&store, "ws");
/// let citation = Citation {
///     context_item_id: "f0001".to_owned(),
///     source_key: "note:notes/fourier.md".to_owned(),
///     revision_hash: "rev-1".to_owned(),
///     chunk_id: "chunk-1".to_owned(),
///     chunk_hash: "chash-1".to_owned(),
///     locator_json: r#"{"start_byte":0}"#.to_owned(),
///     quoted_text: "energy is conserved".to_owned(),
/// };
///
/// // The context item must be one this run handed the model, or the verdict is
/// // `failed` / `citation_invalid` before the store is touched.
/// let verdict = verify_citation(&source, VerifyPolicy::enabled(), &["f0001"], &citation);
/// println!("{:?}", verdict.status);
/// # Ok(())
/// # }
/// ```
pub fn verify_citation<S: CitationSource>(
    source: &S,
    policy: VerifyPolicy,
    context_item_ids: &[&str],
    citation: &Citation,
) -> Verification {
    if !policy.is_enabled() {
        return Verification::skipped(VerificationKind::Citation, VerifyDetail::DisabledByPolicy);
    }

    if !context_item_ids.contains(&citation.context_item_id.as_str()) {
        return Verification::failed(VerificationKind::Citation, VerifyDetail::CitationInvalid);
    }

    let chunk = match source.chunk(&citation.chunk_id) {
        Err(error) => {
            // The verdict vocabulary deliberately has no free-text slot; keep
            // the typed error's path/cause on the diagnostic channel instead
            // of throwing it away at the seam boundary.
            eprintln!("{}", unreadable_context("citation chunk", &error));
            return Verification::unsupported(
                VerificationKind::Citation,
                VerifyDetail::ChunkUnreadable,
            );
        }
        Ok(None) => {
            return Verification::failed(VerificationKind::Citation, VerifyDetail::ChunkMissing);
        }
        Ok(Some(chunk)) => chunk,
    };

    if citation.source_key != chunk.source_key
        || citation.revision_hash != chunk.revision_hash
        || citation.chunk_hash != chunk.chunk_hash
    {
        return Verification::failed(VerificationKind::Citation, VerifyDetail::HashMismatch);
    }

    if citation.locator_json != chunk.locator_json {
        return Verification::failed(VerificationKind::Citation, VerifyDetail::LocatorMismatch);
    }

    // `chunks.text` is a verbatim slice of the normalised source at the
    // chunk's own locator, so finding the quote here is what the locatability rule demands:
    // "引用文本必须能定位到原始 chunk". An empty quote has nothing to locate
    // and therefore cannot satisfy it.
    if citation.quoted_text.is_empty() || !chunk.text.contains(&citation.quoted_text) {
        return Verification::failed(VerificationKind::Citation, VerifyDetail::TextNotLocatable);
    }

    Verification::passed(VerificationKind::Citation)
}

/// Checks whether the bytes named by one source revision are intact.
///
/// Reported under its own `source_integrity` kind rather than folded into the
/// citation verdict, so that three documented facts stay true at once: a
/// citation whose file has been archived still verifies from the stored chunk
/// text, the loss is still reported as `source_missing` rather than
/// silently ignored, and a hash this build cannot recompute is
/// `unsupported` rather than `passed`.
///
/// A read that fails for any reason other than absence is `unsupported`, not
/// `failed`: "I could not open it" is not "it is not there".
///
/// The production seam recomputes the ingest-domain hash from the canonical
/// bytes; a generic seam that cannot do so reports `ByteHashUnchecked`, which
/// stays `unsupported` rather than being promoted to a pass. `skipped` is
/// reachable when the policy is off; it never reaches the seam.
pub fn check_source_integrity<S: CitationSource>(
    source: &S,
    policy: VerifyPolicy,
    source_key: &str,
    revision_hash: &str,
) -> Verification {
    if !policy.is_enabled() {
        return Verification::skipped(
            VerificationKind::SourceIntegrity,
            VerifyDetail::DisabledByPolicy,
        );
    }

    match source.source_bytes(source_key, revision_hash) {
        Err(error) => {
            eprintln!("{}", unreadable_context("source bytes", &error));
            Verification::unsupported(
                VerificationKind::SourceIntegrity,
                VerifyDetail::SourceBytesUnavailable,
            )
        }
        Ok(BytesState::Missing) => Verification::failed(
            VerificationKind::SourceIntegrity,
            VerifyDetail::SourceMissing,
        ),
        Ok(BytesState::Absent) => Verification::unsupported(
            VerificationKind::SourceIntegrity,
            VerifyDetail::NoLocalBytes,
        ),
        Ok(BytesState::Present { hash_matches: None }) => Verification::unsupported(
            VerificationKind::SourceIntegrity,
            VerifyDetail::ByteHashUnchecked,
        ),
        Ok(BytesState::Present {
            hash_matches: Some(true),
        }) => Verification::passed(VerificationKind::SourceIntegrity),
        Ok(BytesState::Present {
            hash_matches: Some(false),
        }) => Verification::failed(
            VerificationKind::SourceIntegrity,
            VerifyDetail::RevisionHashMismatch,
        ),
    }
}

/// Verifies every citation of one answer, plus the sources they name.
///
/// Pure and idempotent: same inputs, same [`AnswerVerdict`], and the seam is
/// read at most once per distinct `(source_key, revision_hash)`.
pub fn verify_answer<S: CitationSource>(
    source: &S,
    policy: VerifyPolicy,
    context_item_ids: &[&str],
    citations: &[Citation],
) -> AnswerVerdict {
    let verdicts = citations
        .iter()
        .map(|citation| verify_citation(source, policy, context_item_ids, citation))
        .collect();

    let mut integrity = Vec::new();
    for (index, citation) in citations.iter().enumerate() {
        let already_checked = citations[..index].iter().any(|earlier| {
            earlier.source_key == citation.source_key
                && earlier.revision_hash == citation.revision_hash
        });
        if already_checked {
            continue;
        }
        integrity.push(check_source_integrity(
            source,
            policy,
            &citation.source_key,
            &citation.revision_hash,
        ));
    }

    AnswerVerdict {
        citations: verdicts,
        integrity,
    }
}

/// Builds the diagnostic line for a seam failure without changing its verdict.
///
/// `VerifyError` already owns the useful context — the store failure, or the
/// path and I/O source — and `Verification` is intentionally a stable,
/// copyable verdict, so this is the one place the two meet.
fn unreadable_context(what: &str, error: &VerifyError) -> String {
    format!("momotaro-verify: {what} read failed: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::VerifyError;
    use crate::source::{BytesState, sealed};
    use crate::status::VerificationStatus;
    use momotaro_contracts::Chunk;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const LOCATOR: &str =
        r#"{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1,"truncated":false}"#;
    /// Same byte length as [`LOCATOR`], different content — a length-only
    /// comparison would let this through.
    const OTHER_LOCATOR: &str =
        r#"{"start_byte":1,"end_byte":10,"start_line":1,"end_line":1,"truncated":false}"#;

    #[derive(Default)]
    struct FakeSource {
        chunks: HashMap<String, Chunk>,
        bytes: HashMap<String, BytesState>,
        fail_chunk: bool,
        fail_bytes: bool,
        chunk_calls: RefCell<u32>,
        bytes_calls: RefCell<u32>,
    }

    impl sealed::Sealed for FakeSource {}

    impl CitationSource for FakeSource {
        fn chunk(&self, chunk_id: &str) -> Result<Option<Chunk>, VerifyError> {
            *self.chunk_calls.borrow_mut() += 1;
            if self.fail_chunk {
                return Err(VerifyError::Store(
                    momotaro_store::StoreError::InvalidSchemaVersion("boom".to_owned()),
                ));
            }
            Ok(self.chunks.get(chunk_id).cloned())
        }

        fn source_bytes(
            &self,
            source_key: &str,
            revision_hash: &str,
        ) -> Result<BytesState, VerifyError> {
            *self.bytes_calls.borrow_mut() += 1;
            if self.fail_bytes {
                return Err(VerifyError::Io {
                    path: "unreadable".into(),
                    source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                });
            }
            Ok(self
                .bytes
                .get(&format!("{source_key}\u{0}{revision_hash}"))
                .cloned()
                .unwrap_or(BytesState::Absent))
        }
    }

    fn chunk() -> Chunk {
        Chunk {
            chunk_id: "cid-1".to_owned(),
            source_key: "note:a.md".to_owned(),
            revision_hash: "rev-1".to_owned(),
            ordinal: 0,
            heading_path: None,
            text: "energy is conserved: the second law of thermodynamics holds".to_owned(),
            locator_json: LOCATOR.to_owned(),
            chunk_hash: "chash-1".to_owned(),
            token_estimate: 8,
        }
    }

    fn citation() -> Citation {
        Citation {
            context_item_id: "f0001".to_owned(),
            source_key: "note:a.md".to_owned(),
            revision_hash: "rev-1".to_owned(),
            chunk_id: "cid-1".to_owned(),
            chunk_hash: "chash-1".to_owned(),
            locator_json: LOCATOR.to_owned(),
            quoted_text: "the second law".to_owned(),
        }
    }

    fn source_with_chunk() -> FakeSource {
        let mut source = FakeSource::default();
        source.chunks.insert("cid-1".to_owned(), chunk());
        source
    }

    fn ids() -> [&'static str; 1] {
        ["f0001"]
    }

    fn check(source: &FakeSource, ids: &[&str], citation: &Citation) -> Verification {
        verify_citation(source, VerifyPolicy::enabled(), ids, citation)
    }

    fn assert_fails(v: &Verification, detail: VerifyDetail, what: &str) {
        assert_eq!(v.kind, VerificationKind::Citation, "{what}: wrong kind");
        assert_eq!(
            v.status,
            VerificationStatus::Failed,
            "{what}: expected failed"
        );
        assert_eq!(v.detail, Some(detail), "{what}: wrong detail");
    }

    #[test]
    fn a_five_tuple_that_matches_the_stored_chunk_passes() {
        let source = source_with_chunk();
        let v = check(&source, &ids(), &citation());
        assert_eq!(v.kind, VerificationKind::Citation);
        assert_eq!(
            v.status,
            VerificationStatus::Passed,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(
            v.detail, None,
            "a passing citation carries no failure detail"
        );
    }

    #[test]
    fn a_chunk_hash_of_the_same_length_but_different_bytes_fails() {
        // The mutation this kills: replace the equality with a length compare.
        let source = source_with_chunk();
        let mut cited = citation();
        cited.chunk_hash = "chash-9".to_owned();
        assert_eq!(
            cited.chunk_hash.len(),
            chunk().chunk_hash.len(),
            "fixture must be length-equal"
        );
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::HashMismatch,
            "chunk_hash",
        );
    }

    #[test]
    fn a_source_key_of_the_same_length_but_different_bytes_fails() {
        // Kills "compare only chunk_hash".
        let source = source_with_chunk();
        let mut cited = citation();
        cited.source_key = "note:b.md".to_owned();
        assert_eq!(cited.source_key.len(), chunk().source_key.len());
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::HashMismatch,
            "source_key",
        );
    }

    #[test]
    fn a_revision_hash_of_the_same_length_but_different_bytes_fails() {
        // Kills "compare only source_key + chunk_hash".
        let source = source_with_chunk();
        let mut cited = citation();
        cited.revision_hash = "rev-2".to_owned();
        assert_eq!(cited.revision_hash.len(), chunk().revision_hash.len());
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::HashMismatch,
            "revision_hash",
        );
    }

    #[test]
    fn a_locator_of_the_same_length_but_different_bytes_fails() {
        let source = source_with_chunk();
        let mut cited = citation();
        cited.locator_json = OTHER_LOCATOR.to_owned();
        assert_eq!(cited.locator_json.len(), LOCATOR.len());
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::LocatorMismatch,
            "locator",
        );
    }

    #[test]
    fn a_quote_that_lives_in_another_chunk_is_not_locatable_here() {
        let mut source = source_with_chunk();
        let mut other = chunk();
        other.chunk_id = "cid-2".to_owned();
        other.text = "an entirely different passage about entropy".to_owned();
        source.chunks.insert("cid-2".to_owned(), other);

        let mut cited = citation();
        cited.quoted_text = "an entirely different passage".to_owned();
        // The quote really is in the store — just not in the chunk cited.
        // Without this the second fixture row would be dead weight and the
        // name would describe a scenario the test never exercises.
        assert!(
            source.chunks["cid-2"].text.contains(&cited.quoted_text),
            "the fixture must place the quote in the other chunk"
        );
        assert!(cited.quoted_text.len() < chunk().text.len());
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::TextNotLocatable,
            "quote living in a different chunk",
        );
    }

    #[test]
    fn quoted_text_that_is_not_in_the_chunk_fails() {
        let source = source_with_chunk();
        let mut cited = citation();
        cited.quoted_text = "no such sentence".to_owned();
        assert!(
            cited.quoted_text.len() < chunk().text.len(),
            "fixture must be short enough for a length-only check to pass"
        );
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::TextNotLocatable,
            "quoted_text",
        );
    }

    #[test]
    fn quoted_text_that_is_empty_fails() {
        // The quoted text must be locatable in the chunk. Nothing to
        // locate cannot satisfy that, so it fails closed rather than passing
        // vacuously.
        let source = source_with_chunk();
        let mut cited = citation();
        cited.quoted_text = String::new();
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::TextNotLocatable,
            "empty",
        );
    }

    #[test]
    fn a_locator_mismatch_is_reported_even_when_the_quote_is_present() {
        // The two judgements are independent: a matching quote must not
        // rescue a wrong locator.
        let source = source_with_chunk();
        let mut cited = citation();
        cited.locator_json = OTHER_LOCATOR.to_owned();
        assert!(chunk().text.contains(&cited.quoted_text));
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::LocatorMismatch,
            "wrong locator, right quote",
        );
    }

    #[test]
    fn a_context_item_id_that_is_not_in_the_run_fails_before_the_chunk_is_looked_up() {
        let source = source_with_chunk();
        let mut cited = citation();
        cited.context_item_id = "f9999".to_owned();
        // The chunk row is also wrong, so the detail identifies *which*
        // judgement fired — and proves the context gate ran first.
        cited.chunk_hash = "chash-9".to_owned();
        let v = check(&source, &ids(), &cited);
        assert_fails(&v, VerifyDetail::CitationInvalid, "unknown context item");
        assert_eq!(
            *source.chunk_calls.borrow(),
            0,
            "the context gate must short-circuit before the store is touched"
        );
    }

    #[test]
    fn a_chunk_row_that_is_not_in_the_store_fails() {
        let source = FakeSource::default();
        assert_fails(
            &check(&source, &ids(), &citation()),
            VerifyDetail::ChunkMissing,
            "absent row",
        );
    }

    #[test]
    fn an_unreadable_store_is_unsupported_rather_than_failed() {
        // light-citation's non-negotiable rule: a transport/read failure is
        // never reported as "the cited fact does not exist".
        let source = FakeSource {
            fail_chunk: true,
            ..FakeSource::default()
        };
        let v = check(&source, &ids(), &citation());
        assert_eq!(v.kind, VerificationKind::Citation);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::ChunkUnreadable));
    }

    #[test]
    fn a_disabled_policy_skips_without_touching_the_store() {
        let source = source_with_chunk();
        let v = verify_citation(&source, VerifyPolicy::disabled(), &ids(), &citation());
        assert_eq!(v.kind, VerificationKind::Citation);
        assert_eq!(v.status, VerificationStatus::Skipped);
        assert_eq!(v.detail, Some(VerifyDetail::DisabledByPolicy));
        assert_eq!(
            *source.chunk_calls.borrow(),
            0,
            "skipped must not read the store"
        );
    }

    #[test]
    fn judging_the_same_input_twice_returns_the_same_verdict() {
        let source = source_with_chunk();
        let ids = ids();
        let citation = citation();
        let first = verify_citation(&source, VerifyPolicy::enabled(), &ids, &citation);
        let second = verify_citation(&source, VerifyPolicy::enabled(), &ids, &citation);
        assert_eq!(first, second, "the judgement must be idempotent");
    }

    // --- source integrity -------------------------------------------------

    fn source_with_bytes(state: BytesState) -> FakeSource {
        let mut source = FakeSource::default();
        source.bytes.insert("note:a.md\u{0}rev-1".to_owned(), state);
        source
    }

    fn integrity(source: &FakeSource) -> Verification {
        check_source_integrity(source, VerifyPolicy::enabled(), "note:a.md", "rev-1")
    }

    fn assert_integrity(v: &Verification, status: VerificationStatus, detail: VerifyDetail) {
        assert_eq!(v.kind, VerificationKind::SourceIntegrity, "wrong kind");
        assert_eq!(v.status, status, "detail: {:?}", v.detail);
        assert_eq!(v.detail, Some(detail), "wrong detail");
    }

    #[test]
    fn missing_bytes_report_source_integrity_failed() {
        let source = source_with_bytes(BytesState::Missing);
        assert_integrity(
            &integrity(&source),
            VerificationStatus::Failed,
            VerifyDetail::SourceMissing,
        );
    }

    #[test]
    fn present_but_unhashable_bytes_report_unsupported_never_passed() {
        // Source integrity means present *and* matching the hash. This crate
        // cannot recompute the hash, so it must not claim
        // the first half proves the whole thing.
        let source = source_with_bytes(BytesState::Present { hash_matches: None });
        assert_integrity(
            &integrity(&source),
            VerificationStatus::Unsupported,
            VerifyDetail::ByteHashUnchecked,
        );
    }

    #[test]
    fn present_and_matching_bytes_pass() {
        let source = source_with_bytes(BytesState::Present {
            hash_matches: Some(true),
        });
        let v = integrity(&source);
        assert_eq!(v.kind, VerificationKind::SourceIntegrity);
        assert_eq!(
            v.status,
            VerificationStatus::Passed,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(v.detail, None);
    }

    #[test]
    fn present_but_mismatching_bytes_fail() {
        let source = source_with_bytes(BytesState::Present {
            hash_matches: Some(false),
        });
        assert_integrity(
            &integrity(&source),
            VerificationStatus::Failed,
            VerifyDetail::RevisionHashMismatch,
        );
    }

    #[test]
    fn a_source_with_no_local_bytes_is_unsupported_rather_than_failed() {
        // A web or arXiv source has no local bytes; that is not a defect.
        let source = source_with_bytes(BytesState::Absent);
        assert_integrity(
            &integrity(&source),
            VerificationStatus::Unsupported,
            VerifyDetail::NoLocalBytes,
        );
    }

    #[test]
    fn a_source_that_cannot_be_read_is_unsupported_rather_than_missing() {
        // light-citation non-negotiable rule 2: never call a transport or
        // permission failure "this does not exist".
        let source = FakeSource {
            fail_bytes: true,
            ..FakeSource::default()
        };
        assert_integrity(
            &integrity(&source),
            VerificationStatus::Unsupported,
            VerifyDetail::SourceBytesUnavailable,
        );
    }

    #[test]
    fn a_disabled_policy_skips_the_integrity_check_without_reading() {
        let source = source_with_bytes(BytesState::Missing);
        let v = check_source_integrity(&source, VerifyPolicy::disabled(), "note:a.md", "rev-1");
        assert_eq!(v.status, VerificationStatus::Skipped);
        assert_eq!(v.detail, Some(VerifyDetail::DisabledByPolicy));
        assert_eq!(*source.bytes_calls.borrow(), 0, "skipped must not read");
    }

    #[test]
    fn judging_source_integrity_twice_returns_the_same_verdict() {
        let source = source_with_bytes(BytesState::Missing);
        assert_eq!(integrity(&source), integrity(&source));
    }

    // --- answer-level verdict --------------------------------------------

    fn verdict(source: &FakeSource, citations: &[Citation]) -> AnswerVerdict {
        verify_answer(source, VerifyPolicy::enabled(), &ids(), citations)
    }

    #[test]
    fn an_answer_whose_citations_all_pass_is_fully_verified() {
        let source = source_with_chunk();
        let v = verdict(&source, &[citation()]);
        assert_eq!(v.citations.len(), 1);
        assert!(v.fully_verified(), "expected fully verified");
    }

    #[test]
    fn a_single_failing_citation_makes_the_answer_not_fully_verified() {
        // The answer may still be output, but it is not
        // fully verified.
        let source = source_with_chunk();
        let mut broken = citation();
        broken.chunk_hash = "chash-9".to_owned();
        let v = verdict(&source, &[citation(), broken]);
        assert!(
            !v.fully_verified(),
            "one broken citation must veto the answer"
        );
        assert_eq!(v.citations.len(), 2, "both citations are still reported");
        assert_eq!(
            v.citations[1].status,
            VerificationStatus::Failed,
            "the order of the input is preserved"
        );
    }

    #[test]
    fn a_missing_bytes_integrity_failure_makes_the_answer_not_fully_verified() {
        // Old citations fail as source_missing / source_integrity, not as a
        // silent miss.
        let mut source = source_with_chunk();
        source
            .bytes
            .insert("note:a.md\u{0}rev-1".to_owned(), BytesState::Missing);
        let v = verdict(&source, &[citation()]);
        assert_eq!(
            v.citations[0].status,
            VerificationStatus::Passed,
            "the citation still verifies from chunk text"
        );
        assert!(
            !v.fully_verified(),
            "the integrity failure must veto the answer"
        );
    }

    #[test]
    fn bytes_whose_hash_cannot_be_recomputed_do_not_block_the_answer() {
        // The bytes *were* read; only the hash could not be recomputed. That
        // is an honest state of a checked answer, not a failed check.
        let mut source = source_with_chunk();
        source.bytes.insert(
            "note:a.md\u{0}rev-1".to_owned(),
            BytesState::Present { hash_matches: None },
        );
        let v = verdict(&source, &[citation()]);
        assert!(v.fully_verified());
        assert_eq!(v.integrity.len(), 1);
        assert_eq!(v.integrity[0].status, VerificationStatus::Unsupported);
    }

    #[test]
    fn an_answer_with_no_citations_is_not_fully_verified() {
        // "Verified" is a positive claim. The same rule applies to sparks:
        // "Empty citations make the card `unverified`."
        let source = source_with_chunk();
        let v = verdict(&source, &[]);
        assert!(v.citations.is_empty());
        assert!(v.integrity.is_empty());
        assert!(!v.fully_verified());
    }

    #[test]
    fn citations_of_one_revision_share_a_single_integrity_verdict() {
        let source = source_with_chunk();
        let mut second = citation();
        second.quoted_text = "thermodynamics holds".to_owned();
        let v = verdict(&source, &[citation(), second, citation()]);
        assert_eq!(v.citations.len(), 3);
        assert_eq!(
            v.integrity.len(),
            1,
            "three citations of one revision produce one integrity result"
        );
        assert_eq!(*source.bytes_calls.borrow(), 1, "the seam is read once");
    }

    #[test]
    fn judging_the_same_answer_twice_returns_the_same_verdict() {
        let source = source_with_chunk();
        let citations = [citation()];
        let first = verify_answer(&source, VerifyPolicy::enabled(), &ids(), &citations);
        let second = verify_answer(&source, VerifyPolicy::enabled(), &ids(), &citations);
        assert_eq!(
            first, second,
            "the whole answer judgement must be idempotent"
        );
    }

    #[test]
    fn an_answer_judged_under_a_disabled_policy_is_skipped_not_verified() {
        let source = source_with_chunk();
        let v = verify_answer(&source, VerifyPolicy::disabled(), &ids(), &[citation()]);
        assert!(
            v.citations
                .iter()
                .all(|c| c.status == VerificationStatus::Skipped)
        );
        assert!(
            v.integrity
                .iter()
                .all(|c| c.status == VerificationStatus::Skipped)
        );
        assert!(!v.fully_verified(), "skipped is not verified");
    }

    /// An `unsupported` citation has not been verified either — and the store
    /// refusing to read is production-reachable (`source.chunk()` → `Err`),
    /// so this is the case an outage would land in.
    #[test]
    fn an_unreadable_store_leaves_the_answer_not_fully_verified() {
        let source = FakeSource {
            fail_chunk: true,
            ..FakeSource::default()
        };
        let v = verdict(&source, &[citation()]);
        assert_eq!(v.citations[0].status, VerificationStatus::Unsupported);
        assert!(
            !v.fully_verified(),
            "an unsupported citation has not been verified"
        );
    }

    /// An integrity check that could not even *run* vetoes. This is the third
    /// kind of `unsupported`, and the one that must not be confused with the
    /// other two: `NoLocalBytes` is a source where nothing was ever going to
    /// be read — no store row, no `local_path`, or a stored path this crate
    /// refuses by form or containment (a web or arXiv source, whose
    /// citations still resolve from chunk text) — and `ByteHashUnchecked` is
    /// a check that
    /// ran to completion with a hash this build cannot recompute. Both are
    /// honest states of a verified answer. A read that was attempted and
    /// failed is neither: the answer was never checked.
    #[test]
    fn an_integrity_check_that_could_not_run_vetoes_the_answer() {
        let mut source = source_with_chunk();
        source.fail_bytes = true;
        let v = verdict(&source, &[citation()]);
        assert_eq!(
            v.integrity[0].status,
            VerificationStatus::Unsupported,
            "report what is known: we could not read it"
        );
        assert_eq!(
            v.integrity[0].detail,
            Some(VerifyDetail::SourceBytesUnavailable)
        );
        assert!(
            !v.fully_verified(),
            "an answer whose integrity check never ran is not fully verified"
        );
    }

    /// The de-duplication key is `(source_key, revision_hash)` — not either
    /// half on its own. Two revisions of one source plus a second source are
    /// three separate facts about the store, and each must be asked about:
    /// dropping one would let a missing-bytes failure vanish, and
    /// `fully_verified()` would then return true on a source whose bytes are
    /// gone.
    #[test]
    fn integrity_gets_one_entry_per_distinct_source_revision() {
        let mut source = source_with_chunk();

        let mut later_revision = chunk();
        later_revision.chunk_id = "cid-2".to_owned();
        later_revision.revision_hash = "rev-2".to_owned();
        source.chunks.insert("cid-2".to_owned(), later_revision);

        let mut other_source = chunk();
        other_source.chunk_id = "cid-3".to_owned();
        other_source.source_key = "note:b.md".to_owned();
        source.chunks.insert("cid-3".to_owned(), other_source);

        let mut second_revision = citation();
        second_revision.revision_hash = "rev-2".to_owned();
        second_revision.chunk_id = "cid-2".to_owned();

        let mut second_source = citation();
        second_source.source_key = "note:b.md".to_owned();
        second_source.chunk_id = "cid-3".to_owned();

        // Give the three pairs three *different* byte states so the three
        // verdicts differ in detail (two of them share a status): first-seen
        // order is then observed rather than argued, and no permutation of
        // this list reads the same as the one asserted below.
        source
            .bytes
            .insert("note:a.md\u{0}rev-1".to_owned(), BytesState::Missing);
        source.bytes.insert(
            "note:b.md\u{0}rev-1".to_owned(),
            BytesState::Present { hash_matches: None },
        );
        // `(note:a.md, rev-2)` gets no entry at all and falls through to
        // `Absent`.

        let v = verdict(&source, &[citation(), second_revision, second_source]);
        assert_eq!(v.citations.len(), 3, "all three citations are reported");
        assert!(
            v.citations
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
            "each fixture row must match its citation for this to isolate the key"
        );
        assert_eq!(
            v.integrity.len(),
            3,
            "one entry per distinct (source_key, revision_hash)"
        );
        assert_eq!(
            *source.bytes_calls.borrow(),
            3,
            "the seam is read once per distinct pair"
        );
        assert_eq!(
            v.integrity[0].status,
            VerificationStatus::Failed,
            "first-seen order: the first pair asked about comes first"
        );
        assert_eq!(
            v.integrity[0].detail,
            Some(VerifyDetail::SourceMissing),
            "first"
        );
        assert_eq!(
            v.integrity[1].status,
            VerificationStatus::Unsupported,
            "second"
        );
        assert_eq!(
            v.integrity[1].detail,
            Some(VerifyDetail::NoLocalBytes),
            "second"
        );
        assert_eq!(
            v.integrity[2].status,
            VerificationStatus::Unsupported,
            "third"
        );
        assert_eq!(
            v.integrity[2].detail,
            Some(VerifyDetail::ByteHashUnchecked),
            "third"
        );
        assert!(
            !v.fully_verified(),
            "a source whose bytes are gone vetoes the answer"
        );
    }

    /// The 先桩后实 seam must fail *closed*: a caller that hands over no ids
    /// at all — exactly the wiring mistake the stub invites — must not have
    /// every citation certify itself.
    #[test]
    fn an_empty_context_item_list_fails_every_citation() {
        let source = source_with_chunk();
        let v = verify_citation(&source, VerifyPolicy::enabled(), &[], &citation());
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::CitationInvalid));

        let answer = verify_answer(&source, VerifyPolicy::enabled(), &[], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an empty evidence set is not a verified answer"
        );
    }

    /// Two judgements broken at once: the hash comparison answers first.
    #[test]
    fn a_hash_mismatch_wins_over_a_locator_mismatch() {
        let source = source_with_chunk();
        let mut cited = citation();
        cited.chunk_hash = "chash-9".to_owned();
        cited.locator_json = OTHER_LOCATOR.to_owned();
        assert_fails(
            &check(&source, &ids(), &cited),
            VerifyDetail::HashMismatch,
            "hash and locator both broken",
        );
    }

    #[test]
    fn unreadable_chunk_context_keeps_the_store_failure() {
        let error = VerifyError::Store(momotaro_store::StoreError::InvalidSchemaVersion(
            "boom".to_owned(),
        ));
        let detail = unreadable_context("citation chunk", &error);
        assert!(detail.contains("citation chunk read failed"));
        assert!(detail.contains("store read failed"));
        assert!(detail.contains("boom"));
    }

    #[test]
    fn unreadable_source_context_keeps_the_path_and_io_cause() {
        let error = VerifyError::Io {
            path: "workspace/notes/a.md".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let detail = unreadable_context("source bytes", &error);
        assert!(detail.contains("source bytes read failed"));
        assert!(detail.contains("workspace/notes/a.md"));
        assert!(detail.contains("permission denied"));
    }
}
