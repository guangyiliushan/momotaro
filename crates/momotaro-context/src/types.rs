//! Building blocks for [`crate::build_context`].
//!
//! Field spellings align with `docs/docs/architecture.md` §8.1
//! (`run_context_items`) and `docs/docs/context-retrieval.md` §3.1
//! (`summary.v1`); wire spellings of every enum are pinned by witness tests
//! in `tests/wire_spellings.rs` (one literal pair per variant).

use momotaro_contracts::{OriginClass, RetrievalHit};

use crate::policy::BudgetPolicy;
use crate::summary::SummaryV1;

/// Query intent, embedded as an enum now and routed later
/// (`docs/docs/context-retrieval.md` §4.1: 0.x embeds the enum, no branches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    /// Single-step factual lookup (default).
    Fact,
    /// Side-by-side comparison.
    Comparison,
    /// Broad survey.
    Survey,
}

impl QueryIntent {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 3] = [Self::Fact, Self::Comparison, Self::Survey];

    /// The wire spelling of this intent (`fact`, `comparison`, `survey`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Comparison => "comparison",
            Self::Survey => "survey",
        }
    }
}

impl std::str::FromStr for QueryIntent {
    type Err = UnknownWireSpelling;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fact" => Ok(Self::Fact),
            "comparison" => Ok(Self::Comparison),
            "survey" => Ok(Self::Survey),
            other => Err(UnknownWireSpelling(other.to_owned())),
        }
    }
}

/// The role a [`HistoryItem`] carries in the conversation replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    /// The user's turn.
    User,
    /// The assistant's turn.
    Assistant,
}

impl HistoryRole {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 2] = [Self::User, Self::Assistant];

    /// The wire spelling of this role.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// The role of a [`ProviderMessage`] as the provider sees it.
///
/// Retrieval evidence never renders as `system` (`architecture.md` §12.1:
/// 检索证据不是 system prompt); policy and tool definitions live in the
/// privileged system position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The privileged system position.
    System,
    /// A user message.
    User,
    /// An assistant message.
    Assistant,
}

impl Role {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 3] = [Self::System, Self::User, Self::Assistant];

    /// The wire spelling of this role.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Which of the six context sources a fragment carries
/// (`architecture.md` §12.2: `policy / question / history / evidence /
/// tool_definitions / instructions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentKind {
    /// The `policy.v1` resident text (constraints ride inside it).
    Policy,
    /// The canonical tool-definitions serialization.
    ToolDefinitions,
    /// A replayed history turn (or its `summary.v1` body).
    History,
    /// One retrieval-evidence block.
    Evidence,
    /// The fixed instruction tail.
    Instructions,
    /// The `question.v1` fragment, always last.
    Question,
}

impl FragmentKind {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 6] = [
        Self::Policy,
        Self::ToolDefinitions,
        Self::History,
        Self::Evidence,
        Self::Instructions,
        Self::Question,
    ];

    /// The wire spelling of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::ToolDefinitions => "tool_definitions",
            Self::History => "history",
            Self::Evidence => "evidence",
            Self::Instructions => "instructions",
            Self::Question => "question",
        }
    }
}

/// The content shape of a fragment body, for `run_context_items.content_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Verbatim text.
    Text,
    /// A `summary.v1` body (`context-retrieval.md` §3.1).
    SummaryV1,
}

impl ContentKind {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 2] = [Self::Text, Self::SummaryV1];

    /// The wire spelling of this content kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::SummaryV1 => "summary.v1",
        }
    }
}

/// Two-stage lowering gate (`architecture.md` §12): the provider only ever
/// receives `visibility = provider` fragments; the trace keeps them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Lowered into the provider request.
    Provider,
    /// Trace-only; never sent to the model.
    Trace,
}

impl Visibility {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 2] = [Self::Provider, Self::Trace];

    /// The wire spelling of this visibility (`architecture.md` §6.6:
    /// `visibility=provider|trace_only`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Trace => "trace_only",
        }
    }
}

/// A parse failure for a wire spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown wire spelling: {0:?}")]
pub struct UnknownWireSpelling(String);

/// One provider tool definition.
///
/// `parameters_json` must hold a JSON object; it is embedded verbatim into the
/// canonical tools serialization (field order fixed, list order = the caller's
/// — 序即身份, §1 第 3 条).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// Tool name (unique within one request).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// The JSON schema of the tool arguments, as JSON text.
    pub parameters_json: String,
}

/// What the history row's `summary` slot holds (`context-retrieval.md` §4).
///
/// Three states on purpose: a `summary.v1` row whose stored text did not
/// parse has to be distinguishable from "this row is not a summary at all".
/// Collapsing both into an absent record would replay the raw JSON body as a
/// normal turn and put it in front of the model, and §4 rule 3 requires the
/// failure to be recorded rather than silently swallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarySlot {
    /// A normal `turn`: there is no summary on this row.
    None,
    /// A `summary.v1` row whose stored text parsed. Fail-closed validation
    /// against [`HistoryItem::body`] still runs in the `select_history`
    /// stage — parsing and validating are two different promises.
    Parsed(SummaryV1),
    /// A `summary.v1` row whose stored text is not `summary.v1` JSON.
    Malformed,
}

impl SummarySlot {
    /// Whether this row *is* a summary rather than a plain turn — the one
    /// spelling of "is this row a summary?", for every site that asks.
    ///
    /// Worth centralising because both `Malformed` and `Parsed` are summaries
    /// and `None` is not: a fourth state would have to be decided here and
    /// nowhere else, and three `matches!` arms that happen to agree today is
    /// how that stops being true.
    pub fn is_summary(&self) -> bool {
        !matches!(self, SummarySlot::None)
    }
}

/// One replayed history item.
///
/// Bodies are replayed **verbatim** (`architecture.md` §6.5 History 不重写):
/// a turn rendered by an earlier run must reappear byte-identical, which is
/// what keeps the request prefix stable across adjacent runs.
///
/// `history_items` mapping (`context-retrieval.md` §3): `kind` = `"turn"` for
/// a normal turn, `"summary"` when [`Self::summary`] is not
/// [`SummarySlot::None`]; `payload_json` carries `referenced_chunk_hashes`
/// (a `summary.v1` mirror is reserved for a later schema — this build neither writes
/// nor reads it); `token_estimate` =
/// `momotaro_contracts::estimate_tokens(body)`; `round` has no column — the
/// run layer re-derives it from the session's run order (`seq` is the
/// run-internal write order, a different axis); a summary row is appended
/// with `role = "user"` (§4: only-append summary.v1).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    /// Stable identifier (survives into `history_items.history_id`).
    pub history_id: String,
    /// The ask round this turn belongs to (1-based); drives the recency
    /// guard band (`context-retrieval.md` §3).
    pub round: u32,
    /// Which side of the conversation this turn is.
    pub role: HistoryRole,
    /// The exact body to replay.
    pub body: String,
    /// Whether this row carries a summary: a turn, a summary that parsed, or
    /// a summary that did not. **Derived from `history_items.content_kind`
    /// (`"summary.v1"`), not from `history_items.kind`** — the two columns are
    /// separate vocabularies (`kind`: `turn | summary`; `content_kind`:
    /// `text | summary.v1`, `HistoryItemRecord`'s field docs) and neither has
    /// a CHECK, so a reader that picks the wrong one still gets a legal row
    /// back and fails silently. Validated fail-closed against `body` when it
    /// is [`SummarySlot::Parsed`].
    pub summary: SummarySlot,
    /// `chunk_hash` values whose full text this turn already carries (its
    /// embedded evidence blocks); used by the dedup stage.
    pub referenced_chunk_hashes: Vec<String>,
}

/// One evidence candidate: a [`RetrievalHit`] joined with the canonical chunk
/// text and the trust origin it must carry end to end
/// (`architecture.md` §12: origin_class 不中途丢弃).
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceItem {
    /// The ranked hit (carries the full citation key set).
    pub hit: RetrievalHit,
    /// Canonical chunk text (verbatim; `chunks.text` semantics).
    pub text: String,
    /// Trust origin of the owning source revision.
    pub origin_class: OriginClass,
    /// Display title of the owning source, when derivable.
    pub title: Option<String>,
}

/// Session-level pure state the ordering and dedup rules read
/// (`context-retrieval.md` §1.1 layer 1 + §3 第 4 条).
///
/// **Production does not persist this.** [`ContextBuildResult::next_session`]
/// is what *this* build would carry forward; the run layer discards it and
/// recomputes the ledger from the rows above on the next turn instead —
/// canonical is append-only, so recomputation cannot drift, and a stored
/// copy would be the second truth §1.1 forbids. The consequence is worth
/// naming: `injected` means "already in a request", so a run that failed
/// *before* the provider call contributes nothing — that costs a repeat
/// budget line and never costs content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    /// Cumulative first-appearance order of `chunk_hash` values across the
    /// session's builds. A reappearing chunk keeps this slot; new chunks
    /// append after.
    ///
    /// 见证口径（`context-retrieval.md` §1.1）：「出现」= 该 chunk 的全文真的进了
    /// provider 请求，即 `run_context_items` 落下的那一条——不是「进了 rank 阶段」。
    /// 被预算裁掉、从未发出的 chunk 不入账：它不在任何请求前缀里，给它一个槽位既不
    /// 影响字节稳定，还会让本账本与 canonical 行成为两份真相。
    ///
    /// 这三个字段是 canonical 行的**纯函数**：`first_seen_order` ←
    /// `run_context_items` 的 evidence 行按 (轮次序, ordinal) 取首现；`injected`
    /// ← `history_items` 里 `role = user` 轮的 payload；`fragment_seq` ← 已渲染
    /// `fragment_id` 的最大值（数值解析，不是字典序）。排查顺序异常时去这三处重算，
    /// 不看任何落库副本。
    pub first_seen_order: Vec<String>,
    /// `chunk_hash` values whose full text already entered a provider
    /// request — including full text carried by replayed history turns.
    /// Deduped out of later evidence selections (`context-retrieval.md`
    /// §3 第 4 条).
    ///
    /// The base **narrows when a summary covers a turn**: the covered turn's
    /// hashes are released, because its text left the replay window with the
    /// demotion and a permanent blacklist would make that content unreachable
    /// in 0.x (no `kb_read`). The release only hands fresh retrieve a chance;
    /// it does not promise the chunk comes back.
    pub injected: Vec<String>,
    /// Next fragment-id sequence. Fragment ids are session-unique citation
    /// handles: a build numbers its fragments `f{seq + 1}`… and advances
    /// this counter, so handles embedded in replayed history turns never
    /// collide with the current turn's ids.
    pub fragment_seq: u32,
}

/// Everything [`crate::build_context`] needs. The function is pure: same
/// input ⇒ same request bytes (stage `duration_ms` values are the only
/// time-varying output, and they never touch the request).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextInput {
    /// The asked question, as the user wrote it.
    pub query: String,
    /// Query intent; embedded now, routed later (§4.1).
    pub intent: QueryIntent,
    /// The `policy.v1` resident text. Constraints belong inside it
    /// (§2 预算分配 level 1: policy + tool definitions + constraints).
    pub policy_text: String,
    /// Tool definitions, in the caller's order (§1 第 3 条: 序即身份 — rendering
    /// fixes the field order but never reorders the list).
    pub tool_definitions: Vec<ToolDefinition>,
    /// The fixed instruction tail (§2 level 5).
    pub instructions: String,
    /// Prior turns, oldest first.
    pub history: Vec<HistoryItem>,
    /// Current retrieval evidence candidates, in retrieval order.
    pub evidence: Vec<EvidenceItem>,
    /// Session pure state (position conservation + injected set).
    pub session: SessionState,
    /// Budget knobs, including the calibratable constants (see
    /// [`BudgetPolicy`]).
    pub budget: BudgetPolicy,
}

/// One context fragment, aligned with `run_context_items` columns
/// (`architecture.md` §8.1).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextFragment {
    /// Session-unique citation handle (`f0001`, …, see
    /// [`SessionState::fragment_seq`]). Gaps are allowed: ids are never
    /// renumbered after a later stage drops a fragment — renumbering would
    /// silently invalidate citations that name a dropped id.
    pub fragment_id: String,
    /// Which source category this fragment carries.
    pub kind: FragmentKind,
    /// Message role at provider lowering.
    pub role: Role,
    /// Body shape for persistence.
    pub content_kind: ContentKind,
    /// The fragment body as the provider sees it. `run_context_items.body_hash`
    /// is computed by the run layer over these `body` **bytes** (the hash
    /// algorithm is the store layer's choice, documented there; this crate
    /// deliberately does not hash).
    pub body: String,
    /// JSON citation reference for evidence fragments (four keys + origin).
    pub source_ref_json: Option<String>,
    /// Estimated tokens of `body` (contracts estimator).
    pub token_estimate: u32,
    /// Lowering gate.
    pub visibility: Visibility,
}

/// One provider message after lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMessage {
    /// Message role.
    pub role: Role,
    /// Message content (joined fragment bodies).
    pub content: String,
}

/// The typed reason one item was dropped at a stage. Omission is always
/// explicit: there is no code path that drops an item silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmittedReason {
    /// The five-level budget ran out before this item (不回填).
    BudgetExhausted,
    /// Same `chunk_hash` as an earlier fragment in this build.
    DuplicateChunkHash,
    /// Full text already injected in an earlier turn (§3 第 4 条).
    PreviouslyInjected,
    /// A valid `summary.v1` covers this raw turn (§3 第 2 条).
    CoveredBySummary,
    /// The summary record failed fail-closed validation; the raw history
    /// falls back instead.
    SummaryValidationFailed,
    /// Citation key set incomplete; fail-closed drop (§12 validate_citations).
    IncompleteCitation,
    /// Beyond the intent-tier top-k cap (§2.1).
    BeyondTierMax,
    /// Untrusted delimiter rendering failed its own well-formedness check.
    DelimiterViolation,
    /// The tool's `parameters_json` is not a JSON object.
    InvalidToolSchema,
}

impl OmittedReason {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 9] = [
        Self::BudgetExhausted,
        Self::DuplicateChunkHash,
        Self::PreviouslyInjected,
        Self::CoveredBySummary,
        Self::SummaryValidationFailed,
        Self::IncompleteCitation,
        Self::BeyondTierMax,
        Self::DelimiterViolation,
        Self::InvalidToolSchema,
    ];

    /// The wire spelling of this reason.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BudgetExhausted => "budget_exhausted",
            Self::DuplicateChunkHash => "duplicate_chunk_hash",
            Self::PreviouslyInjected => "previously_injected",
            Self::CoveredBySummary => "covered_by_summary",
            Self::SummaryValidationFailed => "summary_validation_failed",
            Self::IncompleteCitation => "incomplete_citation",
            Self::BeyondTierMax => "beyond_tier_max",
            Self::DelimiterViolation => "delimiter_violation",
            Self::InvalidToolSchema => "invalid_tool_schema",
        }
    }
}

/// One explicit drop: which item, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Omission {
    /// The item key (`chunk_hash`, `history_id`, or a fixed fragment key).
    pub item: String,
    /// The typed reason.
    pub reason: OmittedReason,
}

/// The ten pipeline stages, in contract order (`architecture.md` §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Stage 1.
    NormalizeQuery,
    /// Stage 2.
    LoadPolicy,
    /// Stage 3.
    SelectHistory,
    /// Stage 4.
    SelectEvidence,
    /// Stage 5.
    RenderFragments,
    /// Stage 6.
    Deduplicate,
    /// Stage 7.
    Rank,
    /// Stage 8.
    EnforceTokenBudget,
    /// Stage 9.
    RenderProviderMessages,
    /// Stage 10.
    ValidateCitations,
}

impl Stage {
    /// Every variant of this enum. Witness tables that promise full
    /// coverage (`wire_spellings`) iterate this and pin its length; a
    /// new variant means extending this list, not just `as_str`.
    pub const ALL: [Self; 10] = [
        Self::NormalizeQuery,
        Self::LoadPolicy,
        Self::SelectHistory,
        Self::SelectEvidence,
        Self::RenderFragments,
        Self::Deduplicate,
        Self::Rank,
        Self::EnforceTokenBudget,
        Self::RenderProviderMessages,
        Self::ValidateCitations,
    ];

    /// The wire spelling of this stage (the §12 name verbatim).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NormalizeQuery => "normalize_query",
            Self::LoadPolicy => "load_policy",
            Self::SelectHistory => "select_history",
            Self::SelectEvidence => "select_evidence",
            Self::RenderFragments => "render_fragments",
            Self::Deduplicate => "deduplicate",
            Self::Rank => "rank",
            Self::EnforceTokenBudget => "enforce_token_budget",
            Self::RenderProviderMessages => "render_provider_messages",
            Self::ValidateCitations => "validate_citations",
        }
    }
}

/// Per-stage record: what passed, what was dropped and why, and how the
/// estimated-token account moved (`architecture.md` §12: 每个阶段记录
/// `included` / `omitted` / `omitted_reason` / `token_delta` / `duration_ms`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRecord {
    /// Which stage this record covers.
    pub stage: Stage,
    /// Item keys that passed the stage.
    pub included: Vec<String>,
    /// Explicit drops; empty only when nothing was dropped.
    pub omitted: Vec<Omission>,
    /// Signed change of the accounted raw tokens.
    pub token_delta: i64,
    /// Wall-clock milliseconds the stage took (injected clock).
    pub duration_ms: u64,
}

/// The budget accounting summary of one build (`architecture.md` §12.2).
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetReport {
    /// Configured ceiling.
    pub max_context_tokens: u32,
    /// Raw estimated tokens of the final fragment bodies (the tools
    /// fragment included).
    pub raw_total: u64,
    /// `ceil(raw_total * coefficient)` — the estimate used for trimming.
    pub adjusted_total: u64,
    /// True when the included content still exceeds `max_context_tokens`
    /// (fixed parts are never silently dropped).
    pub exceeded: bool,
}

/// Everything one [`crate::build_context`] call produces.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBuildResult {
    /// Whitespace-normalized query (retrieval-facing; not rendered).
    pub normalized_query: String,
    /// Final fragments, in render order (policy, tool_definitions, history…,
    /// evidence…, instructions, question).
    pub fragments: Vec<ContextFragment>,
    /// Lowered provider messages (system, history replay…, final user).
    pub messages: Vec<ProviderMessage>,
    /// Canonical tools serialization (the request's `tools` field).
    pub tools_json: String,
    /// The ten stage records, in contract order.
    pub stages: Vec<StageRecord>,
    /// Budget accounting.
    pub budget: BudgetReport,
    /// The ledger as this build would leave it. **Not persisted** — the run
    /// layer recomputes it from canonical rows on the next turn (see
    /// [`SessionState`]); this is the build's own answer, kept so a caller
    /// can compare or test it.
    pub next_session: SessionState,
    /// `chunk_hash` order of the final evidence fragments.
    pub(crate) evidence_chunk_hashes: Vec<String>,
    /// The same fragments' indices into the build's `input.evidence`
    /// (parallel to `evidence_chunk_hashes`).
    pub(crate) evidence_indices: Vec<usize>,
}

impl ContextBuildResult {
    /// Canonical request bytes over the deterministic part of the provider
    /// request: the tools JSON followed by the messages, framed so the
    /// stream is **append-friendly** — replaying more turns only appends,
    /// so an earlier run's bytes stay a byte prefix of a later run's
    /// (the cache-stability contract, `context-retrieval.md` §1).
    ///
    /// Layout (`momotaro.request.v1`):
    ///
    /// ```text
    /// momotaro.request.v1\n
    /// tools <tools_json>\n
    /// msg <role> <content-len-bytes>\n<content-bytes>\n
    /// msg <role> <content-len-bytes>\n<content-bytes>\n
    /// …
    /// ```
    ///
    /// The run layer takes `llm_turns.request_hash` over these bytes.
    pub fn request_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"momotaro.request.v1\n");
        out.extend_from_slice(b"tools ");
        out.extend_from_slice(self.tools_json.as_bytes());
        out.push(b'\n');
        for message in &self.messages {
            out.extend_from_slice(b"msg ");
            out.extend_from_slice(message.role.as_str().as_bytes());
            out.push(b' ');
            out.extend_from_slice(message.content.len().to_string().as_bytes());
            out.push(b'\n');
            out.extend_from_slice(message.content.as_bytes());
            out.push(b'\n');
        }
        out
    }

    /// The content of the final user message (the current turn: evidence
    /// blocks, instructions, question). Run persists this body verbatim for
    /// byte-stable replay in later turns.
    pub fn final_user_body(&self) -> &str {
        self.messages
            .last()
            .map(|m| m.content.as_str())
            .unwrap_or_default()
    }

    /// `chunk_hash` values of the final evidence fragments, in render order.
    pub fn evidence_order(&self) -> impl Iterator<Item = &str> {
        self.evidence_chunk_hashes.iter().map(String::as_str)
    }

    /// Index order of the final evidence fragments into the build's
    /// `input.evidence` — parallel to [`Self::evidence_order`], so a caller
    /// holding the input list can tell which hit survived a dedup.
    pub fn evidence_indices(&self) -> impl Iterator<Item = usize> {
        self.evidence_indices.iter().copied()
    }
}
