//! The CLI surfaces of `ask`, `trace` and `stats` — the two outlets and the
//! five policy flags.
//!
//! The engine under them is driven in-process through the run service with
//! the fake provider rig; the tests then spawn the binary against the same
//! workspace, so the black-box assertions cover exactly what a script sees.

mod common;

use common::{run_cli, stage_vault, write_config};
use momotaro_llm::fake::FakeLlm;
use momotaro_run::{AskOptions, ask_workspace, index_workspace, init_workspace};

fn ready_workspace(tempdir: &tempfile::TempDir) {
    stage_vault(tempdir.path());
    write_config(tempdir.path());
    init_workspace(tempdir.path()).expect("init");
    index_workspace(tempdir.path(), None).expect("index");
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
    let report = ask_workspace(
        tempdir.path(),
        "谱定理",
        &AskOptions {
            allow_network: true,
            ..AskOptions::default()
        },
        &fake,
    )
    .expect("ask runs");
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
    //
    // `schema_version` and `database_path` were the two this loop only ever
    // checked for a KEY on the JSON side, while `format_stats` hand-writes
    // both into one `format!` template — so a wrong value, or a missed
    // `sanitize` on the path, had nothing anywhere to go red.
    let schema_version = match value["schema_version"].as_u64() {
        Some(version) => version.to_string(),
        None => "unknown".to_owned(),
    };
    // The cell below asserts the **line**, not the bare number: a needle of
    // `"9"` would be satisfied by any digit anywhere in the output.
    let schema_version_line = format!("schema version: {schema_version}");
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
        ("schema_version", schema_version_line.as_str()),
        (
            "database_path",
            value["database_path"].as_str().expect("database_path"),
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
    assert!(
        human.contains(&format!(
            "database: {}",
            value["database_path"].as_str().expect("database_path")
        )),
        "the database path renders its value, not just its label: {human}"
    );

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
fn ask_without_a_configured_endpoint_is_refused_before_any_run_row() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // The five policy flags parse; the refusal is the missing endpoint, not a
    // parse failure.
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
    assert!(!ok, "ask without an endpoint exits 1: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("provider unavailable"),
        "the refusal is named: {message}"
    );
    assert!(
        message.contains("[model].base_url_env"),
        "and it says which key would fix it: {message}"
    );
    // And nothing was accepted: the refusal happens before any run row. The
    // count is the witness — the sweep only ever sees `accepted` / `running`,
    // so a run that got as far as finishing would slip past it.
    let mut store =
        momotaro_store::Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store.run_count().expect("count"),
        0,
        "no run row was created"
    );
    let dangling = store.sweep_interrupted_runs().expect("sweep");
    assert!(dangling.is_empty(), "and nothing dangled: {dangling:?}");
}

#[test]
fn ask_names_the_variable_it_could_not_read() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    // The config names a variable; the process does not export it. The
    // refusal must name both halves, and still before any run row exists.
    let mut config =
        std::fs::read_to_string(tempdir.path().join("momotaro.toml")).expect("read config");
    config.push_str("base_url_env = 'MOMOTARO_TEST_BASE_URL'\n");
    std::fs::write(tempdir.path().join("momotaro.toml"), config).expect("write config");

    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &["ask", "--allow-network", "--json", "谱定理"],
    );
    assert!(!ok, "ask without the variable exits 1: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("MOMOTARO_TEST_BASE_URL"),
        "the missing variable is named: {message}"
    );
    assert!(
        message.contains("[model].base_url_env"),
        "and the config key that names it: {message}"
    );
}

#[test]
fn expand_search_without_an_endpoint_is_refused_before_any_llm_call() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &[
            "search",
            "--mode",
            "expand",
            "--allow-network",
            "--json",
            "谱定理",
        ],
    );
    assert!(
        !ok,
        "expand search exits 1 without an endpoint: {stdout}{stderr}"
    );
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("provider unavailable"),
        "the refusal is named: {message}"
    );
    assert!(
        message.contains("[model].base_url_env"),
        "and it says which key would fix it: {message}"
    );
}

#[test]
fn an_ask_given_a_search_mode_is_refused_by_name() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // `--mode` is search's retrieval mode: an ask that silently ignored it
    // would be an ask that lied about what it did. The refusal must come by
    // name on both exits, and before any run row exists.
    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["ask", "谱定理", "--mode", "expand"]);
    assert!(!ok, "ask with a search mode is refused: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("search's retrieval mode"),
        "the refusal names the flag: {message}"
    );

    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &["ask", "谱定理", "--mode", "expand", "--json"],
    );
    assert!(!ok, "the json exit refuses too: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("search's retrieval mode"),
        "the JSON exit says the same words: {message}"
    );

    let store =
        momotaro_store::Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store.run_count().expect("count"),
        0,
        "no run row was created"
    );
}

#[test]
fn expand_search_refuses_under_no_llm_by_name() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // The policy refusal must win over whatever the endpoint configuration
    // says: `--no-llm` is a statement about the request, not about this
    // workspace's setup.
    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &["search", "--mode", "expand", "--no-llm", "--json", "谱定理"],
    );
    assert!(!ok, "expand under --no-llm exits 1: {stdout}{stderr}");
    let message = format!("{stdout}{stderr}");
    assert!(message.contains("--mode expand"), "{message}");
    assert!(message.contains("--no-llm"), "{message}");
    assert!(
        !message.contains("provider unavailable"),
        "policy is answered before configuration: {message}"
    );
}

#[test]
fn ask_and_expand_refuse_without_allow_network() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    // Network is off until it is asked for, and a provider call is a network
    // call. The refusal must be the policy one, not whatever
    // the endpoint configuration would have said.
    for args in [
        vec!["ask", "--json", "谱定理"],
        vec!["search", "--mode", "expand", "--json", "谱定理"],
    ] {
        let (stdout, stderr, ok) = run_cli(tempdir.path(), &args);
        assert!(
            !ok,
            "{args:?} exits 1 without --allow-network: {stdout}{stderr}"
        );
        let message = format!("{stdout}{stderr}");
        assert!(message.contains("--allow-network"), "{message}");
        assert!(
            !message.contains("provider unavailable"),
            "policy is answered before configuration: {message}"
        );
    }

    // Lexical search never dials, so the gate does not apply to it.
    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["search", "--json", "谱定理"]);
    assert!(
        ok,
        "lexical search needs no network permission: {stdout}{stderr}"
    );
}

#[test]
fn lexical_search_is_unaffected_by_no_llm() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["search", "--no-llm", "--json", "谱定理"]);
    assert!(
        ok,
        "lexical search spends no tokens and must not be refused: {stdout}{stderr}"
    );
}

#[test]
fn trace_replays_a_run_in_both_outlets() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    ready_workspace(&tempdir);
    let run_id = planted_run(&tempdir);

    // The positive control for `run_count`: a finished run really is counted.
    // Without it the refusal tests would pass against a counter that always
    // answers zero.
    let store =
        momotaro_store::Store::open(tempdir.path().join(".momotaro/momotaro.db")).expect("open");
    assert_eq!(
        store.run_count().expect("count"),
        1,
        "planted_run created exactly one run"
    );

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
