//! End-to-end CLI tests against the checked-in momotaro-run fixture vault.

use std::path::PathBuf;
use std::process::Command;

fn fixture_vault() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let vault = manifest_dir
        .join("../../crates/momotaro-run/tests/fixtures/vault")
        .canonicalize()
        .expect("fixture vault exists");
    assert!(vault.is_dir(), "fixture vault is a directory");
    vault
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
    let vault = fixture_vault();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tempdir.path().join("momotaro.toml"),
        format!("[vault]\npath = '{}'\n", vault.display()),
    )
    .expect("write momotaro.toml");

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
    let vault = fixture_vault();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tempdir.path().join("momotaro.toml"),
        format!("[vault]\npath = '{}'\n", vault.display()),
    )
    .expect("write momotaro.toml");

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
