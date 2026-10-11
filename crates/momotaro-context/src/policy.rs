//! Budget and compaction knobs.
//!
//! **The numeric constants here are「假设 + 观测闭环」parameters, not settled
//! science.** They are read from configuration and annotated as calibratable;
//! this crate never tweaks their values:
//!
//! - `token_estimate_coefficient = 1.25` (可校准参数 1): engineering-
//!   spectrum low end (drift 5–15%, incident catalog 2.0×, common 1.5×).
//!   Calibration loop: read the P95 underestimate rate back from provider
//!   usage; CJK-heavy libraries should raise it toward 1.4–1.5.
//! - `max_context_tokens = 8192` (可校准参数 2): 25% of a 32k window,
//!   inside the RAG performance peak zone.
//! - `compact_trigger_ratio = 0.85` and `compact_reserve_abs = 12000`:
//!   industry-aligned defaults; 0.40 stays a quality experiment tier
//!   to be re-evaluated against the three observation metrics (每千轮压缩
//!   次数、摘要 token 均值、压缩后首轮引用验证通过率).
//!
//! `adjusted_total = ceil(raw_total * coefficient)`; estimation is
//! for trimming, provider usage is for cost — the two never mix.

/// The budget/compaction configuration for one build.
///
/// All four knobs are configuration values with the documented defaults;
/// see the module docs for their calibration status.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetPolicy {
    /// Ceiling for the adjusted estimate (默认 8192，**待校准**).
    pub max_context_tokens: u32,
    /// Estimation safety factor applied as `ceil(raw * coefficient)`
    /// (默认 1.25，**待校准**; CJK-heavy 1.4–1.5).
    pub token_estimate_coefficient: f64,
    /// Compaction trigger ratio (默认 0.85，**待校准**; 0.40 is the
    /// quality experiment tier).
    pub compact_trigger_ratio: f64,
    /// Absolute compaction reserve, scaled down for small budgets as
    /// `min(compact_reserve_abs, 25% × max_context_tokens)`
    /// (默认 12000，**待校准**).
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

/// 8192 = 25% of a 32k window. Calibratable.
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 8192;

/// Engineering-spectrum low end. Calibratable.
pub const DEFAULT_TOKEN_ESTIMATE_COEFFICIENT: f64 = 1.25;

/// Industry-aligned default. Calibratable.
pub const DEFAULT_COMPACT_TRIGGER_RATIO: f64 = 0.85;

/// Absorbs tool-output bursts. Calibratable.
pub const DEFAULT_COMPACT_RESERVE_ABS: u32 = 12_000;

impl BudgetPolicy {
    /// `adjusted_total = ceil(raw_total * coefficient)`.
    ///
    /// A non-finite or non-positive coefficient from corrupted config fails
    /// **closed** to [`DEFAULT_TOKEN_ESTIMATE_COEFFICIENT`] — config garbage
    /// must never open the budget.
    ///
    /// # Examples
    ///
    /// ```
    /// use momotaro_context::BudgetPolicy;
    ///
    /// let mut policy = BudgetPolicy::default(); // coefficient 1.25
    /// assert_eq!(policy.adjusted_total(800), 1000);
    ///
    /// policy.token_estimate_coefficient = f64::NAN;
    /// assert_eq!(policy.adjusted_total(800), 1000); // fails closed
    /// ```
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
    /// `min(compact_reserve_abs, 25% × max_context_tokens)`.
    pub fn compact_reserve(&self) -> u32 {
        self.compact_reserve_abs.min(self.max_context_tokens / 4)
    }

    /// The compaction trigger:
    /// `min(max_context_tokens - reserve, ratio × max_context_tokens)`.
    ///
    /// Compaction fires when the session's estimated size crosses this
    /// line; the formula is a pure helper so the ask loop and this crate
    /// share one definition.
    ///
    /// A non-finite ratio, zero, or one past 1.0 (a typo'd `8.5`, a `NaN`
    /// from a hand-edited TOML) fails **closed** to
    /// [`DEFAULT_COMPACT_TRIGGER_RATIO`] — the same rule
    /// [`Self::adjusted_total`] applies to the coefficient, and for the same
    /// reason: `(NaN * budget) as u64` is `0`, which would put the trigger on
    /// the floor and report a compaction on every single turn. Config garbage
    /// must never open the trigger.
    ///
    /// # Examples
    ///
    /// ```
    /// use momotaro_context::BudgetPolicy;
    ///
    /// let policy = BudgetPolicy::default(); // 8192-token budget, ratio 0.85
    /// assert_eq!(policy.compact_reserve(), 2048); // 25% of the budget
    /// assert_eq!(policy.compact_trigger(), 6144); // budget minus the reserve
    /// ```
    pub fn compact_trigger(&self) -> u64 {
        let ratio = if self.compact_trigger_ratio.is_finite()
            && self.compact_trigger_ratio > 0.0
            && self.compact_trigger_ratio <= 1.0
        {
            self.compact_trigger_ratio
        } else {
            DEFAULT_COMPACT_TRIGGER_RATIO
        };
        let reserve = self.compact_reserve() as u64;
        let budget = u64::from(self.max_context_tokens);
        let by_ratio = (ratio * self.max_context_tokens as f64).floor() as u64;
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
        // 按预算缩放 min(12000, 25% × 预算).
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

    /// Config garbage fails closed to the documented default — the same rule
    /// [`BudgetPolicy::adjusted_total`] applies to the coefficient.
    ///
    /// Worth its own test because the failure is silent in the other
    /// direction: `(NaN * budget) as u64` is `0`, so a `NaN` ratio would put
    /// the trigger on the floor and report a compaction on **every** turn,
    /// while a `1.5` typo would suppress it entirely on a large budget.
    #[test]
    fn an_out_of_range_ratio_falls_back_to_the_documented_default() {
        let expected = BudgetPolicy {
            max_context_tokens: 100_000,
            ..BudgetPolicy::default()
        }
        .compact_trigger();
        for ratio in [f64::NAN, f64::INFINITY, -0.5, 0.0, 1.5] {
            let policy = BudgetPolicy {
                max_context_tokens: 100_000,
                compact_trigger_ratio: ratio,
                ..BudgetPolicy::default()
            };
            assert_eq!(
                policy.compact_trigger(),
                expected,
                "a ratio of {ratio:?} must not decide the trigger"
            );
        }
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
    // The doc says "non-finite **or non-positive**" and its twin test lists
    // `0.0` for the ratio — this one skipped it. A zero coefficient makes every
    // estimate round to 0, so the budget is never exceeded and compaction
    // never fires.
    let zero = BudgetPolicy {
        token_estimate_coefficient: 0.0,
        ..BudgetPolicy::default()
    };
    assert_eq!(zero.adjusted_total(100), 125);
}
