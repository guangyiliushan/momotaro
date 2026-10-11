//! 前缀字节稳定回归(相邻 run 公共前缀字节级一致)。
//!
//! 契约来源:请求确定性与缓存稳定、evidence 三层排序、build_context 十阶段。
//!
//! 断言的核心性质:**相邻 run 的完整 provider 请求字节是下一个 run 请求字节的
//! 前缀**(在未压缩窗口内),且 session 内已出现过的 chunk 位置守恒。任何把
//! 「按 retrieval rank 重排」「时间戳进稳定前缀」「tools 序列化不稳定」写回
//! 代码的突变都必须让本文件变红。

use momotaro_context::{
    BudgetPolicy, ContextInput, EvidenceItem, HistoryItem, HistoryRole, OmittedReason, QueryIntent,
    SessionState, SummarySlot, ToolDefinition, build_context, build_context_with_clock,
};
use momotaro_contracts::{OriginClass, RetrievalHit};

fn hit(rank: u32, chunk_id: &str, chunk_hash: &str, source_key: &str, score: f64) -> RetrievalHit {
    RetrievalHit {
        rank,
        source_key: source_key.to_string(),
        revision_hash: format!("rev-{chunk_id}"),
        chunk_id: chunk_id.to_string(),
        chunk_hash: chunk_hash.to_string(),
        ordinal: rank,
        heading_path: None,
        score,
        locator_json: r#"{"start_byte":0,"end_byte":1}"#.to_string(),
    }
}

fn evidence(
    hit: RetrievalHit,
    text: &str,
    origin: OriginClass,
    title: Option<&str>,
) -> EvidenceItem {
    EvidenceItem {
        hit,
        text: text.to_string(),
        origin_class: origin,
        title: title.map(str::to_string),
    }
}

fn base_input() -> ContextInput {
    ContextInput {
        query: "what is fourier".to_string(),
        intent: QueryIntent::Fact,
        policy_text: "policy.v1: cite sources.".to_string(),
        tool_definitions: vec![
            ToolDefinition {
                name: "kb_read".to_string(),
                description: "read a chunk".to_string(),
                parameters_json: r#"{"type":"object"}"#.to_string(),
            },
            ToolDefinition {
                name: "calc".to_string(),
                description: "calculator".to_string(),
                parameters_json: r#"{"type":"object","properties":{}}"#.to_string(),
            },
        ],
        instructions: "Answer with citations.".to_string(),
        history: Vec::new(),
        evidence: Vec::new(),
        session: SessionState::default(),
        budget: BudgetPolicy::default(),
    }
}

fn lcp_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// 相邻 run 的公共前缀字节级一致——上一 run 的**完整请求字节**必须是
/// 下一 run 请求字节的前缀(未压缩窗口内)。
///
/// 杀的突变:① 时间戳/run_id 泄进 system 级内容;② 历史轮被重渲染而不是逐字节
/// 重放;③ 新一轮把 evidence 挪到 history 之前。
///
/// tools 列表序的牙不在这里:本文件的 tools 恰好已按名有序,「按名排序」突变与
/// 「永不排序」在此同形(实测 16/16 全绿);那条由 `render.rs` 的
/// `tools_keep_the_callers_order` 杀。
#[test]
fn adjacent_runs_keep_their_common_prefix_byte_stable() {
    // run 1:无历史,两条 evidence,问题 q1。
    let mut input1 = base_input();
    input1.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:ml/fourier.md", 9.0),
            "fourier body one",
            OriginClass::Owner,
            Some("Fourier"),
        ),
        evidence(
            hit(2, "c2", "h2", "note:ml/fft.md", 5.0),
            "fft body two",
            OriginClass::Owner,
            None,
        ),
    ];
    input1.query = "what is fourier".to_string();
    let run1 = build_context_with_clock(input1, &|| 0u64);
    let b1 = run1.request_bytes();

    // run 2:历史 = run 1 的轮次(user body 由 run 层原样存回;此处按契约逐字节
    // 重放),evidence 里旧 chunk 已注入 ⇒ 去重后只剩新 chunk。
    let mut input2 = base_input();
    input2.query = "and the fft".to_string();
    input2.history = vec![
        HistoryItem {
            history_id: "h-1".to_string(),
            round: 1,
            role: momotaro_context::HistoryRole::User,
            body: run1.final_user_body().to_string(),
            summary: SummarySlot::None,
            referenced_chunk_hashes: vec!["h1".to_string(), "h2".to_string()],
        },
        HistoryItem {
            history_id: "h-2".to_string(),
            round: 1,
            role: momotaro_context::HistoryRole::Assistant,
            body: "the fourier transform is…".to_string(),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
    ];
    input2.session = run1.next_session.clone();
    input2.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:ml/fourier.md", 9.0),
            "fourier body one",
            OriginClass::Owner,
            Some("Fourier"),
        ),
        evidence(
            hit(2, "c2", "h2", "note:ml/fft.md", 5.0),
            "fft body two",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(3, "c3", "h3", "note:ml/dft.md", 7.0),
            "dft body three",
            OriginClass::Owner,
            None,
        ),
    ];
    let run2 = build_context_with_clock(input2, &|| 0u64);
    let b2 = run2.request_bytes();

    // 整个 run 1 的请求字节必须逐字节落在 run 2 请求的前缀里。
    assert_eq!(
        lcp_len(&b1, &b2),
        b1.len(),
        "run1 request bytes must be a byte prefix of run2 (lcp {} of {})",
        lcp_len(&b1, &b2),
        b1.len()
    );
}

/// 三连 run 的链式前缀性质(同一未压缩窗口):b1 ⊑ b2 ⊑ b3。
#[test]
fn a_chain_of_runs_grows_by_append_only() {
    let mk = |query: &str, ev: Vec<EvidenceItem>, hist: Vec<HistoryItem>, s: SessionState| {
        let mut i = base_input();
        i.query = query.to_string();
        i.evidence = ev;
        i.history = hist;
        i.session = s;
        build_context_with_clock(i, &|| 0u64)
    };

    let e = |id: &str, hash: &str, src: &str, score: f64, text: &str| {
        evidence(hit(1, id, hash, src, score), text, OriginClass::Owner, None)
    };

    let r1 = mk(
        "q1",
        vec![e("c1", "h1", "note:a.md", 2.0, "body a")],
        vec![],
        SessionState::default(),
    );
    let r2 = mk(
        "q2",
        vec![
            e("c1", "h1", "note:a.md", 2.0, "body a"),
            e("c2", "h2", "note:b.md", 1.0, "body b"),
        ],
        vec![
            HistoryItem {
                history_id: "h-1".to_string(),
                round: 1,
                role: momotaro_context::HistoryRole::User,
                body: r1.final_user_body().to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: vec!["h1".to_string()],
            },
            HistoryItem {
                history_id: "h-2".to_string(),
                round: 1,
                role: momotaro_context::HistoryRole::Assistant,
                body: "answer one".to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: Vec::new(),
            },
        ],
        r1.next_session.clone(),
    );
    let r3 = mk(
        "q3",
        vec![
            e("c1", "h1", "note:a.md", 2.0, "body a"),
            e("c2", "h2", "note:b.md", 1.0, "body b"),
            e("c3", "h3", "note:c.md", 5.0, "body c"),
        ],
        vec![
            HistoryItem {
                history_id: "h-1".to_string(),
                round: 1,
                role: momotaro_context::HistoryRole::User,
                body: r1.final_user_body().to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: vec!["h1".to_string()],
            },
            HistoryItem {
                history_id: "h-2".to_string(),
                round: 1,
                role: momotaro_context::HistoryRole::Assistant,
                body: "answer one".to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: Vec::new(),
            },
            HistoryItem {
                history_id: "h-3".to_string(),
                round: 2,
                role: momotaro_context::HistoryRole::User,
                body: r2.final_user_body().to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: vec!["h2".to_string()],
            },
            HistoryItem {
                history_id: "h-4".to_string(),
                round: 2,
                role: momotaro_context::HistoryRole::Assistant,
                body: "answer two".to_string(),
                summary: SummarySlot::None,
                referenced_chunk_hashes: Vec::new(),
            },
        ],
        r2.next_session.clone(),
    );

    let (b1, b2, b3) = (r1.request_bytes(), r2.request_bytes(), r3.request_bytes());
    assert_eq!(lcp_len(&b1, &b2), b1.len(), "b1 must prefix b2");
    assert_eq!(lcp_len(&b2, &b3), b2.len(), "b2 must prefix b3");
}

/// 第 1 层(session 位置守恒)压过第 3 层(按分数首尾摆放):已出现过但未注入的
/// chunk 保持首次出现位置,高分新 chunk 追加在后——即使它分数更高。
///
/// 杀的突变:rank 阶段改回「按 retrieval score 重排」。
#[test]
fn first_seen_positions_win_over_scores() {
    // run 1:两条 evidence 都进入排序(位置入账)。
    let mut i1 = base_input();
    i1.evidence = vec![
        evidence(
            hit(1, "c1", "seen1", "note:a.md", 1.0),
            "x",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "seen2", "note:b.md", 9.0),
            "y",
            OriginClass::Owner,
            None,
        ),
    ];
    let r1 = build_context_with_clock(i1, &|| 0u64);

    // run 2 是同一 turn 的**重建**(尚未发出请求):位置账本继续用,注入账本
    // 不带入——seen2 因此不去重,保持首次出现槽位。
    let mut i2 = base_input();
    i2.session = SessionState {
        first_seen_order: r1.next_session.first_seen_order.clone(),
        injected: Vec::new(),
        fragment_seq: r1.next_session.fragment_seq,
    };
    i2.evidence = vec![
        evidence(
            hit(1, "c3", "new1", "note:c.md", 99.0),
            "z",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "seen2", "note:b.md", 9.0),
            "y",
            OriginClass::Owner,
            None,
        ),
    ];
    let r2 = build_context_with_clock(i2, &|| 0u64);

    let order: Vec<&str> = r2.evidence_order().collect();
    assert_eq!(
        order,
        vec!["seen2", "new1"],
        "seen2 keeps its first-seen slot ahead of the higher-scored newcomer"
    );
}

/// 同输入两次构建 ⇒ 请求字节逐位一致(纯确定性;时钟只准出现在阶段记录里)。
#[test]
fn the_same_input_renders_the_same_request_bytes() {
    let mut input = base_input();
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:a.md", 1.0),
            "text with <<< trick",
            OriginClass::Web,
            Some("T"),
        ),
        evidence(
            hit(2, "c2", "h2", "note:b.md", 2.0),
            "plain",
            OriginClass::Paper,
            None,
        ),
    ];
    let a = build_context(input.clone()).request_bytes();
    let b = build_context(input).request_bytes();
    assert_eq!(a, b);
}

/// 阶段记录:十个阶段一个不少、顺序固定;每个省略项都带类型化原因;token_delta
/// 求和等于最终 raw_total(不静默丢 token 记账)。
#[test]
fn every_stage_reports_and_no_omission_is_silent() {
    let mut input = base_input();
    input.budget = BudgetPolicy {
        max_context_tokens: 30,
        ..BudgetPolicy::default()
    };
    // 6 条 evidence:意图分档(fact ≤5)与预算都必须产生显式省略。
    input.evidence = (1..=6)
        .map(|n| {
            evidence(
                hit(
                    n,
                    &format!("c{n}"),
                    &format!("h{n}"),
                    &format!("note:{n}.md"),
                    9.0 - n as f64,
                ),
                &format!("body {n}"),
                OriginClass::Owner,
                None,
            )
        })
        .collect();
    let result = build_context_with_clock(input, &|| 7u64);

    let names: Vec<&str> = result.stages.iter().map(|s| s.stage.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "normalize_query",
            "load_policy",
            "select_history",
            "select_evidence",
            "render_fragments",
            "deduplicate",
            "rank",
            "enforce_token_budget",
            "render_provider_messages",
            "validate_citations",
        ]
    );
    let omission_count: usize = result.stages.iter().map(|s| s.omitted.len()).sum();
    assert!(
        omission_count > 0,
        "this fixture must produce explicit omissions"
    );
    for record in &result.stages {
        for omission in &record.omitted {
            assert!(!omission.item.is_empty(), "every omission names its item");
        }
    }
    // 该夹具必须真实产生两类省略;断言字面拼写(不从被测代码取期望)。
    let reasons: Vec<&str> = result
        .stages
        .iter()
        .flat_map(|s| s.omitted.iter().map(|o| o.reason.as_str()))
        .collect();
    assert!(
        reasons.contains(&"beyond_tier_max"),
        "the tier cap must fire: {reasons:?}"
    );
    assert!(
        reasons.contains(&"budget_exhausted"),
        "the budget must fire: {reasons:?}"
    );
    let delta_sum: i64 = result.stages.iter().map(|s| s.token_delta).sum();
    // 外部复算:期望值取自返回的 fragments,不取自被测代码的记账变量。
    let independent: u64 = result
        .fragments
        .iter()
        .map(|f| u64::from(momotaro_contracts::estimate_tokens(&f.body)))
        .sum();
    assert_eq!(
        independent, result.budget.raw_total,
        "the budget report must match the fragment bodies themselves"
    );
    assert_eq!(
        delta_sum as u64, independent,
        "stage token deltas account for exactly the fragment bodies"
    );
}

/// The stage accounting has to survive **every** branch that reverses it.
///
/// `delta_sum == sum(fragment bodies)` is checked by
/// `every_stage_reports_and_no_omission_is_silent`, but that fixture enters
/// none of the six places where a stage subtracts what it dropped instead of
/// adding what it kept — the history load, the render-time drop, the two
/// dedup arms, the citation drop, and the kind filter on stage 10's
/// `included`. Eleven mutants across those branches left the workspace green,
/// because a branch never entered cannot be observed by the invariant it
/// participates in.
#[test]
fn the_stage_deltas_sum_to_the_fragments_across_every_reverse_accounting_branch() {
    let mut input = base_input();
    input.budget = BudgetPolicy {
        // Wide on purpose: this fixture drives the branches that *reverse*
        // accounting (history load, render drop, both dedup arms, the citation
        // drop, stage 10's kind filter) — none of them is the budget, and a
        // budget tight enough to cut evidence would drop the incomplete-citation
        // item before stage 10 ever saw it.
        max_context_tokens: 4096,
        ..BudgetPolicy::default()
    };
    // Comparison's tier is (4, 6): all six candidates have to reach their
    // stages — the render drop included — and fact's 5 cut the web item at
    // `select_evidence` (beyond_tier_max) before `render` ever saw it.
    input.intent = QueryIntent::Comparison;
    // `h1` was injected last turn: the dedup arm that subtracts it fires.
    input.session = SessionState {
        first_seen_order: vec!["h1".to_string()],
        injected: vec!["h1".to_string()],
        fragment_seq: 0,
    };
    // One turn inside the recency band (kept) and one far outside it
    // (dropped by `select_history`), so the history stage both adds and
    // subtracts.
    input.history = vec![
        HistoryItem {
            history_id: "h-out".to_string(),
            round: 1,
            role: HistoryRole::User,
            body: "an older question".to_string(),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
        HistoryItem {
            history_id: "h-in".to_string(),
            round: 9,
            role: HistoryRole::User,
            body: "the latest question".to_string(),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
        HistoryItem {
            history_id: "h-ans".to_string(),
            round: 9,
            role: HistoryRole::Assistant,
            body: "an answer".to_string(),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
    ];
    input.evidence = vec![
        // already injected: the dedup arm that subtracts it fires.
        evidence(
            hit(1, "c1", "h1", "note:a.md", 9.0),
            "seen last turn",
            OriginClass::Owner,
            None,
        ),
        // the same `chunk_hash` twice: the other dedup arm.
        evidence(
            hit(2, "c2", "h2", "note:b.md", 8.0),
            "first copy",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(3, "c2", "h2", "note:b.md", 7.0),
            "second copy",
            OriginClass::Owner,
            None,
        ),
        // the only one that survives all of it.
        evidence(
            hit(4, "c3", "h3", "note:c.md", 6.0),
            "new material",
            OriginClass::Owner,
            None,
        ),
        // Web content with an empty source label: the render stage drops it
        // — a marker carrying an empty label is outside the escaped grammar
        // (`validate_untrusted_regions`'s `UnescapedLabel`), so the block is
        // never emitted — and the stage has to reverse its own accounting on
        // the way. (The previous stand-in — a raw `<<<` in the body — could
        // never fire from here: escaping neutralized the sentinel, and the
        // item was cut at `select_evidence` before `render` saw it.)
        evidence(
            hit(5, "c4", "h4", "", 5.0),
            "a web block whose label is empty",
            OriginClass::Web,
            Some("web"),
        ),
        // A hit whose citation key set is not complete: stage 10 drops it
        // before it can reach the request, and has to hand its tokens back.
        evidence(
            // Rank 0 so `select_evidence`'s rank order keeps it: the tier band
            // and the budget share are what decide the cut, and a late rank
            // never survives them.
            hit(0, "c5", "", "note:d.md", 9.5),
            "a hit with no chunk hash",
            OriginClass::Owner,
            None,
        ),
    ];

    let result = build_context_with_clock(input, &|| 7u64);

    let reasons: Vec<&str> = result
        .stages
        .iter()
        .flat_map(|s| s.omitted.iter().map(|o| o.reason.as_str()))
        .collect();
    assert!(
        reasons.contains(&"previously_injected"),
        "the arm that subtracts an injected chunk must fire: {reasons:?}"
    );
    assert!(
        reasons.contains(&"duplicate_chunk_hash"),
        "…and so must the arm that subtracts a repeat: {reasons:?}"
    );
    assert!(
        reasons.contains(&"delimiter_violation"),
        "the render-time drop has to fire: a web block whose label is empty \
         is never emitted: {reasons:?}"
    );

    // Stage 10's `included` is filtered to evidence: anything else in `work`
    // (policy, instructions, question, history) must not be listed as a
    // citation the answer could have made.
    let citations = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "validate_citations")
        .expect("the citation stage runs");
    assert!(
        !citations.included.is_empty(),
        "…and it has something to report: {citations:?}"
    );
    assert!(
        citations
            .omitted
            .iter()
            .any(|omission| omission.reason.as_str() == "incomplete_citation"),
        "the fail-closed citation check has to fire: {:?}",
        citations.omitted
    );
    assert!(
        citations.token_delta <= 0,
        "dropping a fragment gives its tokens back, it does not claim more: {}",
        citations.token_delta
    );
    assert!(
        citations.included.iter().all(|key| key.starts_with('h')),
        "only chunk hashes are citations — the filter keeps the other kinds out: {:?}",
        citations.included
    );

    // The invariant itself, over a fixture that actually reverses accounting.
    let delta_sum: i64 = result.stages.iter().map(|s| s.token_delta).sum();
    let independent: u64 = result
        .fragments
        .iter()
        .map(|f| u64::from(momotaro_contracts::estimate_tokens(&f.body)))
        .sum();
    assert_eq!(
        independent, result.budget.raw_total,
        "the budget report must match the fragment bodies themselves"
    );
    assert_eq!(
        delta_sum as u64, independent,
        "stage token deltas account for exactly the fragment bodies, after the \\
         history load, both dedup arms and the citation drop have all run"
    );
}

/// The budget stage's accounting matches drops by slot, not by
/// first-match-by-key: a dropped history turn whose id spells a dropped
/// evidence fragment's `chunk_hash` used to have the wrong object's tokens
/// subtracted (−150 where −107 was due), and the stage deltas then
/// stopped totalling the fragment bodies. The invariant at the end is the
/// tooth: discount either collision drop with the other's tokens and it
/// fails.
#[test]
fn the_budget_delta_uses_the_dropped_object_not_its_key_namesake() {
    let mut input = base_input();
    // The resident layer alone exceeds the budget, so every non-forced claim
    // below (the history turn, the fourth evidence item) fails on the spot —
    // which is what puts two colliding drops in one stage record.
    input.policy_text = "policy word ".repeat(62);
    input.budget = BudgetPolicy {
        max_context_tokens: 200,
        ..BudgetPolicy::default()
    };
    input.history = vec![HistoryItem {
        history_id: "same".to_string(),
        round: 9,
        role: HistoryRole::User,
        body: "turn ".repeat(75),
        summary: SummarySlot::None,
        referenced_chunk_hashes: Vec::new(),
    }];
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:x.md", 9.0),
            "tiny one",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h2", "note:x.md", 8.0),
            "tiny two",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(3, "c3", "h3", "note:x.md", 7.0),
            "tiny three",
            OriginClass::Owner,
            None,
        ),
        // The namesake: its `chunk_hash` spells the history turn's id.
        evidence(
            hit(4, "c4", "same", "note:x.md", 6.0),
            "the namesake block",
            OriginClass::Owner,
            None,
        ),
    ];

    let result = build_context_with_clock(input, &|| 7u64);

    let budget_stage = result
        .stages
        .iter()
        .find(|stage| stage.stage.as_str() == "enforce_token_budget")
        .expect("the budget stage runs");
    let same_key_drops = budget_stage
        .omitted
        .iter()
        .filter(|omission| omission.item == "same")
        .count();
    assert_eq!(
        same_key_drops, 2,
        "the collision has to be dropped twice for this test to mean anything: {budget_stage:?}"
    );
    let delta_sum: i64 = result.stages.iter().map(|s| s.token_delta).sum();
    let independent: u64 = result
        .fragments
        .iter()
        .map(|f| u64::from(momotaro_contracts::estimate_tokens(&f.body)))
        .sum();
    assert_eq!(
        delta_sum as u64, independent,
        "a namesake drop must not discount the wrong object's tokens"
    );
}

/// The budget's release set is scoped to the history class: a dropped
/// *evidence* fragment whose `chunk_hash` spells a kept turn's `history_id`
/// must not pull that turn's hashes out of `injected` — the turn is still in
/// the request, and content still in front of the model stays blacklisted
/// (the namesake used to release it).
#[test]
fn a_dropped_evidence_namesake_does_not_release_a_kept_turns_hashes() {
    let mut input = base_input();
    // Wide enough for the history turn, over the line before the fourth
    // evidence item (floor 3 keeps the other three alive).
    input.policy_text = "policy word ".repeat(28);
    input.budget = BudgetPolicy {
        max_context_tokens: 200,
        ..BudgetPolicy::default()
    };
    input.history = vec![HistoryItem {
        history_id: "same".to_string(),
        round: 9,
        role: HistoryRole::User,
        body: "the kept turn".to_string(),
        summary: SummarySlot::None,
        referenced_chunk_hashes: vec!["carry1".to_string()],
    }];
    input.session = SessionState {
        first_seen_order: vec![],
        injected: vec!["carry1".to_string()],
        fragment_seq: 0,
    };
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:x.md", 9.0),
            "tiny one",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h2", "note:x.md", 8.0),
            "tiny two",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(3, "c3", "h3", "note:x.md", 7.0),
            "tiny three",
            OriginClass::Owner,
            None,
        ),
        // The namesake, dropped by the budget: four evidence items, floor 3.
        evidence(
            hit(4, "c4", "same", "note:x.md", 6.0),
            "the namesake evidence block with a longer body to push the budget over the line, and a second copy of that sentence so the block is big enough to fail the claim",
            OriginClass::Owner,
            None,
        ),
    ];

    let result = build_context_with_clock(input, &|| 7u64);

    let budget_stage = result
        .stages
        .iter()
        .find(|stage| stage.stage.as_str() == "enforce_token_budget")
        .expect("the budget stage runs");
    assert_eq!(
        budget_stage
            .omitted
            .iter()
            .filter(|omission| omission.item == "same")
            .count(),
        1,
        "exactly the namesake evidence item is dropped — the kept turn is not: {budget_stage:?}"
    );
    assert!(
        result.next_session.injected.contains(&"carry1".to_string()),
        "the kept turn still carries carry1 — releasing it breaks the rule: {:?}",
        result.next_session.injected
    );
}

/// The stage-1/2/5 deltas count the **escaped** text the fragments actually
/// carry: caller content containing `<<<` is rewritten by
/// `escape_content_sentinels` on the way into a body, and counting the raw
/// input drifted the deltas and `raw_total` apart (40 vs 47) —
/// audit-only, but an audit that lies is not one.
#[test]
fn sentinel_text_in_caller_content_does_not_drift_the_deltas() {
    let mut input = base_input();
    input.query = "what is <<< fourier".to_string();
    input.policy_text = "policy.v1: cite sources. <<< untrusted".to_string();
    input.instructions = "Answer with citations <<<now>>>".to_string();

    let result = build_context_with_clock(input, &|| 7u64);

    let delta_sum: i64 = result.stages.iter().map(|s| s.token_delta).sum();
    let independent: u64 = result
        .fragments
        .iter()
        .map(|f| u64::from(momotaro_contracts::estimate_tokens(&f.body)))
        .sum();
    assert_eq!(
        independent, result.budget.raw_total,
        "the budget report must match the fragment bodies themselves"
    );
    assert_eq!(
        delta_sum as u64, independent,
        "the deltas must count the escaped bodies, not the raw caller text"
    );
}

/// The build reports survivor **indices**, so the run layer can tell which
/// of two hits sharing one `chunk_hash` became the fragment:
/// hash membership read the dropped twin as included, and the
/// `duplicate_chunk_hash` code could never land on a row because the twin
/// was indistinguishable from its survivor.
#[test]
fn the_build_keeps_the_survivors_index_not_its_hash() {
    let mut input = base_input();
    input.evidence = vec![
        evidence(
            hit(1, "c1", "same-hash", "note:a.md", 9.0),
            "first copy",
            OriginClass::Owner,
            None,
        ),
        // The twin: same `chunk_hash`, different identity — the first
        // occurrence is the one dedup keeps.
        evidence(
            hit(2, "c2", "same-hash", "note:b.md", 8.0),
            "second copy",
            OriginClass::Owner,
            None,
        ),
    ];

    let result = build_context_with_clock(input, &|| 7u64);

    assert_eq!(
        result.evidence_order().collect::<Vec<_>>(),
        vec!["same-hash"],
        "one survivor carries the hash"
    );
    assert_eq!(
        result.evidence_indices().collect::<Vec<_>>(),
        vec![0],
        "and the index says it is *this* hit's fragment, not the twin's"
    );
    let dedup = result
        .stages
        .iter()
        .find(|stage| stage.stage.as_str() == "deduplicate")
        .expect("the dedup stage runs");
    assert!(
        dedup.omitted.iter().any(|omission| {
            omission.item == "same-hash" && omission.reason.as_str() == "duplicate_chunk_hash"
        }),
        "the twin's code exists to land on its row: {dedup:?}"
    );
}

/// A fragment without a `chunk_hash` is dropped by the citation check, and
/// its omission row must still be nameable: the audit outlet showed
/// `(,incomplete_citation)` before.
#[test]
fn an_empty_chunk_hash_omission_still_has_a_name() {
    let mut input = base_input();
    input.evidence = vec![evidence(
        hit(1, "c1", "", "note:a.md", 9.0),
        "no hash",
        OriginClass::Owner,
        None,
    )];

    let result = build_context_with_clock(input, &|| 7u64);

    let citations = result
        .stages
        .iter()
        .find(|stage| stage.stage.as_str() == "validate_citations")
        .expect("the citation stage runs");
    assert!(
        citations
            .omitted
            .iter()
            .any(|omission| omission.item == "<no-hash>"),
        "the omission carries a name: {citations:?}"
    );
}

/// 已注入过的 evidence 按 chunk_hash 去重,不因多轮重复占预算。
#[test]
fn already_injected_evidence_never_re_enters() {
    let mut input = base_input();
    input.session = SessionState {
        first_seen_order: vec!["h1".to_string()],
        injected: vec!["h1".to_string()],
        fragment_seq: 0,
    };
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:a.md", 9.0),
            "old body",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h2", "note:b.md", 5.0),
            "new body",
            OriginClass::Owner,
            None,
        ),
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let order: Vec<&str> = result.evidence_order().collect();
    assert_eq!(order, vec!["h2"], "the injected chunk is deduped out");
    let dedup = &result.stages[5];
    assert_eq!(dedup.stage.as_str(), "deduplicate");
    assert!(
        dedup
            .omitted
            .iter()
            .any(|o| o.item == "h1" && o.reason == OmittedReason::PreviouslyInjected),
        "the drop is explicit: {:?}",
        dedup.omitted
    );
}

/// 批内重复 chunk_hash 保首个出现者,其余显式省略。
#[test]
fn duplicate_chunk_hashes_keep_the_first_occurrence() {
    let mut input = base_input();
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:a.md", 9.0),
            "body one",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h1", "note:a.md", 5.0),
            "body two",
            OriginClass::Owner,
            None,
        ),
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let order: Vec<&str> = result.evidence_order().collect();
    assert_eq!(order, vec!["h1"]);
    let bodies: Vec<&str> = result
        .fragments
        .iter()
        .filter(|f| f.kind.as_str() == "evidence")
        .map(|f| f.body.as_str())
        .collect();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("body one"),
        "first occurrence wins: {bodies:?}"
    );
    let dedup = &result.stages[5];
    assert!(
        dedup
            .omitted
            .iter()
            .any(|o| o.reason == OmittedReason::DuplicateChunkHash)
    );
}

/// validate_citations fail-closed:四键不全的 fragment 不进请求,且显式省略。
#[test]
fn a_fragment_without_a_complete_citation_is_dropped_fail_closed() {
    let mut input = base_input();
    let mut bad = hit(1, "c1", "h1", "note:a.md", 9.0);
    bad.revision_hash = String::new();
    let mut bad_locator = hit(3, "c3", "h3", "note:c.md", 7.0);
    bad_locator.locator_json = String::new();
    input.evidence = vec![
        evidence(bad, "bad body", OriginClass::Owner, None),
        evidence(bad_locator, "bad locator body", OriginClass::Owner, None),
        evidence(
            hit(2, "c2", "h2", "note:b.md", 5.0),
            "good body",
            OriginClass::Owner,
            None,
        ),
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let order: Vec<&str> = result.evidence_order().collect();
    assert_eq!(order, vec!["h2"], "the incomplete citations are dropped");
    let vc = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "validate_citations")
        .expect("stage present");
    assert!(
        vc.omitted
            .iter()
            .any(|o| o.item == "h1" && o.reason == OmittedReason::IncompleteCitation),
        "the drop is explicit: {:?}",
        vc.omitted
    );
    assert!(!result.final_user_body().contains("bad body"));
    assert!(!result.final_user_body().contains("bad locator body"));
    assert!(
        vc.omitted
            .iter()
            .any(|o| o.item == "h3" && o.reason == OmittedReason::IncompleteCitation),
        "an empty locator is incomplete too: {:?}",
        vc.omitted
    );
}

/// 预算 level 3(history)按**近轮优先**申领:两轮都放得下单独一条、放不下
/// 两条时,新的一轮留下、旧的一轮显式省略(先到先得不回填 + 近轮全量)。
#[test]
fn history_budget_claims_recent_turns_first() {
    let mut input = base_input();
    // 固定段压到最小,让两轮历史竞争同一段预算(单条放得下、两条放不下)。
    input.policy_text = "p".to_string();
    input.tool_definitions = Vec::new();
    input.instructions = "i".to_string();
    input.query = "q".to_string();
    input.budget = BudgetPolicy {
        max_context_tokens: 40,
        ..BudgetPolicy::default()
    };
    input.history = vec![
        HistoryItem {
            history_id: "h-old".to_string(),
            round: 1,
            role: momotaro_context::HistoryRole::User,
            body: "x".repeat(80),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
        HistoryItem {
            history_id: "h-new".to_string(),
            round: 2,
            role: momotaro_context::HistoryRole::User,
            body: "y".repeat(80),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let replayed: Vec<&str> = result
        .messages
        .iter()
        .filter(|m| m.role.as_str() == "user")
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        replayed.iter().any(|c| c.contains("yyyy")),
        "the recent turn survives: {replayed:?}"
    );
    let budget_stage = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "enforce_token_budget")
        .expect("stage present");
    assert!(
        budget_stage
            .omitted
            .iter()
            .any(|o| o.item == "h-old" && o.reason == OmittedReason::BudgetExhausted),
        "the old turn is an explicit budget drop: {:?}",
        budget_stage.omitted
    );
}

/// 去重的第二半:history 已携带全文的 chunk(hash 记在 referenced_chunk_hashes)
/// 即使 session.injected 为空,也不得重复进 evidence 占预算。
#[test]
fn history_referenced_chunks_dedup_without_session_injection() {
    let mut input = base_input();
    input.session = SessionState::default();
    input.history = vec![HistoryItem {
        history_id: "h-1".to_string(),
        round: 1,
        role: momotaro_context::HistoryRole::User,
        body: "earlier turn already carrying the full text of h1".to_string(),
        summary: SummarySlot::None,
        referenced_chunk_hashes: vec!["h1".to_string()],
    }];
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:a.md", 9.0),
            "full text",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h2", "note:b.md", 5.0),
            "new text",
            OriginClass::Owner,
            None,
        ),
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let order: Vec<&str> = result.evidence_order().collect();
    assert_eq!(order, vec!["h2"], "history-carried text is deduped out");
    let dedup = &result.stages[5];
    assert!(
        dedup
            .omitted
            .iter()
            .any(|o| o.item == "h1" && o.reason == OmittedReason::PreviouslyInjected),
        "the drop is explicit: {:?}",
        dedup.omitted
    );
}

/// 预算 level 3 短路:新的一轮被 BudgetExhausted 后,更旧的一轮不得插队存活。
#[test]
fn an_older_turn_never_survives_when_a_newer_one_was_dropped() {
    let mut input = base_input();
    input.policy_text = "p".to_string();
    input.tool_definitions = Vec::new();
    input.instructions = "i".to_string();
    input.query = "q".to_string();
    input.budget = BudgetPolicy {
        max_context_tokens: 40,
        ..BudgetPolicy::default()
    };
    input.history = vec![
        HistoryItem {
            history_id: "h-old".to_string(),
            round: 1,
            role: momotaro_context::HistoryRole::User,
            body: "x".repeat(40),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
        HistoryItem {
            history_id: "h-new".to_string(),
            round: 2,
            role: momotaro_context::HistoryRole::User,
            body: "y".repeat(320),
            summary: SummarySlot::None,
            referenced_chunk_hashes: Vec::new(),
        },
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let budget_stage = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "enforce_token_budget")
        .expect("stage present");
    assert!(
        budget_stage.omitted.iter().any(|o| o.item == "h-new"),
        "the big new turn is dropped: {:?}",
        budget_stage.omitted
    );
    assert!(
        budget_stage.omitted.iter().any(|o| o.item == "h-old"),
        "the older turn must not sneak past: {:?}",
        budget_stage.omitted
    );
    assert!(!result.final_user_body().contains("xxxx"));
}

/// evidence 兜底是**全局下界 3**,不是档位下界——survey 在紧预算下
/// 也只保 3 条,不因 tier_min=8 强行放大。
#[test]
fn the_evidence_floor_is_three_even_for_survey_under_a_tight_budget() {
    let mut input = base_input();
    input.intent = QueryIntent::Survey;
    input.budget = BudgetPolicy {
        max_context_tokens: 8,
        ..BudgetPolicy::default()
    };
    input.evidence = (1..=8)
        .map(|n| {
            evidence(
                hit(
                    n,
                    &format!("c{n}"),
                    &format!("h{n}"),
                    &format!("note:{n}.md"),
                    9.0 - n as f64,
                ),
                &format!("b{n}"),
                OriginClass::Owner,
                None,
            )
        })
        .collect();
    let result = build_context_with_clock(input, &|| 0u64);
    let order: Vec<&str> = result.evidence_order().collect();
    assert_eq!(order.len(), 3, "floor 3 is the global bound: {order:?}");
    // 断在选择阶段:预算 floor 会把终值也剪回 3,只有 select 记录能区分
    // 「全局底 3」与「档位底 8」两说。
    let select = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "select_evidence")
        .expect("stage present");
    assert_eq!(
        select.included.len(),
        3,
        "the selection itself must stop at the global floor: {:?}",
        select.included
    );
}

/// request_bytes 的黄金字节:线格式是 run 层取 request_hash 的契约,布局
/// 突变必须红。
#[test]
fn the_request_wire_layout_is_pinned_by_golden_bytes() {
    let mut input = base_input();
    input.policy_text = "p".to_string();
    input.tool_definitions = Vec::new();
    input.instructions = "i".to_string();
    input.query = "q".to_string();
    let result = build_context_with_clock(input, &|| 0u64);
    let expected = "momotaro.request.v1
tools []
msg system 1
p
msg user 4
i

q
";
    assert_eq!(String::from_utf8(result.request_bytes()).unwrap(), expected);
}

/// BudgetReport.exceeded 两向都要有断言。
#[test]
fn budget_exceeded_is_reported_both_ways() {
    let mut over = base_input();
    over.budget = BudgetPolicy {
        max_context_tokens: 10,
        ..BudgetPolicy::default()
    };
    let result = build_context_with_clock(over, &|| 0u64);
    assert!(
        result.budget.exceeded,
        "fixed parts over budget report exceeded"
    );

    let mut fine = base_input();
    fine.policy_text = "p".to_string();
    fine.tool_definitions = Vec::new();
    fine.instructions = "i".to_string();
    fine.query = "q".to_string();
    let result = build_context_with_clock(fine, &|| 0u64);
    assert!(
        !result.budget.exceeded,
        "tiny content fits the default budget"
    );

    // The cell neither direction above lands on. `adjusted_total` is
    // `ceil(raw * coefficient)`, so a ceiling set to the value it produces is
    // constructible — and `>` and `>=` disagree on exactly that one input:
    // equal to the ceiling is *not* over it.
    let ceiling = result.budget.adjusted_total;
    assert!(ceiling > 0, "the fixture has something to measure");
    let mut edge = base_input();
    edge.policy_text = "p".to_string();
    edge.tool_definitions = Vec::new();
    edge.instructions = "i".to_string();
    edge.query = "q".to_string();
    edge.budget = BudgetPolicy {
        max_context_tokens: u32::try_from(ceiling).expect("a small ceiling"),
        ..BudgetPolicy::default()
    };
    let edge = build_context_with_clock(edge, &|| 0u64);
    assert_eq!(
        edge.budget.adjusted_total, ceiling,
        "the fixed parts are never dropped, so the boundary really is this"
    );
    assert!(
        !edge.budget.exceeded,
        "on the line is not over the line — only `>` gets that cell right"
    );
}

/// 预算 level 4 的锁存(floor 之后):第 4 条大到必败,第 5 条小到本可塞入,
/// 也不得插队存活(与 history 级的 `an_older_turn_never_survives...` 孪生)。
#[test]
fn evidence_level_stops_claiming_after_the_first_failure() {
    let mut input = base_input();
    input.policy_text = "p".to_string();
    input.tool_definitions = Vec::new();
    input.instructions = "i".to_string();
    input.query = "q".to_string();
    input.budget = BudgetPolicy {
        max_context_tokens: 80,
        ..BudgetPolicy::default()
    };
    input.history = vec![HistoryItem {
        history_id: "t1".to_string(),
        round: 1,
        role: momotaro_context::HistoryRole::User,
        body: "h".repeat(120),
        summary: SummarySlot::None,
        referenced_chunk_hashes: Vec::new(),
    }];
    // 分数安排让 rank 阶段产出 [h1, h2, h3, h4, h5]:h4(大)在申领第 4 位
    // 失败,h5(小)本可塞入但被锁存弃掉。
    input.evidence = vec![
        evidence(
            hit(1, "c1", "h1", "note:1.md", 5.0),
            "x",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(2, "c2", "h2", "note:2.md", 3.0),
            "x",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(3, "c3", "h3", "note:3.md", 2.0),
            "x",
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(4, "c4", "h4", "note:4.md", 1.0),
            &"x".repeat(32),
            OriginClass::Owner,
            None,
        ),
        evidence(
            hit(5, "c5", "h5", "note:5.md", 4.0),
            "xx",
            OriginClass::Owner,
            None,
        ),
    ];
    let result = build_context_with_clock(input, &|| 0u64);
    let budget_stage = result
        .stages
        .iter()
        .find(|s| s.stage.as_str() == "enforce_token_budget")
        .expect("stage present");
    assert!(
        budget_stage.omitted.iter().any(|o| o.item == "h4"),
        "the oversized fourth item is dropped: {:?}",
        budget_stage.omitted
    );
    assert!(
        budget_stage.omitted.iter().any(|o| o.item == "h5"),
        "the smaller fifth item must not sneak past: {:?}",
        budget_stage.omitted
    );
}
