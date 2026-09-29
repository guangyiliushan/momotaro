//! Multi-round history selection (`docs/docs/context-retrieval.md` §3).
//!
//! The recency guard band keeps the **wider** of the last
//! [`HISTORY_GUARD_ROUNDS`] rounds and the most recent
//! [`HISTORY_GUARD_TOKENS`] tokens of turns (§3.1 保护带: 近 2 轮且 ≥8K
//! token 保底，取宽者). Guard-band turns are replayed raw as a floor
//! guarantee. Turns outside the band degrade to their `summary.v1` record
//! (§3.2): the raw turn is dropped as
//! [`OmittedReason::CoveredBySummary`] and the summary body is replayed
//! instead, so old evidence never re-enters as full text (§3.3 keeps only
//! title + pointer in `evidence_refs`).
//!
//! Fail-closed summary handling (§3.1 断言): a summary that fails
//! [`SummaryV1::validate_against_text`](crate::summary::SummaryV1::validate_against_text) is dropped
//! ([`OmittedReason::SummaryValidationFailed`]) and its raw turns fall
//! back — 放弃压缩、保留原史. A validation failure is never silent and
//! never partially applied.
//!
//! Bodies are replayed **verbatim** (`architecture.md` §6.5): `select`
//! only decides which bodies are in, never how they read.

use crate::types::{HistoryItem, Omission, OmittedReason};

/// The guard band keeps at least this many recent rounds (§3.1).
pub const HISTORY_GUARD_ROUNDS: u32 = 2;

/// The guard band keeps at least this many recent raw tokens (§3.1
/// ≥8K token 保底; the wider of the two bounds wins).
pub const HISTORY_GUARD_TOKENS: u32 = 8_000;

/// What the `select_history` stage kept, dropped, and which chunk hashes
/// the kept raw turns already carry.
pub(crate) struct HistorySelection {
    /// Indices into `input.history` that stay, in input order (raw turns
    /// and valid summary items alike).
    pub kept: Vec<usize>,
    /// Explicit drops.
    pub omissions: Vec<Omission>,
    /// `chunk_hash` values whose full text the kept raw turns already
    /// carry (input to the dedup stage).
    pub carried_chunk_hashes: Vec<String>,
}

/// The first round inside the guard band: rounds at or above it stay raw
/// (§3.1, wider of the two bounds).
pub fn guard_band_from_round(history: &[HistoryItem]) -> u32 {
    let max_round = history.iter().map(|item| item.round).max().unwrap_or(0);
    let last_rounds_start = max_round.saturating_sub(HISTORY_GUARD_ROUNDS - 1);

    // The token bound covers everything until the tail actually reaches
    // the floor, i.e. its start is the earliest round in the worst case.
    let mut tail_tokens: u64 = 0;
    let mut token_band_start = history.iter().map(|item| item.round).min().unwrap_or(0);
    for item in history.iter().rev() {
        tail_tokens += u64::from(momotaro_contracts::estimate_tokens(&item.body));
        if tail_tokens >= u64::from(HISTORY_GUARD_TOKENS) {
            token_band_start = item.round;
            break;
        }
    }
    last_rounds_start.min(token_band_start)
}

/// Select history: guard band floor, summary degradation, fail-closed
/// fallback.
pub(crate) fn select_history(history: &[HistoryItem]) -> HistorySelection {
    let mut omissions = Vec::new();

    // Pass 1: validate every summary record fail-closed against its stored
    // body. Invalid records are kept out of the coverage set entirely.
    let mut summary_ok: Vec<bool> = Vec::with_capacity(history.len());
    for item in history {
        let ok = match &item.summary {
            None => false,
            Some(record) => record.validate_against_text(&item.body).is_ok(),
        };
        if item.summary.is_some() && !ok {
            omissions.push(Omission {
                item: item.history_id.clone(),
                reason: OmittedReason::SummaryValidationFailed,
            });
        }
        summary_ok.push(ok);
    }

    // Turns covered by a *valid* summary degrade to it (§3.2).
    let mut covered: Vec<String> = Vec::new();
    for (item, ok) in history.iter().zip(&summary_ok) {
        if *ok && let Some(record) = &item.summary {
            covered.extend(record.covered_turns.iter().cloned());
        }
    }

    let guard_from_round = guard_band_from_round(history);

    // Pass 2: keep or drop.
    let mut kept = Vec::new();
    let mut carried_chunk_hashes = Vec::new();
    for (index, item) in history.iter().enumerate() {
        if item.summary.is_some() {
            if summary_ok[index] {
                kept.push(index);
            }
            continue;
        }
        let in_guard_band = item.round >= guard_from_round;
        if !in_guard_band && covered.contains(&item.history_id) {
            omissions.push(Omission {
                item: item.history_id.clone(),
                reason: OmittedReason::CoveredBySummary,
            });
            continue;
        }
        // Guard-band turns stay raw even if a summary claims them (保底
        // floor against a mis-cutting compressor); uncovered out-of-band
        // turns stay raw until compaction appends their summary (§4 只
        // 追加 summary.v1，不删不改 history_items).
        kept.push(index);
        carried_chunk_hashes.extend(item.referenced_chunk_hashes.iter().cloned());
    }

    HistorySelection {
        kept,
        omissions,
        carried_chunk_hashes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::{EvidenceRef, SUMMARY_VERSION_V1, SummaryV1, UserDecision};
    use crate::types::HistoryRole;

    fn turn(id: &str, round: u32, body: &str) -> HistoryItem {
        HistoryItem {
            history_id: id.to_string(),
            round,
            role: HistoryRole::User,
            body: body.to_string(),
            summary: None,
            referenced_chunk_hashes: Vec::new(),
        }
    }

    fn summary_record(covered: &[&str]) -> SummaryV1 {
        let mut record = SummaryV1 {
            version: SUMMARY_VERSION_V1.to_string(),
            covered_turns: covered.iter().map(|s| (*s).to_string()).collect(),
            first_kept_item_id: "h-keep".to_string(),
            tokens_before: 100,
            user_decisions: vec![UserDecision {
                decision: "d".to_string(),
                reason: "r".to_string(),
            }],
            open_questions: Vec::new(),
            evidence_refs: vec![EvidenceRef {
                chunk_hash: "cafe".to_string(),
                title: "t".to_string(),
                claim: "c".to_string(),
            }],
            next_action_hint: "n".to_string(),
        };
        record.normalize();
        record
    }

    fn summary_item(id: &str, round: u32, record: SummaryV1) -> HistoryItem {
        let stored = serde_json::to_string(&record).expect("encode");
        let mut item = turn(id, round, &stored);
        item.summary = Some(record);
        item
    }

    /// A turn big enough to push the 8K-token guard band forward on its
    /// own (32001 chars ⇒ 8001 estimated tokens).
    fn fat_body() -> String {
        "x".repeat(32_001)
    }

    #[test]
    fn recent_rounds_are_kept_raw() {
        let history = vec![
            turn("h-1", 1, "old one"),
            turn("h-2", 1, "old one a"),
            turn("h-3", 2, "recent"),
            turn("h-4", 2, "recent a"),
        ];
        let selection = select_history(&history);
        assert_eq!(selection.kept, vec![0, 1, 2, 3]);
        assert!(selection.omissions.is_empty());
    }

    #[test]
    fn a_valid_summary_swaps_its_out_of_band_turns_out() {
        // The covered turn sits before a fat turn that owns the token
        // bound, and outside the last-two-rounds bound.
        let record = summary_record(&["h-1"]);
        let history = vec![
            turn("h-1", 1, "old one"),
            turn("h-fat", 2, &fat_body()),
            turn("h-3", 3, "recent"),
            summary_item("h-sum", 4, record),
        ];
        let selection = select_history(&history);
        assert_eq!(selection.kept, vec![1, 2, 3]);
        let drops: Vec<(&str, OmittedReason)> = selection
            .omissions
            .iter()
            .map(|o| (o.item.as_str(), o.reason))
            .collect();
        assert_eq!(drops, vec![("h-1", OmittedReason::CoveredBySummary)]);
    }

    #[test]
    fn an_invalid_summary_falls_back_to_the_raw_history() {
        // The stored text does not carry the evidence hash verbatim ⇒
        // fail-closed: drop the summary, keep every raw turn.
        let record = summary_record(&["h-1"]);
        let mut item = summary_item("h-sum", 4, record);
        item.body = item.body.replace("cafe", "nope");
        let history = vec![turn("h-1", 1, "old one"), turn("h-2", 2, "also old"), item];
        let selection = select_history(&history);
        assert_eq!(selection.kept, vec![0, 1]);
        assert!(
            selection.omissions.iter().any(|o| {
                o.item == "h-sum" && o.reason == OmittedReason::SummaryValidationFailed
            })
        );
        assert!(
            !selection
                .omissions
                .iter()
                .any(|o| o.reason == OmittedReason::CoveredBySummary)
        );
    }

    #[test]
    fn a_guard_band_turn_stays_raw_even_when_a_summary_claims_it() {
        // 保底 floor: the band is a guarantee, coverage does not override it.
        let record = summary_record(&["h-3"]);
        let history = vec![
            turn("h-1", 1, "old"),
            turn("h-2", 2, "old too"),
            turn("h-3", 3, "recent"),
            summary_item("h-sum", 3, record),
        ];
        let selection = select_history(&history);
        assert!(selection.kept.contains(&2), "h-3 stays raw");
        assert!(selection.kept.contains(&3), "the summary stays too");
    }

    #[test]
    fn carried_hashes_come_only_from_kept_raw_turns() {
        let mut t1 = turn("h-1", 1, "old");
        t1.referenced_chunk_hashes = vec!["h1".to_string()];
        let record = summary_record(&["h-1"]);
        let history = vec![
            t1,
            turn("h-fat", 2, &fat_body()),
            turn("h-3", 3, "recent"),
            summary_item("h-sum", 4, record),
        ];
        let selection = select_history(&history);
        // h-1 is covered and dropped ⇒ its hash is no longer carried.
        assert!(selection.carried_chunk_hashes.is_empty());
    }

    #[test]
    fn the_guard_band_takes_the_wider_of_rounds_and_tokens() {
        // Short history: the token bound covers everything (≥8K 保底 not
        // yet reached ⇒ the band starts at the earliest round).
        let short = vec![
            turn("h-1", 1, "a"),
            turn("h-2", 2, "b"),
            turn("h-3", 3, "c"),
        ];
        assert_eq!(guard_band_from_round(&short), 1);

        // Round 10 alone owns the token bound; the two-round bound is
        // wider (starts earlier) and wins.
        let mut long: Vec<HistoryItem> = (1..=9).map(|n| turn(&format!("h-{n}"), n, "x")).collect();
        long.push(turn("h-10", 10, &fat_body()));
        assert_eq!(guard_band_from_round(&long), 9);
    }
}
