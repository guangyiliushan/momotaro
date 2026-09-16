//! Vault discovery and source-key derivation.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::IngestError;

fn is_hidden(file_name: &std::ffi::OsStr) -> bool {
    file_name
        .to_str()
        .is_some_and(|s| s.starts_with('.') && s != ".")
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Walk `vault_root` and return all Markdown file paths, sorted.
///
/// Hidden directories (any component starting with `.`) are skipped, so
/// `.git`, `.obsidian`, and `.momotaro` never contribute files. The root
/// itself is exempt from the hidden check so dot-prefixed temp roots work.
pub fn walk_markdown_files(vault_root: &Path) -> Result<Vec<PathBuf>, IngestError> {
    let metadata = fs::metadata(vault_root).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            IngestError::VaultMissing(vault_root.display().to_string())
        } else {
            IngestError::Io(e)
        }
    })?;
    if !metadata.is_dir() {
        return Err(IngestError::VaultMissing(vault_root.display().to_string()));
    }

    let mut files: Vec<PathBuf> = WalkDir::new(vault_root)
        .into_iter()
        // Prune dot-prefixed entries (and their subtrees) by their own file
        // name; the root itself is exempt so dot-prefixed temp roots work.
        .filter_entry(|entry| entry.depth() == 0 || !is_hidden(entry.file_name()))
        .filter_map(|entry| entry.ok())
        // Symlink entries are skipped: `file_type()` does not follow links,
        // so a vault-supplied `link.md` pointing outside the root can never
        // pull external content into the index.
        .filter(|entry| entry.file_type().is_file() && is_markdown(entry.path()))
        .map(|entry| entry.into_path())
        .collect();
    files.sort();
    Ok(files)
}

/// Derive the vault-relative source key using `/` separators.
///
/// Uses path components only; never string-replaces backslashes.
pub fn source_key_for(vault_root: &Path, path: &Path) -> Result<String, IngestError> {
    let relative = path.strip_prefix(vault_root).map_err(|_| {
        IngestError::OutsideVault(format!(
            "{} is outside vault root {}",
            path.display(),
            vault_root.display()
        ))
    })?;
    let key = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if key.is_empty() {
        return Err(IngestError::OutsideVault(path.display().to_string()));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, contents: &str) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, contents).unwrap();
    }

    #[test]
    fn walk_finds_md_skips_dot_dirs_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "b.md", "b");
        write(root, "a/inner.MD", "upper");
        write(root, "a/c.md", "c");
        write(root, ".obsidian/config.md", "hidden");
        write(root, ".git/hooks.md", "hidden");
        write(root, ".momotaro/db.md", "hidden");
        write(root, "notes.txt", "not md");

        let found = walk_markdown_files(root).unwrap();
        let keys: Vec<String> = found
            .iter()
            .map(|p| source_key_for(root, p).unwrap())
            .collect();
        assert_eq!(keys, vec!["a/c.md", "a/inner.MD", "b.md"]);
    }

    #[test]
    fn walk_missing_root_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(matches!(
            walk_markdown_files(&missing),
            Err(IngestError::VaultMissing(_))
        ));
    }

    #[test]
    fn source_key_uses_forward_slashes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("docs").join("deep").join("note.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "x").unwrap();
        let key = source_key_for(tmp.path(), &file).unwrap();
        assert_eq!(key, "docs/deep/note.md");
        assert!(!key.contains('\\'));
    }

    #[test]
    fn source_key_rejects_outside_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let file = b.path().join("x.md");
        fs::write(&file, "x").unwrap();
        assert!(matches!(
            source_key_for(a.path(), &file),
            Err(IngestError::OutsideVault(_))
        ));
    }

    #[test]
    fn walk_skips_symlink_entries() {
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("secret.md");
            fs::write(&secret, "outside content").unwrap();

            let vault = tempfile::tempdir().unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&secret, vault.path().join("link.md")).unwrap();
            fs::write(vault.path().join("real.md"), "real").unwrap();

            let found = walk_markdown_files(vault.path()).unwrap();
            let names: Vec<String> = found
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
                .collect();
            assert_eq!(names, vec!["real.md"]);
        }
        // On Windows, symlink creation requires privileges; the filter itself
        // is platform-independent (file_type().is_symlink() -> skip), so the
        // unix-only assertion covers the behavior where it is creatable.
        #[cfg(not(unix))]
        {
            let vault = tempfile::tempdir().unwrap();
            fs::write(vault.path().join("real.md"), "real").unwrap();
            let found = walk_markdown_files(vault.path()).unwrap();
            assert_eq!(found.len(), 1);
        }
    }
}
