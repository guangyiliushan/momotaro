//! Budget and compaction knobs (`docs/docs/context-retrieval.md` §2, §2.1, §4;
//! `architecture.md` §12.2).
//!
//! **The numeric constants here are「假设 + 观测闭环」parameters, not settled
//! science.** They are read from configuration and annotated as calibratable;
//! this crate never tweaks their values:
//!
//! - `token_estimate_coefficient = 1.25` (§2.1 可校准参数 1): engineering-
//!   spectrum low end (drift 5–15%, incident catalog 2.0×, common 1.5×).
//!   Calibration loop: read the P95 underestimate rate back from provider
//!   usage; CJK-heavy libraries should raise it toward 1.4–1.5.
//! - `max_context_tokens = 8192` (§2.1 可校准参数 2): 25% of a 32k window,
//!   inside the RAG performance peak zone.
//! - `compact_trigger_ratio = 0.85` and `compact_reserve_abs = 12000` (§4
//!   裁决): industry-aligned defaults; 0.40 stays a quality experiment tier
//!   to be re-evaluated against the three observation metrics (每千轮压缩
//!   次数、摘要 token 均值、压缩后首轮引用验证通过率).
//!
//! `adjusted_total = ceil(raw_total * coefficient)` (§12.2); estimation is
//! for trimming, provider usage is for cost — the two never mix.

/// The budget/compaction configuration for one build.
///
/// All four knobs are configuration values with the documented defaults;
/// see the module docs for their calibration status.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetPolicy {
    /// Ceiling for the adjusted estimate (§2.1 默认 8192，**待校准**).
    pub max_context_tokens: u32,
    /// Estimation safety factor applied as `ceil(raw * coefficient)`
    /// (§2.1 默认 1.25，**待校准**; CJK-heavy 1.4–1.5).
    pub token_estimate_coefficient: f64,
    /// Compaction trigger ratio (§4 默认 0.85，**待校准**; 0.40 is the
    /// quality experiment tier).
    pub compact_trigger_ratio: f64,
    /// Absolute compaction reserve, scaled down for small budgets as
    /// `min(compact_reserve_abs, 25% × max_context_tokens)`
    /// (§4 默认 12000，**待校准**).
    pub compact_reserve_abs: u32,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            token_estimate_coefficient: DEFAULT_TOKEN_ESTIMATE_COEFFICIENT,
            compact_trigger_ratio: DEFAULT_COMPACT_TRIGGER_RATIO,
            compact_reserve_abs: DEFAULT_COMPACT_RESERVE_ABS,
        }
    }
}

/// §2.1: 8192 = 25% of a 32k window. Calibratable.
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 8192;

/// §2.1: engineering-spectrum low end. Calibratable.
pub const DEFAULT_TOKEN_ESTIMATE_COEFFICIENT: f64 = 1.25;

/// §4 裁决: industry-aligned default. Calibratable.
pub const DEFAULT_COMPACT_TRIGGER_RATIO: f64 = 0.85;

/// §4: absorbs tool-output bursts. Calibratable.
pub const DEFAULT_COMPACT_RESERVE_ABS: u32 = 12_000;

impl BudgetPolicy {
    /// `adjusted_total = ceil(raw_total * coefficient)` (§12.2).
    ///
    /// A non-finite or non-positive coefficient from corrupted config fails
    /// **closed** to [`DEFAULT_TOKEN_ESTIMATE_COEFFICIENT`] — config garbage
    /// must never open the budget.
    pub fn adjusted_total(&self, raw_total: u64) -> u64 {
        let coefficient = if self.token_estimate_coefficient.is_finite()
            && self.token_estimate_coefficient > 0.0
        {
            self.token_estimate_coefficient
        } else {
            DEFAULT_TOKEN_ESTIMATE_COEFFICIENT
        };
        (raw_total as f64 * coefficient).ceil() as u64
    }

    /// The compaction reserve, scaled to the budget:
    /// `min(compact_reserve_abs, 25% × max_context_tokens)` (§4).
    pub fn compact_reserve(&self) -> u32 {
        self.compact_reserve_abs.min(self.max_context_tokens / 4)
    }

    /// The compaction trigger (§4):
    /// `min(max_context_tokens - reserve, ratio × max_context_tokens)`.
    ///
    /// Compaction fires when the session's estimated size crosses this
    /// line; the formula is a pure helper so the ask loop and this crate
    /// share one definition.
    pub fn compact_trigger(&self) -> u64 {
        let reserve = self.compact_reserve() as u64;
        let budget = u64::from(self.max_context_tokens);
        let by_ratio = (self.compact_trigger_ratio * self.max_context_tokens as f64).floor() as u64;
        (budget.saturating_sub(reserve)).min(by_ratio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_defaults_are_unchanged() {
        let policy = BudgetPolicy::default();
        assert_eq!(policy.max_context_tokens, 8192);
        assert_eq!(policy.token_estimate_coefficient, 1.25);
        assert_eq!(policy.compact_trigger_ratio, 0.85);
        assert_eq!(policy.compact_reserve_abs, 12_000);
    }

    #[test]
    fn adjusted_total_rounds_up() {
        let policy = BudgetPolicy::default();
        // ceil(10 * 1.25) = ceil(12.5) = 13.
        assert_eq!(policy.adjusted_total(10), 13);
        // ceil(4 * 1.25) = 5.
        assert_eq!(policy.adjusted_total(4), 5);
        assert_eq!(policy.adjusted_total(0), 0);
    }

    #[test]
    fn the_reserve_scales_down_with_the_budget() {
        // §4: 按预算缩放 min(12000, 25% × 预算).
        let big = BudgetPolicy {
            max_context_tokens: 100_000,
            ..BudgetPolicy::default()
        };
        assert_eq!(big.compact_reserve(), 12_000);
        // The default 8192 budget scales the reserve down to 25%.
        let small = BudgetPolicy::default();
        assert_eq!(small.compact_reserve(), 2048);
    }

    #[test]
    fn the_trigger_is_the_tighter_of_ratio_and_reserve() {
        // Default: min(8192 - 2048, floor(0.85 * 8192)) = min(6144, 6963).
        let policy = BudgetPolicy {
            max_context_tokens: 8192,
            ..BudgetPolicy::default()
        };
        assert_eq!(policy.compact_trigger(), 6144);
        // A large budget: min(100000 - 12000, 85000) = 85000.
        let big = BudgetPolicy {
            max_context_tokens: 100_000,
            ..BudgetPolicy::default()
        };
        assert_eq!(big.compact_trigger(), 85_000);
    }

    #[test]
    fn the_three_calibrated_knobs_come_from_config_not_code() {
        // Changing config values must flow through the math unchanged —
        // the formula must not hardcode 0.85 / 12000 / 1.25 anywhere.
        let custom = BudgetPolicy {
            max_context_tokens: 4000,
            token_estimate_coefficient: 1.4,
            compact_trigger_ratio: 0.40,
            compact_reserve_abs: 500,
        };
        assert_eq!(custom.adjusted_total(100), 140);
        assert_eq!(custom.compact_reserve(), 500);
        assert_eq!(custom.compact_trigger(), 1600);
    }
}

#[test]
fn a_corrupted_coefficient_fails_closed_to_the_default() {
    let corrupted = BudgetPolicy {
        token_estimate_coefficient: f64::NAN,
        ..BudgetPolicy::default()
    };
    assert_eq!(corrupted.adjusted_total(100), 125);
    let negative = BudgetPolicy {
        token_estimate_coefficient: -1.0,
        ..BudgetPolicy::default()
    };
    assert_eq!(negative.adjusted_total(100), 125);
}
