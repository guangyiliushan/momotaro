use std::path::{Path, PathBuf};
use std::process::Command;

pub fn stage_vault(workspace: &Path) -> PathBuf {
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

pub fn write_config(workspace: &Path) {
    std::fs::write(
        workspace.join("momotaro.toml"),
        "[vault]\npath = 'vault'\n\n[model]\nprovider = 'fake'\nmodel_id = 'fake-model'\n",
    )
    .expect("write momotaro.toml");
}

pub fn run_cli(cwd: &Path, args: &[&str]) -> (String, String, bool) {
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
