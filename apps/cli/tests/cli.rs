//! End-to-end CLI tests against a copy of the checked-in fixture vault.
//!
//! The vault is staged *inside* the temporary workspace: since ADR 0024 every
//! stored path is workspace-relative, so a vault outside the workspace root is
//! refused by design.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Copies the fixture vault into `<workspace>/vault` and returns its path.
fn stage_vault(workspace: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/momotaro-run/tests/fixtures/vault")
        .canonicalize()
        .expect("fixture vault exists");
    assert!(source.is_dir(), "fixture vault is a directory");

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

/// Writes the workspace configuration pointing at the staged vault.
fn write_config(workspace: &Path) {
    std::fs::write(workspace.join("momotaro.toml"), "[vault]\npath = 'vault'\n")
        .expect("write momotaro.toml");
}

fn run_cli(cwd: &std::path::Path, args: &[&str]) -> (String, String, bool) {
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
