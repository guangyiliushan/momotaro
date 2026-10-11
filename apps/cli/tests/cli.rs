//! End-to-end CLI tests against a copy of the checked-in fixture vault.
//!
//! The vault is staged *inside* the temporary workspace: every stored
//! path is workspace-relative, so a vault outside the workspace root is
//! refused by design.

mod common;

use common::{run_cli, stage_vault, write_config};
use std::process::Command;

#[test]
fn reconcile_exits_zero_on_a_healthy_workspace() {
    // The exit code is the contract a script gates on; the library returning `Ok` says nothing about this arm.
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");
    let _ = stdout;

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["reconcile", "--json"]);
    assert!(
        ok,
        "reconcile should exit 0 on a healthy workspace: {stderr}"
    );
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("reconcile stdout is JSON: {error}\n{stdout}"));
    for key in ["batch_id", "scanned", "changed", "removed", "events"] {
        assert!(value.get(key).is_some(), "`{key}` is in the report");
    }
    assert!(
        value["batch_id"]
            .as_str()
            .unwrap()
            .starts_with("reconcile-")
    );
}

/// `--mode` / `--allow-write` / `--max-context-tokens` are declared on the
/// subcommands that read them, not globally: a global flag is accepted
/// by every subcommand and ignored by all but one — which is how
/// `momotaro index --mode expand --allow-write --max-context-tokens 4096`
/// used to exit 0 with no effect at all. clap's refusal is the contract, so
/// that is what is asserted: re-globalizing any of the three turns these
/// commands green and this test red.
#[test]
fn a_flag_only_the_subcommand_that_reads_it_declares_is_refused_elsewhere() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());
    let (_, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");

    for args in [
        vec!["index", "--mode", "expand"],
        vec!["index", "--allow-write"],
        vec!["index", "--max-context-tokens", "4096"],
        vec!["search", "why is the sky blue?", "--allow-write"],
        vec![
            "search",
            "why is the sky blue?",
            "--max-context-tokens",
            "4096",
        ],
    ] {
        let (_stdout, stderr, ok) = run_cli(tempdir.path(), &args);
        assert!(
            !ok,
            "`{}` must not parse — it exited 0, which is the old silent \
             acceptance",
            args.join(" ")
        );
        assert!(
            stderr.contains("unexpected argument"),
            "the refusal names the argument it would not take: {stderr}"
        );
    }

    // The other half: the flags are still **accepted where they live**. Help
    // text cannot show this — a subcommand's description mentions `--mode`
    // while the top level takes it away — so the parse itself is what is
    // asked. `ask --no-llm` is refused by policy after parsing, which is a
    // different refusal from "unexpected argument"; that difference is the
    // assertion. Deleting the flags entirely would fail here.
    let (_stdout, stderr, _ok) = run_cli(
        tempdir.path(),
        &[
            "ask",
            "why is the sky blue?",
            "--mode",
            "lexical",
            "--allow-write",
            "--max-context-tokens",
            "4096",
            "--no-llm",
        ],
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "`ask` takes all three — the refusal that comes back is policy, not \
         parsing: {stderr}"
    );
    // …and it *is* that policy refusal, in `validate_ask_policy`'s one
    // spelling: asserting only the absence of a parse error would let any
    // other clap message through and still call it a pass.
    assert!(
        stderr.contains("--no-llm forbids"),
        "the parse succeeded and the policy refused it: {stderr}"
    );
    let (_stdout, stderr, _ok) = run_cli(
        tempdir.path(),
        &["search", "why is the sky blue?", "--mode", "lexical"],
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "`search` takes `--mode`: {stderr}"
    );
}

#[test]
fn watch_emits_a_batch_when_a_file_changes() {
    // `momotaro watch` once had no test at all, which is how a batch id, a
    // renderer and an exit path could all be wrong behind a green gate.
    // The reader thread keeps the child from blocking on a full pipe; the wait
    // is bounded rather than a fixed sleep, so a slow machine does not flake.
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());
    let (_, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");
    let (_, stderr, ok) = run_cli(tempdir.path(), &["index"]);
    assert!(ok, "index should succeed: {stderr}");

    let mut child = Command::new(env!("CARGO_BIN_EXE_momotaro-cli"))
        .args(["watch", "--json"])
        .current_dir(tempdir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("watch runs");
    let stdout = child.stdout.take().expect("stdout pipe");
    let collected: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let sink = collected.clone();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            sink.lock().expect("lock").push(line);
        }
    });

    // Give the startup reconciliation a moment, then change a file for real.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    std::fs::write(
        tempdir.path().join("vault").join("a.md"),
        "# A\n\nedited while watch was running\n",
    )
    .expect("write");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut saw_startup = false;
    let mut saw_batch = false;
    while std::time::Instant::now() < deadline && !(saw_startup && saw_batch) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let lines = collected.lock().expect("lock").clone();
        saw_startup = lines
            .iter()
            .any(|line| line.contains("\"kind\":\"reconciled\""));
        saw_batch = lines
            .iter()
            .any(|line| line.contains("\"kind\":\"batch\"") && line.contains("\"rebuilt\":1"));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let lines = collected.lock().expect("lock").clone();
    let joined = lines.join("\n");
    assert!(
        saw_startup,
        "the startup reconciliation is reported: {joined}"
    );
    assert!(
        saw_batch,
        "a real edit produces one batch that rebuilt one file: {joined}"
    );
}

#[test]
fn index_and_search_round_trip() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");
    assert!(stdout.contains("workspace initialized") || !stdout.trim().is_empty());

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["index"]);
    assert!(ok, "index should succeed: {stderr}");
    assert!(stdout.contains("indexed"), "index human output: {stdout}");

    let (stdout, stderr, ok) = run_cli(
        tempdir.path(),
        &["search", "傅里叶", "--json", "--top", "5"],
    );
    assert!(ok, "search should succeed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("search stdout is JSON: {error}\n{stdout}"));
    assert!(value.get("query").is_some(), "query key present");
    let hits = value.get("hits").and_then(|hits| hits.as_array());
    let hits = hits.expect("hits is an array");
    assert!(!hits.is_empty(), "expected hits for 傅里叶");
    let first_source = hits[0]
        .get("source_key")
        .and_then(|key| key.as_str())
        .expect("source_key on first hit");
    assert!(
        first_source.contains("fourier"),
        "first hit should be fourier.md, got {first_source}"
    );

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["search", "qqqqzzzz", "--json"]);
    assert!(ok, "no-match search should succeed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("search stdout is JSON: {error}\n{stdout}"));
    assert!(value.get("query").is_some(), "query key present");
    let hits = value.get("hits").and_then(|hits| hits.as_array());
    let hits = hits.expect("hits is an array");
    assert!(
        hits.is_empty(),
        "expected no hits for qqqqzzzz, got {hits:?}"
    );
}

#[test]
fn a_lexical_search_prints_the_human_outlet_on_every_platform() {
    // The hits half of the human outlet had one witness in this crate and it
    // is `#[cfg(target_os = "linux")]`, so on the Windows CI leg deleting the
    // rendering left the whole suite green. This one is not cfg-gated, and it
    // asserts shape rather than a substring a JSON payload could satisfy
    // (`"source_key": "note:..."` also contains `note:`).
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());

    let (_, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");
    let (_, stderr, ok) = run_cli(tempdir.path(), &["index"]);
    assert!(ok, "index should succeed: {stderr}");

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["search", "傅里叶"]);
    assert!(ok, "human search should succeed: {stderr}");
    assert!(
        !stdout.trim_start().starts_with('{'),
        "the human outlet is not the JSON one: {stdout}"
    );
    assert!(
        stdout.contains("score="),
        "a hit line from format_search_hit: {stdout}"
    );
    assert!(
        stdout.contains("note:"),
        "the source key that hit line names: {stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("    ")),
        "the excerpt line under each hit is indented: {stdout}"
    );

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["search", "qqqqzzzz"]);
    assert!(ok, "an empty search still exits 0: {stderr}");
    assert_eq!(
        stdout.trim(),
        "no results",
        "the empty outlet is one plain line: {stdout}"
    );
}

#[test]
fn rebuild_index_reports_chunks() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    stage_vault(tempdir.path());
    write_config(tempdir.path());

    let (_, stderr, ok) = run_cli(tempdir.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");
    let (_, stderr, ok) = run_cli(tempdir.path(), &["index"]);
    assert!(ok, "index should succeed: {stderr}");

    let (stdout, stderr, ok) = run_cli(tempdir.path(), &["rebuild-index"]);
    assert!(ok, "rebuild-index should succeed: {stderr}");
    assert!(
        stdout.contains("rebuilt index over") && stdout.contains("chunks"),
        "rebuild human output: {stdout}"
    );
}

#[test]
fn doctor_gates_on_health_not_on_failure() {
    let workspace = tempfile::tempdir().expect("workspace");
    stage_vault(workspace.path());
    write_config(workspace.path());

    // Before init: the report is printed, and it is not a pass.
    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor", "--json"]);
    assert!(!ok, "an uninitialized workspace is not healthy");
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("json: {error}\n{stdout}"));
    assert_eq!(value["status"], "uninitialized");
    let version = value["sqlite"]["version"]
        .as_str()
        .unwrap_or_else(|| panic!("engine facts are reported even before init: {stdout}"));
    assert!(!version.is_empty(), "{stdout}");

    let (_, stderr, ok) = run_cli(workspace.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");

    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor"]);
    assert!(ok, "a ready workspace passes: {stdout}");
    assert!(stdout.contains("workspace is ready"), "{stdout}");
    assert!(
        stdout.contains(&format!("(SQLite {version})")),
        "the human line names the same engine the JSON reported ({version}): {stdout}"
    );
}

#[test]
fn doctor_coverage_reads_the_last_batch_back() {
    let workspace = tempfile::tempdir().expect("workspace");
    stage_vault(workspace.path());
    write_config(workspace.path());

    // Nothing indexed yet: not a failure, a fact worth saying out loud.
    let (_, stderr, ok) = run_cli(workspace.path(), &["doctor", "--coverage"]);
    assert!(ok, "no batch yet is not a failure: {stderr}");
    assert!(stderr.contains("no ingest batch recorded yet"), "{stderr}");
    assert!(
        stderr.contains("`momotaro init` first"),
        "advice a reader can act on: {stderr}"
    );
    // …and `--json` still answers on stdout, where a script looks.
    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor", "--coverage", "--json"]);
    assert!(ok, "{stdout}");
    assert_eq!(
        stdout.trim(),
        "null",
        "a script gets an answer where it looks"
    );

    let (_, stderr, ok) = run_cli(workspace.path(), &["init"]);
    assert!(ok, "init: {stderr}");
    // Initialized but never indexed: the advice is `index`, not `init`.
    let (_, stderr, ok) = run_cli(workspace.path(), &["doctor", "--coverage"]);
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("run `momotaro index` first"),
        "an initialized workspace is one `index` away: {stderr}"
    );

    // A directory that is not a workspace at all gets the reason, not advice
    // that cannot run: `init` fails there too, for want of a configuration. The
    // sentence is `doctor`'s, and it goes to stdout like `doctor`'s does.
    let bare = tempfile::tempdir().expect("bare dir");
    let (stdout, _, ok) = run_cli(bare.path(), &["doctor", "--coverage"]);
    assert!(
        !ok,
        "not a workspace is not the same as 'nothing happened yet': {stdout}"
    );
    assert!(stdout.contains("momotaro.toml"), "{stdout}");
    let (stdout, stderr, ok) = run_cli(workspace.path(), &["index", "--json"]);
    assert!(ok, "index: {stderr}");
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("json: {error}\n{stdout}"));
    let batch_id = report["batch_id"].as_str().expect("index names its batch");

    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor", "--coverage"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains(batch_id), "the batch is named: {stdout}");
    assert!(
        stdout.contains("5 indexed"),
        "the fixture is five files: {stdout}"
    );
    assert!(stdout.contains("0 failed"), "{stdout}");

    // The JSON form is the same report, for scripts.
    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor", "--coverage", "--json"]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("json: {error}\n{stdout}"));
    assert_eq!(value["batch_id"], batch_id);
    assert_eq!(value["indexed"], 5);
    assert_eq!(value["finished"], true);
}

/// The source key is derived from the file name, and POSIX allows control
/// characters in names — an ESC or newline must never reach stderr raw.
/// Linux-only: Win32 refuses such names outright, and macOS normalizes the
/// filesystem itself so the NFC/NFD twins this scenario needs collapse.
#[cfg(target_os = "linux")]
#[test]
fn control_characters_in_names_never_reach_stderr_raw() {
    use std::os::unix::ffi::OsStrExt;

    let workspace = tempfile::tempdir().expect("workspace");
    let vault = workspace.path().join("vault");
    std::fs::create_dir(&vault).expect("create vault");
    // NFC and NFD spellings of one name (each carrying a raw ESC) collide on
    // one key, so the collision path is what gets printed.
    std::fs::write(
        vault.join(std::ffi::OsStr::from_bytes(b"caf\xc3\xa9\x1b.md")),
        "# A\n\nbody\n",
    )
    .expect("write nfc file");
    std::fs::write(
        vault.join(std::ffi::OsStr::from_bytes(b"cafe\xcc\x81\x1b.md")),
        "# B\n\nbody\n",
    )
    .expect("write nfd file");

    write_config(workspace.path());

    let (_, stderr, ok) = run_cli(workspace.path(), &["init"]);
    assert!(ok, "init should succeed: {stderr}");

    let (_, stderr, _) = run_cli(workspace.path(), &["index"]);
    assert!(
        !stderr.contains('\u{1b}'),
        "stderr leaked a raw ESC: {stderr:?}"
    );
    assert!(
        stderr.contains("key_collision"),
        "the collision must still be reported: {stderr:?}"
    );

    // The same rule holds for the human-readable search output, which prints
    // the key and the title of every hit. Assert a hit actually came back
    // first, so the escape check cannot pass vacuously on "no results".
    let (stdout, _, ok) = run_cli(workspace.path(), &["search", "body"]);
    assert!(ok, "search should succeed: {stdout}");
    assert!(
        stdout.contains("note:"),
        "expected a hit line carrying the key: {stdout:?}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "stdout leaked a raw ESC: {stdout:?}"
    );
}

#[test]
fn control_characters_in_json_errors_are_stripped_like_the_human_outlet() {
    // The `--json` error is the same string as the human one (`format_error`
    // is the sanitizer on both sides): a bidi override in a path must not
    // ride the machine-readable channel. U+202E is an ordinary character in a
    // Windows file name, so the fixture is portable.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("vault")).expect("create vault");
    std::fs::write(
        workspace.path().join("momotaro.toml"),
        "[vault]\npath = 'vault\u{202e}evil'\n",
    )
    .expect("write momotaro.toml");

    let (stdout, stderr, ok) = run_cli(workspace.path(), &["index", "--json"]);
    assert!(!ok, "a missing vault is an error");
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is one JSON error document");
    let message = payload["error"]
        .as_str()
        .expect("the error carries a message");
    assert!(
        message.contains("vault"),
        "the error must still say what is wrong: {message:?}"
    );
    assert!(
        !stdout.contains('\u{202e}') && !stderr.contains('\u{202e}'),
        "a bidi override survived an outlet: {stdout:?} {stderr:?}"
    );
}

#[test]
fn doctor_coverage_renders_sync_drive_warnings() {
    // An eviction stub and a Windows-illegal name are facts
    // `doctor --coverage` must show — and they are warnings, not failures, in
    // both exits.
    let workspace = tempfile::tempdir().expect("workspace");
    stage_vault(workspace.path());
    write_config(workspace.path());
    let vault = workspace.path().join("vault");
    std::fs::write(vault.join(".note.md.icloud"), "stub text\n").expect("write the stub");
    std::fs::write(vault.join("con.md"), "# con\n\nbody\n").expect("write con.md");

    let (_, stderr, ok) = run_cli(workspace.path(), &["init"]);
    assert!(ok, "init: {stderr}");
    let (stdout, stderr, ok) = run_cli(workspace.path(), &["index"]);
    assert!(ok, "index: {stderr}");
    assert!(
        stdout.contains("indexed 6 of 7 files"),
        "six of the seven files are content: {stdout}"
    );
    assert!(stdout.contains("1 skipped"), "{stdout}");

    let (stdout, stderr, ok) = run_cli(workspace.path(), &["doctor", "--coverage"]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("0 failed"),
        "a warning is not a failure: {stdout}"
    );
    assert!(stdout.contains("icloud_placeholder"), "{stdout}");
    assert!(stdout.contains("windows_incompatible_name"), "{stdout}");

    let (stdout, _, ok) = run_cli(workspace.path(), &["doctor", "--coverage", "--json"]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("json: {error}\n{stdout}"));
    assert_eq!(value["indexed"], 6);
    assert_eq!(value["skipped"], 1);
    assert_eq!(
        value["warnings"]["icloud_placeholder"]["skipped"], 1,
        "{value}"
    );
    assert_eq!(
        value["warnings"]["windows_incompatible_name"]["in_index"], 1,
        "{value}"
    );
    assert_eq!(
        value["failures"].as_array().map(Vec::len),
        Some(0),
        "{value}"
    );
}

#[test]
fn a_top_above_the_cap_is_refused_by_the_parser() {
    // The pre-allocation tantivy does is driven by this number alone (a
    // measured 20M -> 460 MB peak commit), so the parser refuses absurd
    // values instead of letting a typo become a memory spike; 0 stays legal.
    let workspace = tempfile::tempdir().expect("workspace");
    stage_vault(workspace.path());
    write_config(workspace.path());
    let (_, stderr, ok) = run_cli(workspace.path(), &["init"]);
    assert!(ok, "init: {stderr}");
    let (_, stderr, ok) = run_cli(workspace.path(), &["index"]);
    assert!(ok, "index: {stderr}");

    let (_, stderr, ok) = run_cli(workspace.path(), &["search", "傅里叶", "--top", "1001"]);
    assert!(!ok, "1001 is above the cap: {stderr}");
    assert!(
        stderr.contains("1001"),
        "the refusal names the value: {stderr}"
    );

    let (_, stderr, ok) = run_cli(workspace.path(), &["search", "傅里叶", "--top", "0"]);
    assert!(
        ok,
        "0 stays legal — an empty result, not an error: {stderr}"
    );

    let (stdout, _, ok) = run_cli(
        workspace.path(),
        &["search", "傅里叶", "--top", "1001", "--json"],
    );
    assert!(!ok, "still refused under --json");
    let payload: serde_json::Value =
        serde_json::from_str(&stdout).expect("the --json outlet carries the refusal");
    assert!(
        payload["error"]
            .as_str()
            .is_some_and(|sentence| sentence.contains("1001")),
        "{payload}"
    );
}
