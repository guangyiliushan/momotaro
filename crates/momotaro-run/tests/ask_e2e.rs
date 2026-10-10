//! The ask run end to end, against the checked-in fixture vault and the fake
//! provider rig (`architecture.md` §19.4: fixture vault → ingest → search →
//! fake LLM → citation → trace replay).
//!
//! The crash injection lives here, in tests, not in a manual procedure: a
//! child process runs an ask whose provider call parks forever, the parent
//! kills it (`TerminateProcess` — this platform's `kill -9` equivalent) and
//! reopens the store, and the run must read back `failed(interrupted)` with
//! its partial output annotated (`harness.md` §8.1).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use momotaro_llm::fake::FakeLlm;
use momotaro_llm::{ProviderTransport, TransportError, TransportReply};
use momotaro_run::{AskOptions, ask_workspace, index_workspace, init_workspace, trace_workspace};
use momotaro_store::Store;

/// Copies the fixture vault into `<workspace>/vault`.
fn stage_vault(workspace: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vault")
        .canonicalize()
        .expect("fixture vault exists");
    let target = workspace.join("vault");
    copy_tree(&source, &target);
    target
}

fn copy_tree(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("create vault dir");
    for entry in std::fs::read_dir(source).expect("read fixture vault") {
        let entry = entry.expect("dir entry");
        let to = target.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).expect("copy fixture file");
        }
    }
}

fn write_config(workspace: &Path) {
    std::fs::write(
        workspace.join("momotaro.toml"),
        "[vault]\npath = 'vault'\n\n[model]\nprovider = 'fake'\nmodel_id = 'fake-model'\n",
    )
    .expect("write momotaro.toml");
}

/// A ready-to-ask workspace: staged vault, indexed store.
/// The options a test asks with: network permission granted, everything else
/// default. `ask` is a provider call, and architecture §7 keeps the network
/// off until it is allowed — a test that wants an answer has to allow it.
fn network_allowed() -> AskOptions {
    AskOptions {
        allow_network: true,
        ..AskOptions::default()
    }
}

fn ready_workspace(tempdir: &tempfile::TempDir) {
    let workspace = tempdir.path();
    stage_vault(workspace);
    write_config(workspace);
    init_workspace(workspace).expect("init");
    index_workspace(workspace, None).expect("index");
}

/// A completion body in the provider's wire shape, carrying `content`.
fn completion_body(content: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5},
    })
    .to_string()
}

/// A contiguous quote span of `text` that the citation protocol can carry
/// (no `]`, no line break — the block line's grammar).
fn quote_span(text: &str) -> String {
    let mut quote = String::new();
    for c in text.trim_start().chars() {
        if c == ']' || c == '\n' || c == '\r' {
            break;
        }
        quote.push(c);
        if quote.chars().count() >= 24 {
            break;
        }
    }
    assert!(
        !quote.is_empty(),
        "the fixture chunk yields a quoteable span"
    );
    quote
}

/// Scripts the fake with a cited answer quoting the workspace's actual
/// rank-1 hit for `query`: the test never guesses which chunk ranks first
/// (the fixture's `long-mixed.md` owns the 谱定理 term) and never guesses
/// the fragment numbers — `f0003` is the first evidence fragment of a build
/// whose session is empty (the builder numbers policy `f0001`, tool
/// definitions `f0002`, then evidence in rank order). If either the
/// numbering or the retrieval moves, the citation verdict goes red, loudly.
fn enqueue_cited_reply(fake: &FakeLlm, workspace: &Path, query: &str) -> String {
    let search = momotaro_run::search_workspace(workspace, query, 10).expect("search");
    let hit = &search.hits[0];
    let store = Store::open(workspace.join(".momotaro/momotaro.db")).expect("open");
    let chunk = store
        .get_chunk(&hit.hit.chunk_id)
        .expect("chunk")
        .expect("row");
    let quote = quote_span(&chunk.text);
    let content = format!(
        "谱定理保证对称矩阵可以对角化，PCA 正是协方差矩阵的谱分解 [f0003]。

Citations:
[f0003] {quote}
"
    );
    fake.enqueue_reply(200, completion_body(&content));
    quote
}

#[test]
fn the_fifteen_steps_land_the_tables_and_the_answer_verifies() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    let quote = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");

    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert_eq!(report.status, "finished", "{report:?}");
    assert!(report.fully_verified, "{report:?}");
    assert_eq!(fake.call_count(), 1, "ask calls the provider exactly once");
    let answer_text = report.answer_text.as_deref().expect("answer text");
    assert!(
        answer_text.contains(&quote),
        "the answer is stored verbatim, quote included"
    );
    assert_eq!(report.citations.len(), 1);
    assert!(report.citations[0].verified, "{report:?}");
    assert_eq!(report.citations[0].quoted_text, quote);
    assert!(
        !report.citations[0].chunk_hash.is_empty(),
        "the citation carries the five-tuple"
    );

    // The three tables carry rows (W3.9's hard shape), and the timeline
    // carries the vocabulary in order.
    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    assert!(
        !trace.retrievals.is_empty() && !trace.retrievals[0].hits.is_empty(),
        "the hit table has rows"
    );
    assert!(
        !trace.context_items.is_empty(),
        "the context table has rows"
    );
    let answer = trace.answer.expect("the answer row exists");
    assert!(!answer.citations.is_empty(), "the citation table has rows");
    assert_eq!(trace.llm_turns.len(), 1);
    assert_eq!(trace.llm_turns[0].status, "finished");
    assert_eq!(trace.llm_turns[0].request_hash.len(), 64, "sha256 hex");
    assert_eq!(trace.verifications.len(), 2, "one citation + one integrity");
    assert_eq!(trace.history.len(), 2, "the user and assistant turns");

    // `RetrievalRecord::strategy_json` promises the triple `(engine, k, mode)`.
    // Only expand ever asserted `mode` before this; dropping ask's key left
    // the contract half-written with nothing to go red.
    let strategy: serde_json::Value =
        serde_json::from_str(&trace.retrievals[0].strategy_json).expect("strategy is JSON");
    assert_eq!(strategy["engine"], "tantivy_bm25");
    assert_eq!(
        strategy["mode"], "lexical",
        "ask retrieves with plain BM25 — the *retrieval* mode, not the run's"
    );
    assert_eq!(strategy["k"].as_u64(), Some(10), "the recorded top-k");

    let names = trace
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "run.accepted",
            "run.started",
            "retrieval.started",
            "retrieval.finished",
            "context.built",
            "llm.started",
            "llm.finished",
            "citation.validated",
            "answer.finalized",
            "run.finished",
        ],
        "the timeline carries every step of the fifteen"
    );

    // `llm.finished` names its slot, and both producers have to: `expand`
    // always did, `ask` did not, so a reader grouping this stream by slot
    // would silently drop the whole ask path. The turn row carries `slot` as
    // a column too — this is the event half of the same fact.
    let finished = trace
        .events
        .iter()
        .find(|event| event.event_type == "llm.finished")
        .expect("the exchange was recorded");
    let finished_payload: serde_json::Value =
        serde_json::from_str(&finished.data_json).expect("payload is JSON");
    assert_eq!(finished_payload["slot"], "answer", "{finished_payload}");

    // …and the payload that event carries is read, because nothing else does.
    // The two counts live inside `serde_json::json!`, where `cargo mutants`
    // generates no mutant at all — so `!=` becoming `==` would flip
    // `failed` from "nothing failed" to "everything failed" and the whole
    // workspace would stay green.
    let validated = trace
        .events
        .iter()
        .find(|event| event.event_type == "citation.validated")
        .expect("the timeline names it above");
    let payload: serde_json::Value =
        serde_json::from_str(&validated.data_json).expect("payload is JSON");
    assert_eq!(payload["citations"], 1, "{payload}");
    assert_eq!(
        payload["failed"], 0,
        "the one citation verified, so nothing failed in it: {payload}"
    );
}

#[test]
fn replaying_the_same_client_request_returns_the_same_run() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    enqueue_cited_reply(&fake, tempdir.path(), "谱定理");

    let options = AskOptions {
        client_request_id: Some("req-1".to_owned()),
        allow_network: true,
        ..AskOptions::default()
    };
    let first = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("first ask");
    // The replay must not even call the provider: same request, same run.
    let second = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("replay");
    assert_eq!(second.run_id, first.run_id, "the same run, not a new one");
    assert_eq!(fake.call_count(), 1, "the replay re-executes nothing");
    assert_eq!(second.status, first.status);
    assert_eq!(second.answer_text, first.answer_text);

    // And the store holds exactly one run for the key.
    let store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store.run_id_for_client_request("req-1").expect("read"),
        Some(first.run_id.clone())
    );
    assert_eq!(store.run_count().expect("count"), 1, "and no second run");
    // "Writes nothing" has to cover the conversation too: the session row is
    // minted on the first ask, and a replay that minted a second one would
    // leave a phantom conversation no id and no run ever points at.
    assert_eq!(
        store.sessions().expect("sessions").len(),
        1,
        "a replay mints no second session (`harness.md` §1)"
    );
}

#[test]
fn ask_under_no_llm_is_refused_before_a_run_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    let options = AskOptions {
        no_llm: true,
        client_request_id: Some("policy-1".to_owned()),
        allow_network: true,
        ..AskOptions::default()
    };
    let error = ask_workspace(tempdir.path(), "谱定理", &options, &fake)
        .expect_err("policy refuses ask without an LLM");
    assert!(
        matches!(error, momotaro_run::RunError::PolicyRefused(_)),
        "{error:?}"
    );
    assert_eq!(fake.call_count(), 0, "no provider call was made");
    // No run was created — checked against the key this request carries
    // (and by enumeration: any created run would dangle and sweep out),
    // not against an id no code path could have minted.
    let mut store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store.run_id_for_client_request("policy-1").expect("read"),
        None,
        "no run was created"
    );
    assert!(store.sweep_interrupted_runs().expect("sweep").is_empty());
}

#[test]
fn ask_without_network_permission_is_refused_before_a_run_exists() {
    // architecture §7 keeps the network off until it is allowed, and an `ask`
    // is a provider call. The engine must enforce that itself: the CLI has its
    // own gate, so only this test can tell the two apart.
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new(); // nothing enqueued: a call would panic the rig
    let options = AskOptions {
        client_request_id: Some("policy-network".to_owned()),
        ..AskOptions::default()
    };
    let error = ask_workspace(tempdir.path(), "谱定理", &options, &fake)
        .expect_err("policy refuses a call that would reach the network");
    assert!(
        matches!(error, momotaro_run::RunError::PolicyRefused(_)),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("--allow-network"), "{message}");
    assert!(message.contains("[policy].allow_network"), "{message}");
    assert_eq!(fake.call_count(), 0, "no provider call was made");

    let mut store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store
            .run_id_for_client_request("policy-network")
            .expect("read"),
        None,
        "no run was created"
    );
    assert!(store.sweep_interrupted_runs().expect("sweep").is_empty());
}

/// A child whose death is this test's responsibility: `Child`'s drop does
/// not kill, and a parked crash-child leaked by a failing assertion would
/// hold the workspace files open (the RED-path hygiene the mutation round
/// watched happen). The guard kills and waits whatever path exits early.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A provider whose reply never arrives: `send` parks until the process dies.
struct ParkingTransport;

impl ProviderTransport for ParkingTransport {
    fn send(&self, _body: &str) -> Result<TransportReply, TransportError> {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

/// The crash child's body. Ordinary runs return immediately (the env guard);
/// the parent spawns this test binary with the guard set, and kills it while
/// its ask is parked inside the provider call.
#[test]
fn crash_child_parks_the_ask_mid_run() {
    let Ok(workspace) = std::env::var("MOMOTARO_CRASH_CHILD") else {
        return;
    };
    let options = AskOptions {
        client_request_id: Some("crash-1".to_owned()),
        allow_network: true,
        ..AskOptions::default()
    };
    let _ = ask_workspace(&workspace, "谱定理", &options, &ParkingTransport);
    unreachable!("the parking transport never returns");
}

/// The W3.7 acceptance (`harness.md` §8.1): kill -9 mid-run, reopen the
/// store, and the run's terminal state is `failed(interrupted)` — with the
/// partial output annotated and `trace` still usable.
#[test]
fn a_killed_ask_reads_back_failed_interrupted_on_reopen() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let workspace = tempdir.path().canonicalize().expect("workspace path");

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "crash_child_parks_the_ask_mid_run",
                "--exact",
                "--nocapture",
            ])
            .env("MOMOTARO_CRASH_CHILD", &workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the crash child"),
    );

    // Wait (bounded) until the child's ask is parked in the provider call —
    // `llm.started` is the last event before the send. The poll reads the
    // store directly: it must not sweep a live run out from under the child.
    let db = workspace.join(".momotaro/momotaro.db");
    let mut run_id = None;
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(100));
        let Ok(store) = Store::open(&db) else {
            continue;
        };
        let Ok(Some(id)) = store.run_id_for_client_request("crash-1") else {
            continue;
        };
        let Ok(Some(trace)) = store.run_trace(&id) else {
            continue;
        };
        if trace
            .events
            .iter()
            .any(|event| event.event_type == "llm.started")
        {
            run_id = Some(id);
            break;
        }
    }
    let run_id = run_id.expect("the child reached the provider call in time");

    // The kill: `TerminateProcess` on this platform, `SIGKILL` semantics —
    // no unwinding, no cleanup, the database is left mid-run.
    child.0.kill().expect("kill the child");
    let _ = child.0.wait();

    // Reopen through the run engine (its open sweeps — harness §2) and read
    // the run back: terminal `failed(interrupted)`, partial output named.
    let trace = trace_workspace(&workspace, &run_id).expect("trace after the crash");
    assert_eq!(trace.run.status, "failed");
    assert_eq!(
        trace.run.failure_note.as_deref(),
        Some("interrupted after llm.started"),
        "the note names the last step that completed"
    );
    let terminal = trace.events.last().expect("terminal event");
    assert_eq!(terminal.event_type, "run.failed");
    assert_eq!(
        terminal.data_json,
        "{\"attribution\":\"system\",\"category\":\"interrupted\",\"retryable\":true,\"severity\":\"error\"}"
    );
    // The partial output is on the timeline: retrieval and context landed,
    // the provider exchange did not.
    assert!(!trace.retrievals.is_empty(), "the retrieval is annotated");
    assert!(!trace.context_items.is_empty(), "the context is annotated");
    assert!(
        trace.llm_turns.is_empty(),
        "no turn row for a turn that never landed"
    );
    assert!(trace.answer.is_none(), "no answer for an interrupted ask");
}

/// The failure matrix (DoD 7: 引用失败显式标记，不伪装成功). Every citation
/// failure keeps the answer on record and marks the verdict explicitly —
/// `architecture.md` §14.1.5: 引用失败时回答可以输出，但不是 fully verified.
mod citation_failures {
    use super::*;

    fn ask_with_content(workspace: &Path, content: &str) -> momotaro_run::AskReport {
        let fake = FakeLlm::new();
        fake.enqueue_reply(200, completion_body(content));
        ask_workspace(workspace, "谱定理", &network_allowed(), &fake).expect("ask runs")
    }

    #[test]
    fn a_quote_that_is_not_in_the_chunk_fails_by_name() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let report = ask_with_content(
            tempdir.path(),
            "answer [f0003]

Citations:
[f0003] this span is not in the chunk
",
        );
        assert_eq!(report.status, "failed", "{report:?}");
        assert!(!report.fully_verified);
        // The answer is still output, with its failure spelled.
        assert!(report.answer_text.is_some(), "the answer is not swallowed");
        assert_eq!(report.citations.len(), 1);
        assert_eq!(report.citations[0].status, "failed");
        assert_eq!(
            report.citations[0].detail.as_deref(),
            Some("text_not_locatable"),
            "the judgement names itself"
        );
        assert_eq!(
            report.failure_note.as_deref(),
            Some("1 of 1 citations failed verification"),
            "the partial output is annotated (multi-agent §5.1)"
        );
    }

    #[test]
    fn a_fabricated_marker_fails_as_citation_invalid() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let report = ask_with_content(
            tempdir.path(),
            "answer [f9999]

Citations:
[f9999] a quote from nowhere
",
        );
        assert_eq!(report.status, "failed");
        assert_eq!(
            report.citations[0].detail.as_deref(),
            Some("citation_invalid")
        );
    }

    #[test]
    fn a_marker_without_its_block_line_fails_explicitly() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let report = ask_with_content(tempdir.path(), "answer [f0003] cited but never quoted");
        assert_eq!(report.status, "failed");
        assert_eq!(
            report.citations[0].detail.as_deref(),
            Some("text_not_locatable"),
            "the empty quote is an explicit failure, not a silent pass"
        );
    }

    #[test]
    fn a_malformed_block_line_fails_instead_of_hiding() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let report = ask_with_content(
            tempdir.path(),
            "answer [f0003]\n\n\nCitations:\n[f0003] a real quote\nnot a citation line",
        );
        // The good citation may pass; the malformed line is an explicit
        // failure, never silently dropped (architecture 14.1 rule 4).
        assert_eq!(report.status, "failed", "{report:?}");
        let junk = report
            .citations
            .iter()
            .find(|citation| citation.source_key.is_empty())
            .expect("the malformed line is recorded as an unverifiable claim");
        assert_eq!(junk.detail.as_deref(), Some("citation_invalid"));
    }

    #[test]
    fn an_empty_retrieval_fails_as_retrieval_empty() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let fake = FakeLlm::new();
        let report = ask_workspace(
            tempdir.path(),
            "zzz-no-such-term-anywhere",
            &network_allowed(),
            &fake,
        )
        .expect("ask runs");
        assert_eq!(report.status, "failed");
        assert_eq!(
            report.failure_note.as_deref(),
            Some("retrieval returned no hits")
        );
        assert_eq!(
            fake.call_count(),
            0,
            "no provider call for an empty retrieval"
        );
        // And the failure lands with its §18 category on the timeline.
        let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
        let terminal = trace.events.last().expect("terminal");
        assert_eq!(terminal.event_type, "run.failed");
        assert!(
            terminal
                .data_json
                .contains("\"category\":\"retrieval_empty\""),
            "{}",
            terminal.data_json
        );
    }

    #[test]
    fn an_empty_answer_body_fails_without_swallowing_the_turn() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        let report = ask_with_content(tempdir.path(), "   ");
        assert_eq!(report.status, "failed");
        assert_eq!(
            report.failure_note.as_deref(),
            Some("the provider returned no answer text")
        );
        // The exchange itself is on record (its usage columns are the spend).
        let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
        assert_eq!(trace.llm_turns.len(), 1);
        assert_eq!(trace.llm_turns[0].status, "finished");
        assert!(trace.answer.is_none(), "no answer row for an empty body");
    }
}

/// The failure matrix's provider half: an exchange that never lands still
/// gets its turn row (its usage columns are the attempt's record) and its
/// §18 category on the terminal event.
#[test]
fn a_provider_failure_is_recorded_and_categorised() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    // Two failures: the one permitted retry (identical bytes, before any
    // observable output) also fails — then the exchange honestly fails. A
    // single failure with a scripted success afterwards is the retry path,
    // and would rightly finish.
    fake.enqueue_failure(false, "connection reset");
    fake.enqueue_failure(false, "connection reset again");
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert_eq!(report.status, "failed");
    assert_eq!(
        report.failure_note.as_deref(),
        Some("provider exchange failed (llm_provider)")
    );
    assert_eq!(fake.call_count(), 2, "the one permitted retry, then done");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    assert_eq!(trace.llm_turns.len(), 1, "one exchange, one turn row");
    assert_eq!(trace.llm_turns[0].status, "failed");
    let terminal = trace.events.last().expect("terminal");
    assert_eq!(terminal.event_type, "run.failed");
    let terminal_payload: serde_json::Value =
        serde_json::from_str(&terminal.data_json).expect("terminal payload is JSON");
    assert_eq!(
        terminal_payload["category"], "llm_provider",
        "{terminal_payload}"
    );
    // The rest of the §18 object, read back from the terminal write: a
    // literal attribution here (`"system"`) survived every test built before
    // this one — only `category` was asserted.
    assert_eq!(
        terminal_payload["attribution"], "provider",
        "{terminal_payload}"
    );
    assert_eq!(terminal_payload["retryable"], true, "{terminal_payload}");
    assert_eq!(terminal_payload["severity"], "error", "{terminal_payload}");

    // The §18 object under `llm.finished` is the other place this layer writes
    // it, and until now nothing read it back — the two sites had retyped its
    // four fields by hand next to the builder that already had them, so a field
    // renamed in one place left the other quietly wrong.
    let finished = trace
        .events
        .iter()
        .find(|event| event.event_type == "llm.finished")
        .expect("the turn that failed was recorded");
    let payload: serde_json::Value =
        serde_json::from_str(&finished.data_json).expect("payload is JSON");
    assert_eq!(payload["error"]["category"], "llm_provider", "{payload}");
    assert_eq!(payload["error"]["attribution"], "provider", "{payload}");
    assert_eq!(payload["error"]["retryable"], true, "{payload}");
    assert_eq!(
        payload["error"]["severity"],
        momotaro_contracts::Severity::Error.as_str(),
        "{payload}"
    );
    assert_eq!(payload["turn_id"], trace.llm_turns[0].turn_id, "{payload}");
    assert_eq!(payload["slot"], "answer", "{payload}");
}

/// An exchange that fails **after** the provider reported tokens still spends
/// them.
///
/// `commit_run_llm_turn`'s own contract says the turn row lands for failed
/// exchanges and "its usage columns are the record of what the attempt spent";
/// `contracts` reads a NULL in those columns as *not reported*. A 2xx body
/// that is not a readable completion carries no `choices`, so the exchange
/// fails — but `best_effort_usage` salvages its `usage` onto the error
/// precisely so it can be written here. Dropping it would file reported
/// tokens as unreported.
#[test]
fn a_failed_exchange_that_reported_usage_still_records_it_on_the_turn() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    // No `choices`, so this is a malformed failure — and `output_observed`,
    // so it is off the table for the permitted retry: one call.
    fake.enqueue_reply(
        200,
        r#"{"usage": {"prompt_tokens": 7, "completion_tokens": 9, "cached_tokens": 3, "billable_tokens": 6, "completion_tokens_details": {"reasoning_tokens": 4}}}"#,
    );
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert_eq!(report.status, "failed", "{report:?}");
    assert_eq!(fake.call_count(), 1, "a body that was seen is not retried");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let turn = trace
        .llm_turns
        .first()
        .expect("the turn row lands on a failure too");
    assert_eq!(turn.status, "failed");
    assert_eq!(
        turn.input_tokens,
        Some(7),
        "the provider reported 7 input tokens: {turn:?}"
    );
    assert_eq!(
        turn.output_tokens,
        Some(9),
        "…and 9 output tokens: {turn:?}"
    );
    // The cache half and the thinking tokens are the columns the contract
    // means by "the record of what the attempt spent" — and they were the
    // three a whole battery found written and never read: a mutant dropping
    // any of them left the entire workspace green. Each is asserted here, by
    // name, against the numbers this body reports.
    assert_eq!(turn.cached_tokens, Some(3), "{turn:?}");
    assert_eq!(turn.billable_tokens, Some(6), "{turn:?}");
    assert_eq!(turn.reasoning_tokens, Some(4), "{turn:?}");
    assert_eq!(turn.slot.as_deref(), Some("answer"), "{turn:?}");

    // `reference.md` promises the cache summary on `llm.finished` whenever the
    // provider reports one — and this body did. The turn-row columns are
    // asserted above (the `cached_tokens` / `billable_tokens` / `reasoning_tokens`
    // block); this is the event half of the same numbers.
    let finished = trace
        .events
        .iter()
        .find(|event| event.event_type == "llm.finished")
        .expect("the exchange was recorded");
    let payload: serde_json::Value =
        serde_json::from_str(&finished.data_json).expect("payload is JSON");
    assert_eq!(payload["cached_tokens"], 3, "{payload}");
    assert_eq!(payload["billable_tokens"], 6, "{payload}");
}

/// The rewrite exchange records **all** of what the provider reported —
/// every one of the five usage columns, and the slot it ran in.
///
/// `contracts` reads a NULL on those columns as *not reported*, so a dropped
/// write files provider numbers as unreported. The five-column assignment is
/// written in four places (ask's success, ask's failure and expand's failure
/// through `record_failure_usage`, and this one), which is exactly why each
/// landing spot is read back here instead of trusted.
#[test]
fn the_expand_turn_records_the_thinking_tokens_the_provider_reported() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    fake.enqueue_reply(
        200,
        r#"{"id":"chatcmpl-expand","choices":[{"message":{"content":"[\"谱定理\"]"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":9,"cached_tokens":3,"billable_tokens":6,"completion_tokens_details":{"reasoning_tokens":4}}}"#,
    );

    let output =
        momotaro_run::expand_search_workspace(tempdir.path(), "谱定理", 10, false, true, &fake)
            .expect("expand runs");
    let run_id = output.run_id.expect("expand creates a run");
    let trace = trace_workspace(tempdir.path(), &run_id).expect("trace");
    let turn = trace.llm_turns.first().expect("one exchange, one turn row");
    assert_eq!(turn.status, "finished", "{turn:?}");
    assert_eq!(turn.input_tokens, Some(7), "{turn:?}");
    assert_eq!(turn.output_tokens, Some(9), "{turn:?}");
    assert_eq!(
        turn.reasoning_tokens,
        Some(4),
        "the thinking tokens the provider reported: {turn:?}"
    );
    assert_eq!(turn.cached_tokens, Some(3), "{turn:?}");
    assert_eq!(turn.billable_tokens, Some(6), "{turn:?}");
    assert_eq!(
        turn.raw_ref.as_deref(),
        Some("chatcmpl-expand"),
        "the provider's response id is recorded verbatim: {turn:?}"
    );
    assert_eq!(
        turn.slot.as_deref(),
        Some("answer"),
        "the turn row records the slot it ran in: {turn:?}"
    );

    // …and the event names it too: `expand` always wrote `"slot"` here, `ask`
    // did not until both were pinned, and a reader grouping by slot would drop
    // whichever producer forgot it.
    let finished = trace
        .events
        .iter()
        .find(|event| event.event_type == "llm.finished")
        .expect("the exchange was recorded");
    let payload: serde_json::Value =
        serde_json::from_str(&finished.data_json).expect("payload is JSON");
    assert_eq!(payload["slot"], "answer", "{payload}");
    assert_eq!(payload["cached_tokens"], 3, "{payload}");
    assert_eq!(payload["billable_tokens"], 6, "{payload}");
    assert_eq!(payload["turn_id"], turn.turn_id, "{payload}");
}

/// Both stages an expand run goes through are bracketed.
///
/// `ask` emits `retrieval.started` and `llm.started`; expand emitted only the
/// two `.finished` halves, so its timeline had two dangling ends — and both
/// spellings are **stable** events in `reference.md`, while `append_run_event`'s
/// own doc says those two exist precisely for the moments that write no table
/// rows. `run_events` is a canonical, append-only audit table: the same named
/// pair must not be half present depending on which of the two run-creating
/// flows wrote it.
#[test]
fn the_expand_run_brackets_both_of_its_stages() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    fake.enqueue_reply(
        200,
        r#"{"choices":[{"message":{"content":"[\"谱定理\"]"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":9}}"#,
    );

    let output =
        momotaro_run::expand_search_workspace(tempdir.path(), "谱定理", 10, false, true, &fake)
            .expect("expand runs");
    let run_id = output.run_id.expect("expand creates a run");
    let trace = trace_workspace(tempdir.path(), &run_id).expect("trace");
    let names = trace
        .events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "run.accepted",
            "run.started",
            "llm.started",
            "llm.finished",
            "retrieval.started",
            "retrieval.finished",
            "run.finished",
        ],
        "every stage bracketed, in the order they run: {names:?}"
    );
}

/// A successful exchange writes all five usage columns — and the slot.
///
/// The failure half is pinned in
/// `a_failed_exchange_that_reported_usage_still_records_it_on_the_turn`; this
/// is the success half, where the numbers come off `output.usage` instead of
/// being salvaged. Either landing spot left alone would file reported tokens
/// as "not reported", which is what `contracts` says a NULL means.
#[test]
fn a_successful_exchange_records_every_usage_column_on_the_turn() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // Every column the provider can report, so a write that goes missing is a
    // number that goes missing — not a NULL standing for "unreported" either
    // way.
    let fake = FakeLlm::new();
    fake.enqueue_reply(
        200,
        r#"{"id":"chatcmpl-ask","choices":[{"message":{"content":"an answer with no citations"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":5,"cached_tokens":1,"billable_tokens":2,"completion_tokens_details":{"reasoning_tokens":7}}}"#,
    );
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert_eq!(report.status, "finished", "{report:?}");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let turn = trace.llm_turns.first().expect("one exchange, one turn row");
    assert_eq!(turn.status, "finished", "{turn:?}");
    assert_eq!(turn.input_tokens, Some(3), "{turn:?}");
    assert_eq!(turn.output_tokens, Some(5), "{turn:?}");
    assert_eq!(turn.cached_tokens, Some(1), "{turn:?}");
    assert_eq!(turn.billable_tokens, Some(2), "{turn:?}");
    assert_eq!(turn.reasoning_tokens, Some(7), "{turn:?}");
    assert_eq!(
        turn.raw_ref.as_deref(),
        Some("chatcmpl-ask"),
        "the provider's response id is recorded verbatim: {turn:?}"
    );
    assert_eq!(turn.slot.as_deref(), Some("answer"), "{turn:?}");

    // …and the event half carries the cache summary `reference.md` promises.
    // The failure half of this promise has had a read-back since round 12; the
    // success half had none, so replacing its two keys with `null` — writing
    // the event as if the provider reported nothing — left everything green.
    let finished = trace
        .events
        .iter()
        .find(|event| event.event_type == "llm.finished")
        .expect("the exchange was recorded");
    let payload: serde_json::Value =
        serde_json::from_str(&finished.data_json).expect("payload is JSON");
    assert_eq!(payload["cached_tokens"], 1, "{payload}");
    assert_eq!(payload["billable_tokens"], 2, "{payload}");
    assert_eq!(payload["turn_id"], turn.turn_id, "{payload}");
}

/// A retrieval that cannot run (the index is gone) fails the run with
/// `index_unavailable` — the run was accepted and then failed, on the record.
#[test]
fn an_unusable_index_fails_the_run_as_index_unavailable() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    std::fs::remove_dir_all(tempdir.path().join(".momotaro/index")).expect("remove index");

    let fake = FakeLlm::new();
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert_eq!(report.status, "failed");
    assert_eq!(
        report.failure_note.as_deref(),
        Some("retrieval could not run")
    );
    assert_eq!(fake.call_count(), 0);
    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let terminal = trace.events.last().expect("terminal");
    assert!(
        terminal
            .data_json
            .contains("\"category\":\"index_unavailable\""),
        "{}",
        terminal.data_json
    );
}

/// DoD 8: the index is a derived artifact — rebuilding it changes no run
/// fact. The trace after a rebuild is the trace before it.
#[test]
fn rebuilding_the_index_leaves_run_history_untouched() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let quote = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
    assert!(report.fully_verified, "{report:?}");
    let before = trace_workspace(tempdir.path(), &report.run_id).expect("trace");

    momotaro_run::rebuild_index(tempdir.path()).expect("rebuild");

    let after = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    assert_eq!(before, after, "the rebuild touched no run fact");
    assert!(
        after
            .answer
            .as_ref()
            .expect("answer")
            .citations
            .iter()
            .any(|citation| citation.quoted_text == quote),
        "the citation survives byte for byte"
    );
}

/// Q18 / H3: a `summary.v1` row whose stored text is not `summary.v1` JSON
/// must fail closed — never be replayed as a turn. Folding it into "this row
/// is not a summary" would put raw JSON in front of the model, which is the
/// shape the three-state slot exists to make impossible.
///
/// The row is planted through the **public** store API rather than raw SQL:
/// this is a shape a compressor can really produce, so a test that needed
/// hand-written SQL to build it would only be testing an unreachable state.
#[test]
fn a_malformed_summary_row_never_reaches_the_model_as_a_turn() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    // Turn 1 opens the session and leaves its two history rows behind.
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first = ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask");
    let session_id = first
        .session_id
        .clone()
        .expect("ask opens a session when none was named");

    // Plant a summary row for that session whose body parses as nothing.
    const PLANTED_AT: i64 = 2_000_000_000;
    let planted_run = "run-planted-summary".to_owned();
    let mut store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open store");
    store
        .commit_run_create(
            &momotaro_store::NewRun {
                run_id: planted_run.clone(),
                session_id: Some(session_id.clone()),
                parent_run_id: None,
                project_id: None,
                attempt: 1,
                mode: momotaro_contracts::RunMode::Ask.as_str().to_owned(),
                query: "plant".to_owned(),
                client_request_id: None,
                run_spec_hash: "h".to_owned(),
                run_spec_json: "{}".to_owned(),
                provider: Some("fake".to_owned()),
                model: Some("fake-model".to_owned()),
                context_window: Some(32_768),
                max_output_tokens: Some(4096),
                created_at: PLANTED_AT,
            },
            &momotaro_store::NewRunEvent {
                event_type: momotaro_contracts::run_event::ACCEPTED.to_owned(),
                data_json: "{}".to_owned(),
                created_at: PLANTED_AT,
            },
        )
        .expect("plant a run for the row to live on");
    store
        .commit_run_answer(
            &planted_run,
            &momotaro_contracts::AnswerRecord {
                answer_id: "ans-planted".to_owned(),
                run_id: planted_run.clone(),
                text: "planted".to_owned(),
                prompt_hash: "ph".to_owned(),
                created_at: PLANTED_AT,
                decision_rationale: None,
                assumptions: None,
                supersedes: None,
                citations: Vec::new(),
            },
            &[momotaro_contracts::HistoryItemRecord {
                history_id: "hist-planted-summary".to_owned(),
                session_id: Some(session_id.clone()),
                run_id: planted_run.clone(),
                seq: 1,
                // `kind` and `content_kind` are separate vocabularies
                // (`turn | summary` vs `text | summary.v1`) and neither column
                // has a CHECK, so this row deliberately does **not** agree with
                // itself: `content_kind` says summary.v1, `kind` says turn.
                // `history_item` reads `content_kind`, and this is the only
                // fixture that can tell — a reader switched to `kind` would
                // take the row for a raw turn and replay the malformed body
                // straight into the request.
                kind: "turn".to_owned(),
                role: "user".to_owned(),
                content_kind: "summary.v1".to_owned(),
                body: "{not summary.v1 at all".to_owned(),
                payload_json: "{}".to_owned(),
                source_ref_json: None,
                token_estimate: 8,
                created_at: PLANTED_AT,
                occurred_at: Some(PLANTED_AT),
            }],
            &momotaro_store::NewRunEvent {
                event_type: momotaro_contracts::run_event::ANSWER_FINALIZED.to_owned(),
                data_json: "{}".to_owned(),
                created_at: PLANTED_AT,
            },
        )
        .expect("plant the summary row");
    // Terminal, or the next `ask` refuses the session as already having an
    // active run (`harness.md` §5).
    store
        .commit_run_terminal(
            &planted_run,
            momotaro_contracts::TerminalEvent::Finished,
            "{}",
            None,
            PLANTED_AT,
        )
        .expect("terminate the planted run");
    drop(store);

    // Turn 2 resumes the session and must replay it without the bad row.
    fake.enqueue_reply(200, completion_body("no citation in this reply"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id.clone()),
        ..AskOptions::default()
    };
    let second =
        ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("ask resumes the session");
    assert_eq!(
        second.session_id.as_deref(),
        Some(session_id.as_str()),
        "the resumed run reports the session it continued"
    );

    assert_eq!(fake.call_count(), 2, "two provider calls, one per turn");
    let request = fake.sent_body(1);
    assert!(
        !request.contains("{not summary.v1 at all"),
        "the malformed summary never reaches the model as a turn; first 400 bytes: {}",
        &request[..request.len().min(400)]
    );
    assert!(
        request.contains("谱定理保证对称矩阵可以对角化"),
        "the raw turns do replay, so this really is a multi-round request"
    );
}

/// The four columns `ask` writes onto the session row, read back.
///
/// Nothing could read them until `Store::sessions` existed: `session_exists`
/// answers a boolean, so an empty title or a lost first question passed every
/// test in the repo. They are the fields `NewSession` documents as a list
/// preview, so they are only worth writing if they come back.
#[test]
fn the_session_row_ask_mints_reads_back_whole() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");

    let store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    let sessions = store.sessions().expect("sessions");
    assert_eq!(sessions.len(), 1, "one ask opens one conversation");
    let row = &sessions[0];
    assert_eq!(
        row.session_id,
        report.session_id.clone().expect("ask opens a session"),
        "and it is the id the report hands the next turn"
    );
    assert_eq!(
        row.title, "谱定理",
        "the title is the question, kept whole here"
    );
    assert_eq!(
        row.title_source, "derived",
        "and its provenance is recorded"
    );
    assert_eq!(
        row.first_user_message.as_deref(),
        Some("谱定理"),
        "the first question is kept verbatim"
    );
    let run = store
        .run_trace(&report.run_id)
        .expect("trace reads")
        .expect("the run exists");
    assert_eq!(
        row.created_at, run.run.created_at,
        "created at the same instant as the run that opened it"
    );
}

/// `hit_inclusion`'s third spelling, through the whole rendering rather than
/// through the pure function.
///
/// `ask` can never produce it: every stage that drops evidence records an
/// omission, and `WorkItem::key` *is* the `chunk_hash` for evidence, so a hit
/// that did not reach the request always comes back with a reason. The only
/// writer of `(included: false, omitted_reason: None)` in the tree is
/// `--mode expand` — no context stage runs for a search (`index_service.rs`
/// says so in the line that writes it) — so the shape is reachable only from
/// that trace, and an `ask`-only test cannot get there however it is
/// arranged.
#[test]
fn expand_renders_every_hit_as_not_included_because_no_context_stage_ran() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    fake.enqueue_reply(200, completion_body("fourier transform syntax"));

    let output =
        momotaro_run::expand_search_workspace(tempdir.path(), "谱定理", 10, false, true, &fake)
            .expect("expand runs");
    assert!(!output.hits.is_empty(), "the fixture vault yields hits");
    let run_id = output.run_id.clone().expect("a search that runs is a run");

    let trace = trace_workspace(tempdir.path(), &run_id).expect("trace");
    // The frozen spec pins the slot on the search side too — it is the same
    // constant the turn row and the event payload name, and each of those
    // three landing spots is a separate write.
    let spec: serde_json::Value =
        serde_json::from_str(&trace.run.run_spec_json).expect("the spec is JSON");
    assert_eq!(spec["slot"], serde_json::json!("answer"), "{spec}");
    assert_eq!(
        spec["thinking"],
        serde_json::json!("medium"),
        "the frozen spec carries the default thinking tier: {spec}"
    );
    let rendered = momotaro_run::format_trace(&trace);
    let rows: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains(" score "))
        .collect();
    assert_eq!(
        rows.len(),
        output.hits.len(),
        "every retrieval row reaches the rendered trace: {rendered}"
    );
    for row in rows {
        assert!(
            row.contains("— not included"),
            "no context stage ran, so nothing was included and no stage omitted \
             anything: {row}"
        );
    }
    assert!(
        !rendered.contains("— included"),
        "and the two reason-carrying spellings stay off this trace: {rendered}"
    );
}

/// `sessions.last_activity_at` is documented as「recency 排序，区别于
/// created_at」(`architecture.md`). A column set once at insert and never
/// again contradicts its own comment from the second turn on — and it would
/// be invisible, because until `Store::sessions` existed nothing could read
/// it back. This is both the behaviour witness and the proof the column is
/// not write-only.
#[test]
fn a_continued_session_advances_last_activity_but_never_created_at() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("first turn");
    let session_id = first.session_id.clone().expect("a session opens");
    let opened = first;

    // `now_secs` is second-granularity, so two turns inside the same second
    // leave `last_activity_at == created_at` no matter how correct the write
    // is. Cross the boundary explicitly rather than let the test pass or fail
    // on how busy the machine was.
    std::thread::sleep(std::time::Duration::from_millis(1_100));

    fake.enqueue_reply(200, completion_body("a second answer, no citations"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id.clone()),
        ..AskOptions::default()
    };
    let second = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("second turn");

    let store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    let sessions = store.sessions().expect("sessions");
    assert_eq!(sessions.len(), 1, "both turns share one conversation");
    let row = &sessions[0];
    let origin = store
        .run_trace(&opened.run_id)
        .expect("trace reads")
        .expect("the first run exists")
        .run
        .created_at;
    assert_eq!(
        row.created_at, origin,
        "`created_at` is the creation instant and never moves"
    );
    assert!(
        row.last_activity_at.unwrap_or_default() > row.created_at,
        "the second turn moved `last_activity_at` past it: {row:?}"
    );
    // The clamp is `max(seen, answer.created_at)` — the **answer's** instant,
    // not the run's. The two land in the same second most of the time, which
    // is exactly why asserting the run's `created_at` here flaked on a busy
    // machine: `ask_workspace` can start a run in one second and write its
    // answer in the next. The sleep above already guarantees the second
    // answer strictly dominates the first turn's stamp, so the clamp's value
    // is the answer's — that is the number to compare against.
    let continued_answer_at = store
        .run_trace(&second.run_id)
        .expect("trace reads")
        .expect("the second run exists")
        .answer
        .expect("the second turn answered")
        .created_at;
    assert_eq!(
        row.last_activity_at,
        Some(continued_answer_at),
        "…and it landed on the turn's answer instant, which is what the \
         clamp is max() against — not some other time"
    );
}

/// Which revision a verification judged is the whole point of the row —
/// without it a `source_integrity` failure says "these bytes are wrong" about
/// something no reader can name. The coordinate comes from
/// `distinct_revisions(citations)`, and nothing observed it: returning
/// `[("", "")]` left every test in the workspace green while every row's
/// `details_json` claimed an empty source and an empty revision.
#[test]
fn every_verification_row_names_the_revision_it_judged() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let answer = trace.answer.as_ref().expect("the run saved an answer");
    assert!(
        !answer.citations.is_empty(),
        "the fixture cites something, so there is a coordinate set to check against"
    );
    let coordinates: std::collections::HashSet<(&str, &str)> = answer
        .citations
        .iter()
        .map(|citation| {
            (
                citation.source_key.as_str(),
                citation.revision_hash.as_str(),
            )
        })
        .collect();

    // `verifications` holds two shapes: per-citation rows (keyed by
    // `citation_index`) and per-revision rows (keyed by the coordinate
    // `distinct_revisions` supplies). Only the second kind is under test.
    let judged: Vec<&momotaro_contracts::VerificationRecord> = trace
        .verifications
        .iter()
        .filter(|row| row.details_json.contains("source_key"))
        .collect();
    assert!(
        !judged.is_empty(),
        "no row names a revision, so nothing observes `distinct_revisions`: {:?}",
        trace
            .verifications
            .iter()
            .map(|row| (row.kind.as_str(), row.status.as_str()))
            .collect::<Vec<_>>()
    );
    for row in judged {
        let details: serde_json::Value =
            serde_json::from_str(&row.details_json).expect("details are JSON");
        let source_key = details["source_key"]
            .as_str()
            .unwrap_or_else(|| panic!("the row names its source: {row:?}"));
        let revision_hash = details["revision_hash"]
            .as_str()
            .unwrap_or_else(|| panic!("the row names its revision: {row:?}"));
        assert!(
            !source_key.is_empty() && !revision_hash.is_empty(),
            "…and neither half is empty: {row:?}"
        );
        assert!(
            coordinates.contains(&(source_key, revision_hash)),
            "the row names a revision the answer never cited: {coordinates:?} — {row:?}"
        );
    }
}

/// The frozen spec is what a later config edit cannot rewrite
/// (`architecture.md` §6.4), so a permission flag dropped on the way in is a
/// permission decision silently forgotten. `allow_write` merges from two
/// sources — the flag and `[policy]` — and the merge was never observed: `||`
/// → `&&` left every test in the workspace green while the stored spec
/// reported `false` for a run launched with `--allow-write`.
#[test]
fn the_frozen_spec_records_the_write_flag_it_was_given() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");

    let options = AskOptions {
        allow_network: true,
        allow_write: true,
        ..AskOptions::default()
    };
    let report = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("ask runs");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let spec: serde_json::Value =
        serde_json::from_str(&trace.run.run_spec_json).expect("the spec is JSON");
    assert_eq!(
        spec["slot"],
        serde_json::json!("answer"),
        "the frozen spec pins the slot too: {spec}"
    );
    assert_eq!(
        spec["thinking"],
        serde_json::json!("medium"),
        "the frozen spec carries the default thinking tier too: {spec}"
    );
    assert_eq!(
        spec["allow_write"],
        serde_json::json!(true),
        "`--allow-write` has to survive into the frozen spec: {spec}"
    );
    assert_eq!(
        spec["allow_network"],
        serde_json::json!(true),
        "…and the flag it shares a line with, as the control: {spec}"
    );
    // The turn row's slot cell is written from the same constant; it is read
    // back on the turn row in
    // `a_successful_exchange_records_every_usage_column_on_the_turn`.
}

/// W3.10's B8: `hit_inclusion` had only unit assertions — nothing drove the
/// whole `format_trace` rendering, so a renderer that dropped the retrieval
/// section (or printed its own word instead of the three spellings) would
/// leave every unit test green.
#[test]
fn the_rendered_trace_carries_every_retrieval_hit_and_its_treatment() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let report =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");

    let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
    let rendered = momotaro_run::format_trace(&trace);

    let total: usize = trace
        .retrievals
        .iter()
        .map(|retrieval| retrieval.hits.len())
        .sum();
    assert!(total > 0, "the fixture vault yields hits");
    // `--json` already carries `session_id`; the human rendering has to name
    // the conversation too, or a trace cannot say which session it is a turn
    // of — and `format_trace` has two exits, so one of them dropping the line
    // would be invisible to every other assertion here.
    let session_id = report.session_id.clone().expect("ask opens a session");
    assert!(
        rendered
            .lines()
            .any(|line| line == format!("session {session_id}")),
        "the human trace names the session it belongs to: {rendered}"
    );
    assert_eq!(
        rendered
            .lines()
            .filter(|line| line.contains(" score "))
            .count(),
        total,
        "every retrieval row reaches the rendered trace"
    );

    // Each row must carry one of `hit_inclusion`'s three spellings verbatim —
    // "not included" contains "included", so the needles keep the em-dash and
    // the word before it distinct.
    for retrieval in &trace.retrievals {
        for hit in &retrieval.hits {
            let treatment = match (hit.included, hit.omitted_reason.as_deref()) {
                (true, _) => "— included",
                (false, Some(_)) => "— omitted (",
                (false, None) => "— not included",
            };
            assert!(
                rendered
                    .lines()
                    .any(|line| line.contains(&hit.chunk_id) && line.contains(treatment)),
                "hit {} must render as {treatment:?}",
                hit.chunk_id
            );
        }
    }
}

/// W3.11 ③: the two `[policy]` knobs reach the build, not merely the parser.
/// The trigger line recorded on `context.built` is computed from *this*
/// config — a build that silently fell back to the 0.85/8192 defaults would
/// record a different number.
#[test]
fn the_policy_knobs_reach_the_compaction_trigger() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let path = tempdir.path().join("momotaro.toml");
    let base = std::fs::read_to_string(&path).expect("read config");

    // Runs one `ask` under `[policy] … compact_trigger_ratio = {ratio}` and
    // returns `context.built`'s `compaction` payload.
    let compaction_under = |ratio: &str| -> serde_json::Value {
        let mut text = base.clone();
        text.push_str(&format!(
            "\n[policy]\nmax_context_tokens = 4096\n\
             compact_trigger_ratio = {ratio}\ncompact_reserve_abs = 0\n"
        ));
        std::fs::write(&path, text).expect("write config");
        let fake = FakeLlm::new();
        let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
        let report =
            ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
        let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
        let built = trace
            .events
            .iter()
            .find(|event| event.event_type.as_str() == "context.built")
            .expect("the build is recorded");
        let payload: serde_json::Value = serde_json::from_str(&built.data_json).expect("json");
        payload["compaction"].clone()
    };

    let compaction = compaction_under("0.4");
    assert_eq!(
        compaction["reserve"].as_u64(),
        Some(0),
        "min(0, 25% of 4096): {compaction}"
    );
    assert_eq!(
        compaction["trigger"].as_u64(),
        Some(1638),
        "min(4096 - 0, floor(0.4 × 4096)) — from this config, not from the \
         built-in 0.85 / 8192 defaults: {compaction}"
    );
    let raw_total = compaction["raw_total"]
        .as_u64()
        .expect("the estimate the line was compared against is recorded too");
    assert_eq!(
        compaction["max_context_tokens"].as_u64(),
        Some(4096),
        "the ceiling the verdict is priced in rides along too — neither \
         `adjusted_total` nor `triggered` can be read back out of: {compaction}"
    );
    assert!(
        compaction["triggered"].is_boolean(),
        "`triggered` is the field §4's 观测三指标 will count, so it has to be \
         there and it has to be a verdict: {compaction}"
    );

    // The verdict, both ways — and in the **right unit**: `trigger` is priced
    // in `max_context_tokens`, the adjusted ceiling, so the test drives the
    // line strictly *between* the two estimates, where only a comparison
    // against `adjusted_total` can see it. The arithmetic and why it costs a
    // firing point are in `reference.md` (Event vocabulary) and
    // `context-retrieval.md` §4; at §2.1's coefficient the band between the
    // two is a quarter of the raw estimate wide, so this is not a knife edge.
    let adjusted_total = compaction["adjusted_total"]
        .as_u64()
        .expect("`adjusted_total` rides along so the verdict can be checked");
    assert_eq!(
        adjusted_total,
        ((raw_total as f64) * 1.25).ceil() as u64,
        "the two estimates are related by §2.1's documented coefficient: {compaction}"
    );
    assert!(
        adjusted_total > raw_total,
        "…and strictly larger, or this test cannot tell the two units apart: \
         {compaction}"
    );

    let between = raw_total + 1;
    assert!(
        between <= adjusted_total && adjusted_total < 4096,
        "the fixture has to leave room between the two units and stay under the \
         budget: raw {raw_total}, adjusted {adjusted_total}"
    );
    assert_eq!(
        compaction_under(&format!("{:.6}", (between as f64 + 0.5) / 4096.0))["triggered"],
        serde_json::json!(true),
        "a line above `raw_total` and at or below `adjusted_total` fires — only \
         the adjusted comparison can see it (raw {raw_total}, adjusted \
         {adjusted_total}, trigger {between})"
    );

    // The boundary cell itself: a build exactly **on** the line fires.
    // Everything above and below it looks identical to `>=` and to `>`, so
    // this is the only cell that can tell them apart — and nothing automated
    // could see it, because `cargo mutants` does not descend into
    // `serde_json::json!` macro bodies: `--list` shows not one mutant in the
    // range this verdict is written in, so the headline behaviour of this
    // whole wave had zero in-diff coverage.
    assert_eq!(
        compaction_under(&format!("{:.6}", (adjusted_total as f64 + 0.5) / 4096.0))["triggered"],
        serde_json::json!(true),
        "on the line counts: `adjusted_total >= trigger` fires when they are equal, \
         and `>` would report false for a build sitting exactly on it \
         (raw {raw_total}, adjusted {adjusted_total})"
    );

    let above = adjusted_total + 1;
    assert_eq!(
        compaction_under(&format!("{:.6}", (above as f64 + 0.5) / 4096.0))["triggered"],
        serde_json::json!(false),
        "a line above `adjusted_total` does not fire: nothing this build reaches \
         (raw {raw_total}, adjusted {adjusted_total}, trigger {above})"
    );

    assert_eq!(
        compaction_under("1.0")["triggered"],
        serde_json::json!(false),
        "trigger = 4096: no build this small reaches the ceiling"
    );
    assert_eq!(
        compaction_under("0.001")["triggered"],
        serde_json::json!(true),
        "trigger = floor(0.001 × 4096) = 4: every build crosses it"
    );
}

/// D26: the freeze writes the **configured** semantic tier, not a constant.
/// `thinking = "high"` must reach `run_spec_json` as `"high"`, and the
/// default `auto` as its answer-slot default `"medium"` — a mapping or a
/// freeze that hardcodes either value turns one half of this red.
#[test]
fn the_frozen_spec_carries_the_configured_thinking_tier() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let path = tempdir.path().join("momotaro.toml");

    let frozen_thinking = |thinking: &str| -> serde_json::Value {
        let text = format!(
            "[vault]\npath = 'vault'\n\n[model]\nprovider = 'fake'\n\
             model_id = 'fake-model'\nthinking = '{thinking}'\n"
        );
        std::fs::write(&path, text).expect("write config");
        let fake = FakeLlm::new();
        let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
        let report =
            ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
        let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
        let spec: serde_json::Value =
            serde_json::from_str(&trace.run.run_spec_json).expect("the spec is JSON");
        spec["thinking"].clone()
    };

    assert_eq!(
        frozen_thinking("high"),
        serde_json::json!("high"),
        "the configured tier survives the freeze"
    );
    assert_eq!(
        frozen_thinking("auto"),
        serde_json::json!("medium"),
        "`auto` freezes as the answer slot's default"
    );
}

/// Every replayed history fragment carries `content_kind = text`. Flipping
/// the "is this row a summary?" test would mark the whole history
/// `summary.v1`, and nothing else in the build would notice.
#[test]
fn replayed_history_fragments_are_text_not_summary_v1() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    // Turn 1 opens the session; turn 2 is the one that has history to replay.
    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first = ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask");
    let session_id = first.session_id.clone().expect("a session opens");

    fake.enqueue_reply(200, completion_body("a second answer, no citations"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id),
        ..AskOptions::default()
    };
    let second =
        ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("ask resumes the session");

    let trace = trace_workspace(tempdir.path(), &second.run_id).expect("trace");
    let history = trace
        .context_items
        .iter()
        .filter(|item| item.kind == "history")
        .collect::<Vec<_>>();
    assert!(
        !history.is_empty(),
        "the resumed turn replays the previous turns as history fragments"
    );
    for item in history {
        assert_eq!(
            item.content_kind, "text",
            "no summary row exists yet, so every replayed turn is text"
        );
    }
}

/// The one refusal `usage.md` 多轮会话 promises — a typo'd `--session` never
/// mints a conversation.
///
/// The **ordering** half of `harness.md` §2/§5 (sweep first, so a crashed
/// session is reaped rather than blocked behind this check) is a separate
/// claim and has a separate test:
/// `a_typo_in_the_session_id_still_reaps_the_run_the_open_was_going_to_sweep`.
/// What this body proves is the other half: nothing else in the tree reaches
/// this branch, and a mutation that drops the guard mints a new conversation
/// on a typo.
#[test]
fn an_unknown_session_is_refused_before_anything_is_written() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();
    let options = AskOptions {
        allow_network: true,
        session_id: Some("ghost".to_owned()),
        ..AskOptions::default()
    };

    let error = ask_workspace(tempdir.path(), "谱定理", &options, &fake)
        .expect_err("there is no session named ghost");
    assert!(
        matches!(error, momotaro_run::RunError::PolicyRefused(_)),
        "a missing session is a refusal: {error}"
    );
    let text = error.to_string();
    assert!(text.contains("ghost"), "the refusal names the id: {text}");
    assert_eq!(fake.call_count(), 0, "not one token");

    let store = Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert!(
        !store.session_exists("ghost").expect("exists"),
        "a typo must not mint a conversation"
    );
    assert_eq!(store.run_count().expect("count"), 0, "and no run either");

    // The id is the one byte of caller input that reaches an error string,
    // and the CLI prints that string on both outlets. `PolicyRefused` was
    // `&'static str` before this wave, so nothing user-supplied could get
    // here; now it can, and a bidi override in a mistyped id must not reach
    // a terminal (or a log) on either path.
    let hostile = AskOptions {
        allow_network: true,
        session_id: Some("ghost\u{202e}42\u{200b}".to_owned()),
        ..AskOptions::default()
    };
    let error = ask_workspace(tempdir.path(), "谱定理", &hostile, &fake)
        .expect_err("still no such session");
    let text = error.to_string();
    assert!(
        !text.contains('\u{202e}') && !text.contains('\u{200b}'),
        "control and bidi characters are stripped on the way out: {text:?}"
    );
    assert!(text.contains("ghost42"), "the id is still readable: {text}");
    assert_eq!(store.run_count().expect("count"), 0, "and still no run");
}

/// `harness.md` §5 and the comment in `ask_workspace` both say the sweep runs
/// **before** the session decision — §2 specifies the sweep itself and §8.1 is
/// the `kill -9` acceptance item, so neither is where the order lives. Until
/// now nothing could tell: the refusal test above never planted an orphan, and
/// the orphan test below uses a session that exists, so both orders pass it. Moving
/// `sweep_interrupted_runs()` below the gate left every test in the workspace
/// green — this is the fixture that cannot.
///
/// Only one combination distinguishes the two orders: an orphan **and** a
/// session id that does not exist. The orphan alone is not enough (the gate
/// opens either way), and the typo alone is not enough (nothing is left to
/// reap).
#[test]
fn a_typo_in_the_session_id_still_reaps_the_run_the_open_was_going_to_sweep() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    let db = tempdir.path().join(".momotaro/momotaro.db");
    const ORPHANED_AT: i64 = 2_000_000_000;
    {
        let mut store = Store::open(&db).expect("open");
        store
            .commit_run_create(
                &momotaro_store::NewRun {
                    run_id: "run-abandoned".to_owned(),
                    session_id: Some("sess-before".to_owned()),
                    parent_run_id: None,
                    project_id: None,
                    attempt: 1,
                    mode: momotaro_contracts::RunMode::Ask.as_str().to_owned(),
                    query: "died mid-flight".to_owned(),
                    client_request_id: None,
                    run_spec_hash: "h".to_owned(),
                    run_spec_json: "{}".to_owned(),
                    provider: None,
                    model: None,
                    context_window: None,
                    max_output_tokens: None,
                    created_at: ORPHANED_AT,
                },
                &momotaro_store::NewRunEvent {
                    event_type: momotaro_contracts::run_event::ACCEPTED.to_owned(),
                    data_json: "{}".to_owned(),
                    created_at: ORPHANED_AT,
                },
            )
            .expect("plant an abandoned run");
    }

    // A typo, which returns **before** it could possibly have swept anything.
    let options = AskOptions {
        allow_network: true,
        session_id: Some("ghost".to_owned()),
        ..AskOptions::default()
    };
    ask_workspace(tempdir.path(), "谱定理", &options, &fake)
        .expect_err("there is no session named ghost");

    let store = Store::open(&db).expect("reopen");
    let trace = store
        .run_trace("run-abandoned")
        .expect("trace reads")
        .expect("the planted run exists");
    assert_eq!(
        trace.run.status, "failed",
        "the open swept on its way to the refusal — `harness.md` §8.1 is 
         reachable from `--session` even when the id it was given is wrong. 
         Move the sweep below the session gate and this run is still `accepted`"
    );
    assert!(
        trace
            .events
            .iter()
            .any(|event| event.event_type == "run.failed"),
        "…as a `run.failed(interrupted)`, not by disappearing"
    );
    assert!(
        !store.session_exists("ghost").expect("exists"),
        "and the typo still minted nothing"
    );
}

#[test]
fn a_crashed_run_in_the_session_is_swept_before_the_session_takes_another_turn() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first = ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask");
    let session_id = first.session_id.clone().expect("a session opens");

    // A run left `accepted` with no terminal event — the shape a crash (or an
    // `Err` after acceptance) leaves behind, and the reason `harness.md` §8.1
    // promises the next open reaps it. `--session` is the entry point that
    // actually blocks on this, so it is the one that has to sweep.
    let db = tempdir.path().join(".momotaro/momotaro.db");
    const ORPHANED_AT: i64 = 2_000_000_000;
    let mut store = Store::open(&db).expect("open");
    store
        .commit_run_create(
            &momotaro_store::NewRun {
                run_id: "run-orphaned".to_owned(),
                session_id: Some(session_id.clone()),
                parent_run_id: None,
                project_id: None,
                attempt: 1,
                mode: momotaro_contracts::RunMode::Ask.as_str().to_owned(),
                query: "died mid-flight".to_owned(),
                client_request_id: None,
                run_spec_hash: "h".to_owned(),
                run_spec_json: "{}".to_owned(),
                provider: None,
                model: None,
                context_window: None,
                max_output_tokens: None,
                created_at: ORPHANED_AT,
            },
            &momotaro_store::NewRunEvent {
                event_type: momotaro_contracts::run_event::ACCEPTED.to_owned(),
                data_json: "{}".to_owned(),
                created_at: ORPHANED_AT,
            },
        )
        .expect("plant an orphaned run");
    drop(store);

    fake.enqueue_reply(200, completion_body("a second answer, no citations"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id.clone()),
        ..AskOptions::default()
    };
    let second = ask_workspace(tempdir.path(), "谱定理", &options, &fake)
        .expect("the session is usable after its run crashed");

    assert_eq!(
        second.session_id.as_deref(),
        Some(session_id.as_str()),
        "and it continued the same conversation rather than opening a new one"
    );
    assert_eq!(fake.call_count(), 2, "one provider call per turn");

    // `run_trace` is read directly — `trace_workspace` sweeps on the way in
    // and would destroy the evidence.
    let store = Store::open(&db).expect("reopen");
    let trace = store
        .run_trace("run-orphaned")
        .expect("trace reads")
        .expect("the planted run exists");
    assert_eq!(
        trace.run.status, "failed",
        "the sweep reaped the orphan on the way in (`harness.md` §8.1)"
    );
    assert!(
        trace
            .events
            .iter()
            .any(|event| event.event_type == "run.failed"),
        "with an interrupted terminal event, not silently deleted"
    );
}

/// `derive_session_state`'s production witness.
///
/// Every assertion of that function until now called it directly and
/// bypassed `run_steps`, so swapping the call for `SessionState::default()`
/// left the whole workspace green — the ledger was wired in and nothing
/// proved it. Two halves are observable from outside the crate:
///
/// * **`fragment_seq`**, the counter the next build numbers its fragments
///   from. Restart it and the same `[fNNNN]` citation marker means two
///   different things inside one conversation. This is the half that shows
///   on any two-turn session, and it is what this test pins.
/// * **`injected`**, the blacklist the dedup stage reads. In-window turns
///   are protected a second time by `carried_chunk_hashes` — the history row
///   carries its own hashes — so on a two-turn conversation an empty
///   `injected` changes nothing; it only bites once a turn falls out of the
///   replay window (`context-retrieval.md` §3), which is what
///   `momotaro-context`'s release test covers at that layer.
#[test]
fn the_ledger_a_turn_rebuilds_carries_its_fragment_counter_forward() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("first turn");
    let session_id = first.session_id.clone().expect("a session opens");

    fake.enqueue_reply(200, completion_body("a second answer, no citations"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id),
        ..AskOptions::default()
    };
    let second = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("second turn");

    let opened = trace_workspace(tempdir.path(), &first.run_id).expect("trace 1");
    let resumed = trace_workspace(tempdir.path(), &second.run_id).expect("trace 2");
    let before: Vec<&str> = opened
        .context_items
        .iter()
        .map(|item| item.fragment_id.as_str())
        .collect();
    let after: Vec<&str> = resumed
        .context_items
        .iter()
        .map(|item| item.fragment_id.as_str())
        .collect();
    assert!(
        !before.is_empty(),
        "the first turn wrote fragments for the second to number past"
    );
    assert!(
        !after.is_empty(),
        "and the second turn wrote some — otherwise there is nothing to compare"
    );
    for fragment in &after {
        assert!(
            !before.contains(fragment),
            "{fragment} was already issued in turn 1 ({before:?}); the ledger's \
             `fragment_seq` did not carry forward, so `[fNNNN]` now names two \
             different things in one conversation"
        );
    }

    // The dedup half: same question twice, so identical retrieval hits — the
    // only thing that can keep turn 2 from re-sending them is the hashes the
    // session already holds.
    let carried = evidence_hashes(&opened);
    assert!(
        !carried.is_empty(),
        "the first turn really put evidence in front of the model"
    );
    let again = evidence_hashes(&resumed);
    let resent: Vec<&String> = again.iter().filter(|hash| carried.contains(hash)).collect();
    assert!(
        resent.is_empty(),
        "the second turn re-sent chunks the session already injected: {resent:?} \
         (turn 1 sent {carried:?})"
    );
    assert!(
        resumed
            .context_items
            .iter()
            .any(|item| item.kind == "history"),
        "the resumed turn still replayed the first as history: {} items",
        resumed.context_items.len()
    );
}

/// The `chunk_hash` of every evidence fragment a run put in front of the
/// model, in render order — read off `run_context_items`, i.e. what was
/// actually written, not what the build thought it did.
fn evidence_hashes(trace: &momotaro_contracts::RunTrace) -> Vec<String> {
    trace
        .context_items
        .iter()
        .filter(|item| item.kind == "evidence")
        .filter_map(|item| {
            let json = item.source_ref_json.as_deref()?;
            let value: serde_json::Value = serde_json::from_str(json).ok()?;
            value
                .get(momotaro_contracts::source_ref::CHUNK_HASH)?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

/// Turn 1's messages, replayed byte for byte in front of turn 2's, with
/// everything outside the messages array unchanged.
///
/// What this proves is the *content* of the replay: the store-loaded history,
/// the round axis and the `SummarySlot` mapping all have to line up for the
/// prefix to hold. The append-friendly **framing** over those messages
/// (`request_bytes`, the bytes `llm_turns.request_hash` is taken over) is
/// pinned separately in `momotaro-context`'s `prefix_stability.rs` and, with
/// the store round trip in it, in `ask.rs`'s
/// `a_session_round_trip_keeps_the_request_bytes_append_only`.
#[test]
fn two_real_turns_replay_the_first_request_in_front_of_the_second() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let fake = FakeLlm::new();

    let _ = enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
    let first =
        ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("first turn");
    let session_id = first.session_id.clone().expect("a session opens");

    fake.enqueue_reply(200, completion_body("a second answer, no citations"));
    let options = AskOptions {
        allow_network: true,
        session_id: Some(session_id),
        ..AskOptions::default()
    };
    let second = ask_workspace(tempdir.path(), "谱定理", &options, &fake).expect("second turn");

    assert_eq!(fake.call_count(), 2, "one provider call per turn");
    assert_ne!(
        second.run_id, first.run_id,
        "two turns are two runs in the one session they share"
    );
    let head: serde_json::Value =
        serde_json::from_str(&fake.sent_body(0)).expect("request 1 is JSON");
    let next: serde_json::Value =
        serde_json::from_str(&fake.sent_body(1)).expect("request 2 is JSON");

    let mut head_rest = head.clone();
    let head_messages = head_rest
        .as_object_mut()
        .expect("an object")
        .remove("messages")
        .expect("a messages array");
    let mut next_rest = next.clone();
    let next_rest_messages = next_rest
        .as_object_mut()
        .expect("an object")
        .remove("messages")
        .expect("a messages array");
    assert_eq!(
        head_rest, next_rest,
        "everything outside the messages array — system, tools, model, \
         sampling — has to be byte-identical or the cached head moves"
    );

    let turn1 = head_messages.as_array().expect("array");
    let turn2 = next_rest_messages.as_array().expect("array");
    assert!(
        turn2.len() > turn1.len(),
        "the second turn carries the first as history plus its own turn: {} vs {}",
        turn1.len(),
        turn2.len()
    );
    assert_eq!(
        &turn2[..turn1.len()],
        turn1,
        "turn 1's messages replay verbatim and in order — the store-loaded \
         history, the round axis and the ledger all have to line up for this \
         to hold"
    );
}

/// §L6 adjudication pin (I4-F1): the ask path's `llm_turns.request_hash` is
/// the **context stream** hash (`request_bytes()`) — model-free — not the
/// provider exchange's. Two identical workspaces asked with different
/// `model_id`s must record the same hash; a hash taken over the provider
/// request (which carries the model — the expand path's semantics) would
/// differ, and that is the mutation this pins.
#[test]
fn the_ask_request_hash_is_the_model_free_context_stream() {
    let read_hash = |model_id: &str| -> String {
        let tempdir = tempfile::tempdir().expect("tempdir");
        ready_workspace(&tempdir);
        std::fs::write(
            tempdir.path().join("momotaro.toml"),
            format!(
                "[vault]\npath = 'vault'\n\n[model]\nprovider = 'fake'\nmodel_id = '{model_id}'\n"
            ),
        )
        .expect("rewrite config");
        let fake = FakeLlm::new();
        enqueue_cited_reply(&fake, tempdir.path(), "谱定理");
        let report =
            ask_workspace(tempdir.path(), "谱定理", &network_allowed(), &fake).expect("ask runs");
        let trace = trace_workspace(tempdir.path(), &report.run_id).expect("trace");
        trace.llm_turns[0].request_hash.clone()
    };

    let first = read_hash("fake-model");
    let second = read_hash("another-model");
    assert_eq!(first.len(), 64, "sha256 hex");
    assert_eq!(
        first, second,
        "one context stream, two models, one hash — the ask path stays model-free"
    );
}
