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
        ask_workspace(tempdir.path(), "谱定理", &AskOptions::default(), &fake).expect("ask runs");
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
}

#[test]
fn replaying_the_same_client_request_returns_the_same_run() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    enqueue_cited_reply(&fake, tempdir.path(), "谱定理");

    let options = AskOptions {
        client_request_id: Some("req-1".to_owned()),
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
}

#[test]
fn ask_under_no_llm_is_refused_before_a_run_exists() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let fake = FakeLlm::new();
    let options = AskOptions {
        no_llm: true,
        client_request_id: Some("policy-1".to_owned()),
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
        ask_workspace(workspace, "谱定理", &AskOptions::default(), &fake).expect("ask runs")
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
            &AskOptions::default(),
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
        ask_workspace(tempdir.path(), "谱定理", &AskOptions::default(), &fake).expect("ask runs");
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
    assert!(
        terminal.data_json.contains("\"category\":\"llm_provider\""),
        "{}",
        terminal.data_json
    );
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
        ask_workspace(tempdir.path(), "谱定理", &AskOptions::default(), &fake).expect("ask runs");
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
        ask_workspace(tempdir.path(), "谱定理", &AskOptions::default(), &fake).expect("ask runs");
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
