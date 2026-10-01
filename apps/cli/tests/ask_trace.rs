//! The CLI surfaces of `ask`, `trace` and `stats` — the two outlets and the
//! five policy flags (`docs/docs/usage.md`).
//!
//! The engine under them is driven in-process through the run service with
//! the fake provider rig; the tests then spawn the binary against the same
//! workspace, so the black-box assertions cover exactly what a script sees.

use std::path::{Path, PathBuf};
use std::process::Command;

use momotaro_llm::fake::FakeLlm;
use momotaro_run::{AskOptions, ask_workspace, index_workspace, init_workspace};

fn stage_vault(workspace: &Path) {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/momotaro-run/tests/fixtures/vault")
        .canonicalize()
        .expect("fixture vault exists");
    copy_tree(&source, &workspace.join("vault"));
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
    let config = concat!(
        "[vault]\npath = 'vault'\n\n",
        "[model]\nprovider = 'fake'\nmodel_id = 'fake-model'\n",
    );
    std::fs::write(workspace.join("momotaro.toml"), config).expect("write momotaro.toml");
}

fn ready_workspace(tempdir: &tempfile::TempDir) {
    stage_vault(tempdir.path());
    write_config(tempdir.path());
    init_workspace(tempdir.path()).expect("init");
    index_workspace(tempdir.path(), None).expect("index");
}

fn run_cli(cwd: &Path, args: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_momotaro-cli"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("cli binary runs");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

/// A contiguous quote span the citation protocol can carry.
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
    quote
}

/// Runs one engine ask with the fake provider and returns its run id.
fn planted_run(tempdir: &tempfile::TempDir) -> String {
    let fake = FakeLlm::new();
    let search = momotaro_run::search_workspace(tempdir.path(), "谱定理", 10).expect("search");
    let store =
        momotaro_store::Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    let chunk = store
        .get_chunk(&search.hits[0].hit.chunk_id)
        .expect("chunk")
        .expect("row");
    let quote = quote_span(&chunk.text);
    let content =
        format!("谱定理保证对称矩阵可以对角化 [f0003]。\n\nCitations:\n[f0003] {quote}\n");
    let body = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5},
    })
    .to_string();
    fake.enqueue_reply(200, body);
    let report =
        ask_workspace(tempdir.path(), "谱定理", &AskOptions::default(), &fake).expect("ask runs");
    assert_eq!(report.status, "finished", "{report:?}");
    report.run_id
}

#[test]
fn stats_human_and_json_carry_the_same_fields() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());
    let (_, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["stats", "--json"]);
    assert!(ok, "stats --json should succeed: {stderr}");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stats --json prints JSON");

    let (human, stderr, ok) = run_cli(tempdir.path(), &["stats"]);
    assert!(ok, "stats should succeed: {stderr}");
    // 两出口字段一致: every field the JSON carries is named in the human
    // rendering, with its value.
    for (field, needle) in [
        ("status", value["status"].as_str().expect("status")),
        (
            "initialized",
            if value["initialized"].as_bool().expect("initialized") {
                "true"
            } else {
                "false"
            },
        ),
    ] {
        assert!(
            human.contains(needle),
            "the human line shows {field} ({needle}): {human}"
        );
    }
    for field in [
        "status",
        "schema_version",
        "database_path",
        "initialized",
        "sources",
        "chunks",
    ] {
        assert!(
            value.get(field).is_some(),
            "`{field}` is in the JSON: {value}"
        );
    }
    // The counts render their VALUES (not just their labels).
    for field in ["sources", "chunks"] {
        match value[field].as_u64() {
            Some(count) => assert!(
                human.contains(&format!("{field}: {count}")),
                "{field} shows its value on the human outlet: {human}"
            ),
            None => assert!(
                human.contains(&format!("{field}: unknown")),
                "{field} shows unknown, not a number: {human}"
            ),
        }
    }
    assert!(human.contains("schema version:"), "{human}");
    assert!(human.contains("database:"), "{human}");

    // And the unknown half of the rule (a missing count is never 0,
    // architecture 5.2 rule 12) where counts are unavailable: an
    // uninitialized workspace reports none.
    let bare = tempfile::tempdir().expect("tempdir");
    stage_vault(bare.path());
    write_config(bare.path());
    let (stdout, stderr, ok) = run_cli(bare.path(), &["stats", "--json"]);
    assert!(
        ok,
        "stats on an uninitialized workspace still reports: {stderr}"
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stats --json prints JSON");
    if value["sources"].is_null() {
        let (human, _, _) = run_cli(bare.path(), &["stats"]);
        assert!(
            human.contains("sources: unknown") && !human.contains("sources: 0"),
            "unknown stays unknown: {human}"
        );
    }
}

#[test]
fn ask_refuses_under_no_llm_before_anything_else() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["ask", "--no-llm", "--json", "谱定理"]);
    assert!(!ok, "ask under --no-llm exits 1: {stdout}{stderr}");
    assert!(
        stderr.contains("policy refused") || stdout.contains("policy refused"),
        "the refusal is named: {stdout}{stderr}"
    );
}

#[test]
fn ask_names_the_missing_provider_transport() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // The five policy flags parse; the refusal is the
    // transport seam, not a parse failure.
    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &[
            "ask",
            "--allow-network",
            "--allow-write",
            "--max-context-tokens",
            "512",
            "--json",
            "谱定理",
        ],
    );
    assert!(!ok, "ask without a transport exits 1: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("no provider transport in this build"),
        "the seam is named: {message}"
    );
    // And nothing was accepted: the refusal happens before any run row —
    // checked by enumeration (a created run would dangle and sweep out
    // here), not by looking up an id no code path could have minted.
    let mut store =
        momotaro_store::Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    let dangling = store.sweep_interrupted_runs().expect("sweep");
    assert!(dangling.is_empty(), "no run row was created: {dangling:?}");
}

#[test]
fn expand_search_is_refused_by_name() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &["search", "--mode", "expand", "--json", "谱定理"],
    );
    assert!(
        !ok,
        "expand search exits 1 without a transport: {stdout}{stderr}"
    );
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("search --mode expand"),
        "the mode is named in the refusal: {message}"
    );
}

#[test]
fn trace_replays_a_run_in_both_outlets() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let run_id = planted_run(&tempdir);

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["trace", &run_id, "--json"]);
    assert!(ok, "trace --json should succeed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("trace is JSON");
    for key in [
        "run",
        "events",
        "retrievals",
        "context_items",
        "llm_turns",
        "verifications",
        "answer",
        "history",
    ] {
        assert!(value.get(key).is_some(), "`{key}` is in the trace: {value}");
    }
    assert_eq!(value["run"]["run_id"], run_id);
    assert_eq!(value["run"]["status"], "finished");
    assert!(!value["retrievals"].as_array().expect("array").is_empty());
    assert!(!value["context_items"].as_array().expect("array").is_empty());
    assert!(
        !value["answer"]["citations"]
            .as_array()
            .expect("array")
            .is_empty()
    );

    let (human, stderr, ok) = run_cli(tempdir.path(), &["trace", &run_id]);
    assert!(ok, "trace should succeed: {stderr}");
    // The two outlets carry the same facts: the labels AND the values
    // behind them (a label without its value is what a hollow renderer
    // would print).
    for needle in [
        &run_id,
        "finished",
        "retrieval:",
        "model context:",
        "llm turns:",
        "verifications:",
        "citations:",
    ] {
        assert!(
            human.contains(needle),
            "the human trace shows {needle}: {human}"
        );
    }
    for value_needle in [
        value["retrievals"][0]["retrieval_id"].as_str().expect("id"),
        value["answer"]["answer_id"].as_str().expect("id"),
        value["answer"]["citations"][0]["source_key"]
            .as_str()
            .expect("key"),
    ] {
        assert!(
            human.contains(value_needle),
            "the human trace shows the same values: {value_needle} missing from {human}"
        );
    }
}

#[test]
fn trace_names_a_run_that_does_not_exist() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["trace", "run-ghost", "--json"]);
    assert!(!ok, "a missing run exits 1: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(message.contains("run-ghost"), "the id is named: {message}");
}
