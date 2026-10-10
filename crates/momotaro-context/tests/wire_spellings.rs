//! Wire-spelling witness table (database-schema B2 / test-suite F04).
//!
//! Every enum value that can land in a DDL TEXT column or the request wire
//! is pinned to its literal spelling here. A drift turns this file red —
//! and not only a drift in an existing value: each family iterates the
//! enum's `ALL` through a no-wildcard match, checks every variant lands in
//! the table, and pins the table's length against `ALL`, so a **new**
//! variant cannot skip this file and a lost row cannot hide.

use momotaro_context::{
    ContentKind, FragmentKind, HistoryRole, OmittedReason, QueryIntent, Role, Stage, Visibility,
};

#[test]
fn stage_names_match_the_contract_list() {
    let table = [
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
    ];
    for (stage, spelling) in table {
        assert_eq!(stage.as_str(), spelling);
    }
    // A no-wildcard match, the same guard `every_source_kind_is_in_all`
    // has: a new stage cannot be added to `as_str` alone.
    for stage in Stage::ALL {
        match stage {
            Stage::NormalizeQuery
            | Stage::LoadPolicy
            | Stage::SelectHistory
            | Stage::SelectEvidence
            | Stage::RenderFragments
            | Stage::Deduplicate
            | Stage::Rank
            | Stage::EnforceTokenBudget
            | Stage::RenderProviderMessages
            | Stage::ValidateCitations => {}
        }
        assert!(
            table.iter().any(|(s, _)| *s == stage),
            "{stage:?} is missing from the table"
        );
    }
    assert_eq!(
        table.len(),
        Stage::ALL.len(),
        "Stage grew or shrank: extend the table and ALL together, not just as_str"
    );
}

#[test]
fn omitted_reasons_match_the_registered_vocabulary() {
    let table = [
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
    ];
    for (reason, spelling) in table {
        assert_eq!(reason.as_str(), spelling);
    }
    for reason in OmittedReason::ALL {
        match reason {
            OmittedReason::BudgetExhausted
            | OmittedReason::DuplicateChunkHash
            | OmittedReason::PreviouslyInjected
            | OmittedReason::CoveredBySummary
            | OmittedReason::SummaryValidationFailed
            | OmittedReason::IncompleteCitation
            | OmittedReason::BeyondTierMax
            | OmittedReason::DelimiterViolation
            | OmittedReason::InvalidToolSchema => {}
        }
        assert!(
            table.iter().any(|(r, _)| *r == reason),
            "{reason:?} is missing from the table"
        );
    }
    assert_eq!(
        table.len(),
        OmittedReason::ALL.len(),
        "OmittedReason grew or shrank: extend the table and ALL together, not just as_str"
    );
}

#[test]
fn fragment_kinds_are_the_six_context_sources() {
    let table = [
        (FragmentKind::Policy, "policy"),
        (FragmentKind::ToolDefinitions, "tool_definitions"),
        (FragmentKind::History, "history"),
        (FragmentKind::Evidence, "evidence"),
        (FragmentKind::Instructions, "instructions"),
        (FragmentKind::Question, "question"),
    ];
    for (kind, spelling) in table {
        assert_eq!(kind.as_str(), spelling);
    }
    for kind in FragmentKind::ALL {
        match kind {
            FragmentKind::Policy
            | FragmentKind::ToolDefinitions
            | FragmentKind::History
            | FragmentKind::Evidence
            | FragmentKind::Instructions
            | FragmentKind::Question => {}
        }
        assert!(
            table.iter().any(|(k, _)| *k == kind),
            "{kind:?} is missing from the table"
        );
    }
    assert_eq!(
        table.len(),
        FragmentKind::ALL.len(),
        "FragmentKind grew or shrank: extend the table and ALL together, not just as_str"
    );
}

#[test]
fn roles_content_kinds_and_visibility_keep_their_spellings() {
    let roles = [
        (Role::System, "system"),
        (Role::User, "user"),
        (Role::Assistant, "assistant"),
    ];
    for (role, spelling) in roles {
        assert_eq!(role.as_str(), spelling);
    }
    for role in Role::ALL {
        match role {
            Role::System | Role::User | Role::Assistant => {}
        }
        assert!(roles.iter().any(|(r, _)| *r == role), "{role:?} missing");
    }
    assert_eq!(roles.len(), Role::ALL.len(), "Role grew or shrank");

    let history_roles = [
        (HistoryRole::User, "user"),
        (HistoryRole::Assistant, "assistant"),
    ];
    for (role, spelling) in history_roles {
        assert_eq!(role.as_str(), spelling);
    }
    for role in HistoryRole::ALL {
        match role {
            HistoryRole::User | HistoryRole::Assistant => {}
        }
        assert!(
            history_roles.iter().any(|(r, _)| *r == role),
            "{role:?} missing"
        );
    }
    assert_eq!(
        history_roles.len(),
        HistoryRole::ALL.len(),
        "HistoryRole grew or shrank"
    );

    let kinds = [
        (ContentKind::Text, "text"),
        (ContentKind::SummaryV1, "summary.v1"),
    ];
    for (kind, spelling) in kinds {
        assert_eq!(kind.as_str(), spelling);
    }
    for kind in ContentKind::ALL {
        match kind {
            ContentKind::Text | ContentKind::SummaryV1 => {}
        }
        assert!(kinds.iter().any(|(k, _)| *k == kind), "{kind:?} missing");
    }
    assert_eq!(
        kinds.len(),
        ContentKind::ALL.len(),
        "ContentKind grew or shrank"
    );

    let visibilities = [
        (Visibility::Provider, "provider"),
        // architecture §6.6 vocabulary: provider | trace_only.
        (Visibility::Trace, "trace_only"),
    ];
    for (visibility, spelling) in visibilities {
        assert_eq!(visibility.as_str(), spelling);
    }
    for visibility in Visibility::ALL {
        match visibility {
            Visibility::Provider | Visibility::Trace => {}
        }
        assert!(
            visibilities.iter().any(|(v, _)| *v == visibility),
            "{visibility:?} missing"
        );
    }
    assert_eq!(
        visibilities.len(),
        Visibility::ALL.len(),
        "Visibility grew or shrank"
    );
}

#[test]
fn query_intent_round_trips_and_rejects_unknown_spellings() {
    let table = [
        (QueryIntent::Fact, "fact"),
        (QueryIntent::Comparison, "comparison"),
        (QueryIntent::Survey, "survey"),
    ];
    for (intent, spelling) in table {
        assert_eq!(intent.as_str(), spelling);
        assert_eq!(spelling.parse::<QueryIntent>().unwrap(), intent);
    }
    for intent in QueryIntent::ALL {
        match intent {
            QueryIntent::Fact | QueryIntent::Comparison | QueryIntent::Survey => {}
        }
        assert!(
            table.iter().any(|(i, _)| *i == intent),
            "{intent:?} is missing from the table"
        );
    }
    assert_eq!(
        table.len(),
        QueryIntent::ALL.len(),
        "QueryIntent grew or shrank: extend the table and ALL together"
    );
    assert!("survey ".parse::<QueryIntent>().is_err());
    assert!("Survey".parse::<QueryIntent>().is_err());
}
