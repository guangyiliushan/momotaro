//! Evidence selection and the three-layer ordering
//! (`docs/docs/context-retrieval.md` §1.1, §2, §2.1; `architecture.md` §12).
//!
//! Selection bounds (§2.1 evidence 兜底): the evidence count scales with the
//! budget (at most the largest count fitting in 50% of the total budget),
//! clamped to the intent tier (fact 3–5 / comparison 4–6 / survey 8–10) and
//! to the global floor 3 / ceiling 10. The floor is hard: however tight the
//! budget, P1 evidence is kept (count at least 3 when that many exist).
//! Items beyond the cap are dropped with an explicit reason — 不静默丢。
//!
//! Ordering (`context-retrieval.md` §1.1), in priority order:
//!
//! 1. **session 位置守恒**: a chunk that appeared earlier keeps its
//!    first-seen slot; new chunks append after;
//! 2. **同源原文序**: chunks of one source stay contiguous, in original
//!    text order (`ordinal`);
//! 3. **首尾摆放**: first-time sources order by importance — the
//!    highest-scored source first, the second-highest last, the rest in
//!    the middle by score.
//!
//! Retrieval scores decide *who gets in* (selection keeps retrieval order);
//! the three layers decide *how it is placed*. The side-grouping half of
//! the comparison tier (每侧 4–6) needs a side model that v0.3 does not
//! embed yet; the per-side range currently applies as the plain count
//! range (registered as a known remaining surface).

use crate::types::{ContextInput, Omission, OmittedReason, QueryIntent};

use crate::budget::EVIDENCE_FLOOR;

/// Global upper bound of the evidence count (§2.1 上界 10).
pub const EVIDENCE_CEILING: usize = 10;

/// The evidence section may use at most this share of the total budget
/// (§2.1 不超过总预算 50%).
pub const EVIDENCE_BUDGET_SHARE: f64 = 0.5;

/// Intent-tier count bounds (§2.1). The comparison range is the per-side
/// range used as the plain count range until sides are modeled.
pub fn tier_bounds(intent: QueryIntent) -> (usize, usize) {
    match intent {
        QueryIntent::Fact => (3, 5),
        QueryIntent::Comparison => (4, 6),
        QueryIntent::Survey => (8, 10),
    }
}

/// What the selection stage kept, and what it dropped with which reason.
pub(crate) struct SelectionOutcome {
    /// Indices into `input.evidence`, in retrieval order.
    pub kept: Vec<usize>,
    /// Explicit drops.
    pub omissions: Vec<Omission>,
}

/// Select evidence: intent tier top-k with the budget-linked cap and the
/// hard P1 floor. Selection keeps retrieval order (hit rank ascending).
pub(crate) fn select_evidence(input: &ContextInput) -> SelectionOutcome {
    let mut candidates: Vec<usize> = (0..input.evidence.len()).collect();
    candidates.sort_by_key(|&i| {
        (
            input.evidence[i].hit.rank,
            input.evidence[i].hit.chunk_hash.clone(),
        )
    });

    // The tier band is the target range; under a tight budget the
    // contract-literal floor is the global 3 (§2.1 下界 3, 预算再紧也保留),
    // not the tier minimum — `tier_bounds` documents the band.
    let (_tier_min, tier_max) = tier_bounds(input.intent);

    // Budget-linked cap: the largest count whose adjusted tokens fit in
    // the evidence share of the budget.
    let share = input.budget.max_context_tokens as f64 * EVIDENCE_BUDGET_SHARE;
    let mut fitted = 0usize;
    let mut used = 0f64;
    for &i in &candidates {
        let item_tokens = u64::from(momotaro_contracts::estimate_tokens(&input.evidence[i].text));
        // One formula only: `BudgetPolicy::adjusted_total` (no hand-copied
        // calibration math — six-lens P1).
        let adjusted = input.budget.adjusted_total(item_tokens) as f64;
        if used + adjusted > share {
            break;
        }
        used += adjusted;
        fitted += 1;
    }

    // k = min(budget share, tier max, ceiling), then the hard floor wins
    // over a tight budget (预算再紧也保留 P1 的证据).
    let mut k = fitted.min(tier_max).min(EVIDENCE_CEILING);
    k = k.max(EVIDENCE_FLOOR);
    k = k.min(candidates.len());

    let kept: Vec<usize> = candidates.iter().take(k).copied().collect();
    let mut omissions = Vec::new();
    for (position, &i) in candidates.iter().enumerate() {
        if position < k {
            continue;
        }
        // Distinguish the binding constraint per item: anything within the
        // tier max but past k was cut by the budget share; beyond the tier
        // max is a tier cut.
        let reason = if position < tier_max {
            OmittedReason::BudgetExhausted
        } else {
            OmittedReason::BeyondTierMax
        };
        omissions.push(Omission {
            item: input.evidence[i].hit.chunk_hash.clone(),
            reason,
        });
    }
    SelectionOutcome { kept, omissions }
}

/// The three-layer order (§1.1) over the selected evidence.
///
/// Returns indices into `input.evidence`, in final render order.
pub(crate) fn order_evidence(input: &ContextInput, selected: &[usize]) -> Vec<usize> {
    let first_seen = &input.session.first_seen_order;

    let (mut seen, mut fresh): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    for &i in selected {
        if first_seen.contains(&input.evidence[i].hit.chunk_hash) {
            seen.push(i);
        } else {
            fresh.push(i);
        }
    }

    // Layer 1: seen chunks keep their session relative order.
    seen.sort_by_key(|&i| {
        first_seen
            .iter()
            .position(|h| h == &input.evidence[i].hit.chunk_hash)
            .unwrap_or(usize::MAX)
    });

    // Layer 2: fresh chunks group per source, contiguous, ordinal ascending.
    let mut source_keys: Vec<String> = Vec::new();
    for &i in &fresh {
        let key = input.evidence[i].hit.source_key.clone();
        if !source_keys.contains(&key) {
            source_keys.push(key);
        }
    }
    let mut groups: Vec<(f64, String, Vec<usize>)> = source_keys
        .into_iter()
        .map(|key| {
            let mut members: Vec<usize> = fresh
                .iter()
                .copied()
                .filter(|&i| input.evidence[i].hit.source_key == key)
                .collect();
            members.sort_by_key(|&i| {
                (
                    input.evidence[i].hit.ordinal,
                    input.evidence[i].hit.chunk_hash.clone(),
                )
            });
            let best = members
                .iter()
                .map(|&i| input.evidence[i].hit.score)
                .fold(f64::NEG_INFINITY, f64::max);
            (best, key, members)
        })
        .collect();

    // Layer 3: highest-scored source first, second-highest last, the rest
    // in the middle by score (descending).
    groups.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut fresh_order: Vec<usize> = Vec::with_capacity(fresh.len());
    if let Some(first) = groups.first() {
        fresh_order.extend(&first.2);
    }
    if groups.len() >= 3 {
        for group in &groups[2..] {
            fresh_order.extend(&group.2);
        }
    }
    if groups.len() >= 2 {
        fresh_order.extend(&groups[1].2);
    }

    // Layer 1 dominates: new chunks append after the seen ones.
    seen.extend(fresh_order);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EvidenceItem, SessionState};
    use momotaro_contracts::{OriginClass, RetrievalHit};

    fn item(chunk_hash: &str, source_key: &str, ordinal: u32, score: f64) -> EvidenceItem {
        EvidenceItem {
            hit: RetrievalHit {
                rank: ordinal,
                source_key: source_key.to_string(),
                revision_hash: "rev".to_string(),
                chunk_id: format!("c-{chunk_hash}"),
                chunk_hash: chunk_hash.to_string(),
                ordinal,
                heading_path: None,
                score,
                locator_json: "{}".to_string(),
            },
            text: format!("text of {chunk_hash}"),
            origin_class: OriginClass::Owner,
            title: None,
        }
    }

    fn input_with(evidence: Vec<EvidenceItem>, session: SessionState) -> ContextInput {
        ContextInput {
            query: "q".to_string(),
            intent: QueryIntent::Fact,
            policy_text: "p".to_string(),
            tool_definitions: Vec::new(),
            instructions: "i".to_string(),
            history: Vec::new(),
            evidence,
            session,
            budget: crate::policy::BudgetPolicy::default(),
        }
    }

    fn order_hashes(input: &ContextInput) -> Vec<String> {
        let selection = select_evidence(input);
        order_evidence(input, &selection.kept)
            .into_iter()
            .map(|i| input.evidence[i].hit.chunk_hash.clone())
            .collect()
    }

    #[test]
    fn the_tier_bounds_match_the_contract_table() {
        assert_eq!(tier_bounds(QueryIntent::Fact), (3, 5));
        assert_eq!(tier_bounds(QueryIntent::Comparison), (4, 6));
        assert_eq!(tier_bounds(QueryIntent::Survey), (8, 10));
    }

    #[test]
    fn the_best_source_leads_the_second_best_trails_and_the_rest_sit_in_the_middle() {
        // §1.1 layer 3: 最高分源置首、次高置尾、其余居中.
        let input = input_with(
            vec![
                item("h1", "note:a.md", 1, 1.0),
                item("h2", "note:b.md", 1, 5.0),
                item("h3", "note:c.md", 1, 3.0),
            ],
            SessionState::default(),
        );
        // b (5.0) first; c (3.0) last (second-highest); a (1.0) in between.
        assert_eq!(order_hashes(&input), vec!["h2", "h1", "h3"]);
    }

    #[test]
    fn same_source_chunks_stay_together_in_original_order() {
        // §1.1 layer 2, even when a middle-scored source exists.
        let input = input_with(
            vec![
                item("h1", "note:a.md", 2, 5.0),
                item("h2", "note:b.md", 1, 9.0),
                item("h3", "note:a.md", 1, 1.0),
            ],
            SessionState::default(),
        );
        // Sources: b (9.0) first; a (5.0) second-last (only two sources);
        // within a: ordinal order h3 (1), h1 (2).
        assert_eq!(order_hashes(&input), vec!["h2", "h3", "h1"]);
    }

    #[test]
    fn first_seen_positions_win_over_scores() {
        // §1.1 layer 1 over layer 3: the session has seen h1 and h2 (in
        // that order); a much higher-scored newcomer appends after.
        let session = SessionState {
            first_seen_order: vec!["h1".to_string(), "h2".to_string()],
            injected: Vec::new(),
            fragment_seq: 0,
        };
        let input = input_with(
            vec![
                item("h9", "note:z.md", 1, 99.0),
                item("h2", "note:b.md", 1, 5.0),
                item("h1", "note:a.md", 1, 1.0),
            ],
            session,
        );
        assert_eq!(order_hashes(&input), vec!["h1", "h2", "h9"]);
    }

    #[test]
    fn evidence_stops_claiming_after_the_first_failure() {
        // floor 3 + 第 4 条大到必败 + 第 5 条小到本可塞入:第 5 条同样显式
        // 省略——先到先得的「先」是申领顺序也是弃序。
        let mut input = input_with(
            vec![
                item("h1", "note:1.md", 1, 1.0),
                item("h2", "note:2.md", 1, 2.0),
                item("h3", "note:3.md", 1, 3.0),
                item("h4", "note:4.md", 1, 4.0),
                item("h5", "note:5.md", 1, 5.0),
            ],
            SessionState::default(),
        );
        input.evidence[3].text = "x".repeat(400);
        input.evidence[4].text = "tiny".to_string();
        input.budget.max_context_tokens = 20;
        let selection = select_evidence(&input);
        // floor 保住前三条;第 4 条预算失败;第 5 条不得插队。
        assert_eq!(selection.kept.len(), 3);
        assert!(
            selection
                .omissions
                .iter()
                .any(|o| o.item == "h4" && o.reason == OmittedReason::BudgetExhausted)
        );
        assert!(
            selection
                .omissions
                .iter()
                .any(|o| o.item == "h5" && o.reason == OmittedReason::BudgetExhausted)
        );
    }

    #[test]
    fn middle_sources_order_by_score_descending() {
        // 四源:最高(a=9)置首、次高(c=7)置尾、其余(b=5, d=6)居中且降序。
        let input = input_with(
            vec![
                item("hb", "note:b.md", 1, 5.0),
                item("ha", "note:a.md", 1, 9.0),
                item("hd", "note:d.md", 1, 6.0),
                item("hc", "note:c.md", 1, 7.0),
            ],
            SessionState::default(),
        );
        assert_eq!(order_hashes(&input), vec!["ha", "hd", "hb", "hc"]);
    }

    #[test]
    fn equal_scores_break_ties_by_source_key() {
        let input = input_with(
            vec![
                item("hz", "note:z.md", 1, 3.0),
                item("ha", "note:a.md", 1, 3.0),
            ],
            SessionState::default(),
        );
        assert_eq!(order_hashes(&input), vec!["ha", "hz"]);
    }

    #[test]
    fn the_evidence_floor_survives_a_tiny_budget() {
        // §2.1: 预算再紧也保留 P1 的证据,下界 3.
        let mut input = input_with(
            (1..=6)
                .map(|n| item(&format!("h{n}"), &format!("note:{n}.md"), n, 6.0 - n as f64))
                .collect(),
            SessionState::default(),
        );
        input.budget.max_context_tokens = 8;
        let selection = select_evidence(&input);
        assert_eq!(selection.kept.len(), EVIDENCE_FLOOR);
        assert_eq!(selection.omissions.len(), 3);
        // 归因逐条钉:先两条是预算截断,越过 tier_max 的那条才是档位截断。
        let reasons: Vec<OmittedReason> = selection.omissions.iter().map(|o| o.reason).collect();
        assert_eq!(
            reasons,
            vec![
                OmittedReason::BudgetExhausted,
                OmittedReason::BudgetExhausted,
                OmittedReason::BeyondTierMax,
            ]
        );
    }

    #[test]
    fn beyond_the_tier_max_items_are_dropped_with_explicit_reasons() {
        // fact tier max = 5; seven candidates ⇒ two explicit drops.
        let input = input_with(
            (1..=7)
                .map(|n| item(&format!("h{n}"), &format!("note:{n}.md"), n, 1.0))
                .collect(),
            SessionState::default(),
        );
        let selection = select_evidence(&input);
        assert_eq!(selection.kept.len(), 5);
        assert_eq!(selection.omissions.len(), 2);
        assert!(
            selection
                .omissions
                .iter()
                .all(|o| o.reason == OmittedReason::BeyondTierMax)
        );
    }
}
