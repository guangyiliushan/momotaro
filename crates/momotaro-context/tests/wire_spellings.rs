//! Wire-spelling witness table (database-schema B2 / test-suite F04).
//!
//! Every enum value that can land in a DDL TEXT column or the request wire
//! is pinned to its literal spelling here. A drift turns this file red.

use momotaro_context::{
    ContentKind, FragmentKind, HistoryRole, OmittedReason, QueryIntent, Role, Stage, Visibility,
};

#[test]
fn stage_names_match_the_contract_list() {
    for (stage, spelling) in [
        (Stage::NormalizeQuery, "normalize_query"),
        (Stage::LoadPolicy, "load_policy"),
        (Stage::SelectHistory, "select_history"),
        (Stage::SelectEvidence, "select_evidence"),
        (Stage::RenderFragments, "render_fragments"),
        (Stage::Deduplicate, "deduplicate"),
        (Stage::Rank, "rank"),
        (Stage::EnforceTokenBudget, "enforce_token_budget"),
        (Stage::RenderProviderMessages, "render_provider_messages"),
        (Stage::ValidateCitations, "validate_citations"),
    ] {
        assert_eq!(stage.as_str(), spelling);
    }
}

#[test]
fn omitted_reasons_match_the_registered_vocabulary() {
    for (reason, spelling) in [
        (OmittedReason::BudgetExhausted, "budget_exhausted"),
        (OmittedReason::DuplicateChunkHash, "duplicate_chunk_hash"),
        (OmittedReason::PreviouslyInjected, "previously_injected"),
        (OmittedReason::CoveredBySummary, "covered_by_summary"),
        (
            OmittedReason::SummaryValidationFailed,
            "summary_validation_failed",
        ),
        (OmittedReason::IncompleteCitation, "incomplete_citation"),
        (OmittedReason::BeyondTierMax, "beyond_tier_max"),
        (OmittedReason::DelimiterViolation, "delimiter_violation"),
        (OmittedReason::InvalidToolSchema, "invalid_tool_schema"),
    ] {
        assert_eq!(reason.as_str(), spelling);
    }
}

#[test]
fn fragment_kinds_are_the_six_context_sources() {
    for (kind, spelling) in [
        (FragmentKind::Policy, "policy"),
        (FragmentKind::ToolDefinitions, "tool_definitions"),
        (FragmentKind::History, "history"),
        (FragmentKind::Evidence, "evidence"),
        (FragmentKind::Instructions, "instructions"),
        (FragmentKind::Question, "question"),
    ] {
        assert_eq!(kind.as_str(), spelling);
    }
}

#[test]
fn roles_content_kinds_and_visibility_keep_their_spellings() {
    assert_eq!(Role::System.as_str(), "system");
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(Role::Assistant.as_str(), "assistant");
    assert_eq!(HistoryRole::User.as_str(), "user");
    assert_eq!(HistoryRole::Assistant.as_str(), "assistant");
    assert_eq!(ContentKind::Text.as_str(), "text");
    assert_eq!(ContentKind::SummaryV1.as_str(), "summary.v1");
    assert_eq!(Visibility::Provider.as_str(), "provider");
    // architecture §6.6 vocabulary: provider | trace_only.
    assert_eq!(Visibility::Trace.as_str(), "trace_only");
}

#[test]
fn query_intent_round_trips_and_rejects_unknown_spellings() {
    for intent in [
        QueryIntent::Fact,
        QueryIntent::Comparison,
        QueryIntent::Survey,
    ] {
        let spelling = intent.as_str();
        assert_eq!(spelling.parse::<QueryIntent>().unwrap(), intent);
    }
    assert_eq!(QueryIntent::Fact.as_str(), "fact");
    assert_eq!(QueryIntent::Comparison.as_str(), "comparison");
    assert_eq!(QueryIntent::Survey.as_str(), "survey");
    assert!("survey ".parse::<QueryIntent>().is_err());
    assert!("Survey".parse::<QueryIntent>().is_err());
}
