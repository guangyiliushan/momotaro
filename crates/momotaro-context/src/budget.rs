//! The five-level token budget.
//!
//! `max_context_tokens` allocates by fixed priority, 先到先得不回填 — each
//! level claims budget in order and a truncated level is never revisited:
//!
//! | 顺序 | 类别 | 约束 |
//! |---|---|---|
//! | 1 | policy + tool definitions + constraints 常驻层 | 固定小预算，内容稳定 |
//! | 2 | question | 全量保留 |
//! | 3 | history | 近轮优先（保护带与摘要先裁过） |
//! | 4 | evidence | 意图分档 top-k，**兜底 floor 3** |
//! | 5 | instructions | 固定尾部 |
//!
//! Fixed levels (1, 2, 5) are never silently dropped: when they alone
//! exceed the budget the build still includes them and reports
//! `exceeded` (bounds are enforced by the caller's
//! configuration, not by discarding the question). Evidence keeps a hard
//! floor of [`EVIDENCE_FLOOR`] items — 预算再紧也保留 P1 的证据.
//!
//! History walks **recent-first** inside level 3: the recency guarantee
//! means the newest turns win the level's budget. Once one
//! claim fails, the rest of the level stops claiming — an older turn never
//! survives a dropped newer one — and every drop is an explicit
//! [`OmittedReason::BudgetExhausted`].

use crate::policy::BudgetPolicy;
use crate::types::{Omission, OmittedReason};

/// Global evidence floor (下界 3) — budget-proof.
pub const EVIDENCE_FLOOR: usize = 3;

/// The five allocation levels, in contract order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLevel {
    /// policy + tool definitions + constraints (resident).
    Resident = 1,
    /// The question (full).
    Question = 2,
    /// History replay (recent-first).
    History = 3,
    /// Evidence blocks (tiered top-k, floor 3).
    Evidence = 4,
    /// The fixed instruction tail.
    Instructions = 5,
}

/// One item presented to the budget stage.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetCandidate {
    /// Item key for the omission record.
    pub key: String,
    /// Which level claims it.
    pub level: BudgetLevel,
    /// Raw estimated tokens of its rendered body.
    pub raw_tokens: u64,
}

/// What the budget stage kept and dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetOutcome {
    /// Indices (into the candidate list) that survived, in input order.
    pub kept: Vec<usize>,
    /// Explicit drops, all [`OmittedReason::BudgetExhausted`].
    pub dropped: Vec<Omission>,
    /// The same drops by candidate slot (index into the candidate list), in
    /// the same order as `dropped`.
    ///
    /// Keys repeat across kinds — a `chunk_hash` can spell a `history_id` —
    /// so consumers that need the *object* (its tokens, its class) match by
    /// slot; first-match-by-key takes the wrong one when they collide
    /// (observed failures: a wrong `budget_delta`, and a kept turn's hashes released).
    pub dropped_slots: Vec<usize>,
}

/// Run the five-level allocation (先到先得不回填).
///
/// `candidates` arrive in render order; history candidates inside level 3
/// are walked most-recent-first by the caller (they keep render order in
/// `kept`).
pub(crate) fn enforce_token_budget(
    candidates: &[BudgetCandidate],
    history_walk_order: &[usize],
    budget: &BudgetPolicy,
) -> BudgetOutcome {
    let mut kept = vec![false; candidates.len()];
    let mut dropped = Vec::new();
    let mut dropped_slots: Vec<usize> = Vec::new();
    let mut deferred_drops: Vec<(usize, Omission)> = Vec::new();
    let mut running_raw: u64 = 0;

    // One claim against the running budget. `force` is for the fixed
    // levels and the evidence floor. Returns whether the claim succeeded.
    let mut claim = |index: usize, running_raw: &mut u64, force: bool| -> bool {
        let candidate = &candidates[index];
        let adjusted = budget.adjusted_total(*running_raw + candidate.raw_tokens);
        if force || adjusted <= u64::from(budget.max_context_tokens) {
            kept[index] = true;
            *running_raw += candidate.raw_tokens;
            true
        } else {
            dropped.push(Omission {
                item: candidate.key.clone(),
                reason: OmittedReason::BudgetExhausted,
            });
            dropped_slots.push(index);
            false
        }
    };

    // Level 1 + 2: fixed content, never dropped.
    for (index, candidate) in candidates.iter().enumerate() {
        if matches!(
            candidate.level,
            BudgetLevel::Resident | BudgetLevel::Question
        ) {
            claim(index, &mut running_raw, true);
        }
    }

    // Level 3: history, recent-first. Once a claim fails the budget is out
    // for this level: older turns never sneak past a dropped newer one
    // (近轮优先是申领顺序，也是弃序——older turns are the first to drop).
    let mut history_out = false;
    for &index in history_walk_order {
        if candidates[index].level != BudgetLevel::History {
            continue;
        }
        if history_out {
            deferred_drops.push((
                index,
                Omission {
                    item: candidates[index].key.clone(),
                    reason: OmittedReason::BudgetExhausted,
                },
            ));
            continue;
        }
        history_out = !claim(index, &mut running_raw, false);
    }

    // Level 4: evidence in ranked order; the first EVIDENCE_FLOOR items
    // are the hard floor (兜底), then the same stop-on-failure rule.
    let mut evidence_out = false;
    let mut evidence_seen = 0usize;
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.level != BudgetLevel::Evidence {
            continue;
        }
        let force = evidence_seen < EVIDENCE_FLOOR;
        evidence_seen += 1;
        if force {
            claim(index, &mut running_raw, true);
            continue;
        }
        if evidence_out {
            deferred_drops.push((
                index,
                Omission {
                    item: candidate.key.clone(),
                    reason: OmittedReason::BudgetExhausted,
                },
            ));
            continue;
        }
        evidence_out = !claim(index, &mut running_raw, false);
    }

    // Level 5: instructions, fixed tail.
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.level == BudgetLevel::Instructions {
            claim(index, &mut running_raw, true);
        }
    }
    // Deferred drops append in encounter order, exactly as before — now with
    // their slots travelling beside them.
    for (index, omission) in deferred_drops {
        dropped_slots.push(index);
        dropped.push(omission);
    }

    let kept = (0..candidates.len()).filter(|i| kept[*i]).collect();
    BudgetOutcome {
        kept,
        dropped,
        dropped_slots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::BudgetPolicy;

    fn candidate(key: &str, level: BudgetLevel, tokens: u64) -> BudgetCandidate {
        BudgetCandidate {
            key: key.to_string(),
            level,
            raw_tokens: tokens,
        }
    }

    #[test]
    fn the_five_levels_claim_budget_in_order_without_backfill() {
        // Question fits; the first history turn fits; the second does not;
        // instructions (level 5) still land because they are fixed — and
        // no budget is backfilled into the dropped history turn.
        let budget = BudgetPolicy {
            max_context_tokens: 100,
            ..BudgetPolicy::default()
        };
        let candidates = vec![
            candidate("policy", BudgetLevel::Resident, 10),
            candidate("question", BudgetLevel::Question, 10),
            candidate("hist-old", BudgetLevel::History, 40),
            candidate("hist-new", BudgetLevel::History, 40),
            candidate("instr", BudgetLevel::Instructions, 5),
        ];
        // Recent-first walk: hist-new claims first.
        let outcome = enforce_token_budget(&candidates, &[3, 2], &budget);
        assert_eq!(outcome.kept, vec![0, 1, 3, 4]);
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].item, "hist-old");
        assert_eq!(outcome.dropped[0].reason, OmittedReason::BudgetExhausted);
    }

    /// The boundary is `<=`, and it is on the keeping side: an estimate that
    /// lands **exactly** on the ceiling fits. The coefficient makes that a
    /// real point and not a rounding curiosity — `ceil(raw × 1.25) == 8192`
    /// first holds at raw 6553 (6553 × 1.25 = 8191.25), the one raw value
    /// that sits on the line. Flip the comparison to `<` and this item drops
    /// for being exactly the size the budget allows.
    #[test]
    fn an_estimate_that_lands_exactly_on_the_ceiling_still_fits() {
        let budget = BudgetPolicy {
            max_context_tokens: 8192,
            ..BudgetPolicy::default()
        };
        // History is not force-claimed, so the `<=` comparison is what
        // decides here. (Evidence past `EVIDENCE_FLOOR` claims the same way;
        // this level is simply the one with a walk order to drive.)
        let candidates = vec![candidate("hist", BudgetLevel::History, 6553)];
        let outcome = enforce_token_budget(&candidates, &[0], &budget);
        assert_eq!(outcome.kept, vec![0], "{:?}", outcome.dropped);
    }

    #[test]
    fn drops_report_their_slots_alongside_their_keys() {
        // The keys in `dropped` are what the stage record prints; the slots
        // are what accounting matches by — keys repeat across kinds, slots
        // do not.
        let budget = BudgetPolicy {
            max_context_tokens: 100,
            ..BudgetPolicy::default()
        };
        let candidates = vec![
            candidate("policy", BudgetLevel::Resident, 10),
            candidate("question", BudgetLevel::Question, 10),
            candidate("hist-old", BudgetLevel::History, 40),
            candidate("hist-new", BudgetLevel::History, 40),
            candidate("instr", BudgetLevel::Instructions, 5),
        ];
        let outcome = enforce_token_budget(&candidates, &[3, 2], &budget);
        assert_eq!(outcome.dropped[0].item, "hist-old");
        assert_eq!(outcome.dropped_slots, vec![2]);
    }

    #[test]
    fn deferred_drops_report_their_slots_in_encounter_order() {
        // The doc promises `dropped_slots` in `dropped`'s order; the single
        // drop above could not see an order change (adversarial lane #7).
        // The walk stops at the first failed claim, so the tail turns are
        // deferred — their slots must follow in encounter order.
        let budget = BudgetPolicy {
            max_context_tokens: 100,
            ..BudgetPolicy::default()
        };
        let candidates = vec![
            candidate("policy", BudgetLevel::Resident, 10),
            candidate("question", BudgetLevel::Question, 10),
            candidate("hist-c", BudgetLevel::History, 40),
            candidate("hist-b", BudgetLevel::History, 40),
            candidate("hist-a", BudgetLevel::History, 5),
            candidate("hist-z", BudgetLevel::History, 5),
            candidate("instr", BudgetLevel::Instructions, 5),
        ];
        let outcome = enforce_token_budget(&candidates, &[2, 3, 4, 5], &budget);
        assert_eq!(
            outcome.dropped_slots,
            vec![3, 4, 5],
            "claim drop first, then deferred drops in encounter order"
        );
        assert_eq!(
            outcome
                .dropped
                .iter()
                .map(|omission| omission.item.as_str())
                .collect::<Vec<_>>(),
            vec!["hist-b", "hist-a", "hist-z"]
        );
    }

    #[test]
    fn the_evidence_floor_survives_a_budget_that_nothing_fits_in() {
        let budget = BudgetPolicy {
            max_context_tokens: 1,
            ..BudgetPolicy::default()
        };
        let candidates = vec![
            candidate("e1", BudgetLevel::Evidence, 500),
            candidate("e2", BudgetLevel::Evidence, 500),
            candidate("e3", BudgetLevel::Evidence, 500),
            candidate("e4", BudgetLevel::Evidence, 500),
        ];
        let outcome = enforce_token_budget(&candidates, &[], &budget);
        // floor 3 keeps e1..e3; e4 is an explicit drop.
        assert_eq!(outcome.kept, vec![0, 1, 2]);
        assert_eq!(outcome.dropped.len(), 1);
        assert_eq!(outcome.dropped[0].item, "e4");
    }

    #[test]
    fn fixed_parts_are_reported_as_exceeded_not_silently_trimmed() {
        let budget = BudgetPolicy {
            max_context_tokens: 10,
            ..BudgetPolicy::default()
        };
        let candidates = vec![
            candidate("policy", BudgetLevel::Resident, 500),
            candidate("question", BudgetLevel::Question, 500),
            candidate("instr", BudgetLevel::Instructions, 500),
        ];
        let outcome = enforce_token_budget(&candidates, &[], &budget);
        assert_eq!(outcome.kept, vec![0, 1, 2]);
        let kept_raw: u64 = outcome.kept.iter().map(|&i| candidates[i].raw_tokens).sum();
        assert!(kept_raw > u64::from(budget.max_context_tokens));
        assert!(outcome.dropped.is_empty());
    }
}
