//! Vault discovery and source-key derivation.

use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// Derives the vault-relative `raw_name` exactly as the file spells it
/// (original case, `/` separators).
///
/// Kept for rename matching, collision reporting and display. It is never an
/// identity: the key is.
pub fn raw_name_for(vault_root: &Path, path: &Path) -> Result<String, IngestError> {
    relative_name(vault_root, path)
}

/// Derives the scheme-prefixed source key for one vault file.
///
/// Delegates to [`momotaro_contracts::note_source_key`]: NFC on the tail only,
/// case preserved verbatim, `/` the only separator, and absolute or
/// `..`-bearing tails rejected.
pub fn source_key_for(vault_root: &Path, path: &Path) -> Result<String, IngestError> {
    let raw = relative_name(vault_root, path)?;
    momotaro_contracts::note_source_key(&raw)
        .map_err(|error| IngestError::InvalidSourceKey(format!("{path:?}: {error}")))
}

/// Vault-relative name built from path components, `/`-joined.
///
/// Uses components rather than string surgery: on Windows a component never
/// contains a backslash, so no byte of a name is ever rewritten here. A root,
/// prefix or parent-dir component means the caller did not hand us a
/// vault-relative name, and is refused rather than rendered as a name byte
/// (`strip_prefix("")` matches vacuously, so `/etc/x` would otherwise become
/// part of the key).
fn relative_name(vault_root: &Path, path: &Path) -> Result<String, IngestError> {
    let relative = path.strip_prefix(vault_root).map_err(|_| {
        IngestError::OutsideVault(format!("{path:?} is outside vault root {vault_root:?}"))
    })?;
    let mut parts: Vec<std::borrow::Cow<'_, str>> = Vec::new();
    for component in relative.components() {
        match component {
            // A name that is not valid UTF-8 would be folded onto U+FFFD by
            // `to_string_lossy`, handing two distinct files one identity.
            Component::Normal(name) => match name.to_str() {
                Some(text) => parts.push(std::borrow::Cow::Borrowed(text)),
                None => {
                    return Err(IngestError::InvalidUtf8(format!("{path:?}")));
                }
            },
            Component::CurDir => continue,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(IngestError::OutsideVault(format!("{path:?}")));
            }
        }
    }
    if parts.is_empty() {
        return Err(IngestError::OutsideVault(format!("{path:?}")));
    }
    Ok(parts.join("/"))
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
        assert_eq!(keys, vec!["note:a/c.md", "note:a/inner.MD", "note:b.md"]);
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
    fn source_key_is_scheme_prefixed_and_keeps_forward_slashes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("docs").join("deep").join("note.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "x").unwrap();
        let key = source_key_for(tmp.path(), &file).unwrap();
        assert_eq!(key, "note:docs/deep/note.md");
        assert!(!key.contains('\\'));
    }

    #[test]
    fn raw_name_keeps_the_original_spelling_and_is_not_a_key() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("Notes").join("A.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "x").unwrap();
        assert_eq!(raw_name_for(tmp.path(), &file).unwrap(), "Notes/A.md");
        assert_eq!(
            source_key_for(tmp.path(), &file).unwrap(),
            "note:Notes/A.md"
        );
    }

    /// The vault layer hands the key builder a name in whatever spelling the
    /// filesystem reports; the key must be the NFC form (D42). Walks a real
    /// directory so the walk -> key pipeline is exercised end to end.
    #[test]
    fn source_key_normalizes_a_decomposed_file_name_to_nfc() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("cafe\u{0301}.md"), "x").unwrap();

        let found = walk_markdown_files(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            source_key_for(tmp.path(), &found[0]).unwrap(),
            "note:caf\u{00e9}.md"
        );
        assert_eq!(
            raw_name_for(tmp.path(), &found[0]).unwrap(),
            "cafe\u{0301}.md"
        );
    }

    /// The walker never produces these, but a malformed caller must not be
    /// able to smuggle a root or a parent-dir component into a key: with a
    /// degenerate root, `strip_prefix` matches vacuously and the root
    /// pseudo-component would be rendered as a name byte.
    #[test]
    fn source_key_rejects_non_normal_path_components() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            source_key_for(
                std::path::Path::new(""),
                std::path::Path::new("/etc/passwd")
            ),
            Err(IngestError::OutsideVault(_))
        ));
        assert!(matches!(
            source_key_for(tmp.path(), &tmp.path().join("..").join("x.md")),
            Err(IngestError::OutsideVault(_))
        ));
    }

    /// A name that is not valid UTF-8 must be refused: `to_string_lossy` would
    /// map distinct byte sequences onto U+FFFD and hand them one identity.
    /// Built from an OsStr directly — no filesystem support for the name is
    /// required, so the guard is testable on both platforms.
    #[cfg(unix)]
    #[test]
    fn source_key_rejects_a_name_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(std::ffi::OsStr::from_bytes(b"bad\xff.md"));
        assert!(matches!(
            source_key_for(tmp.path(), &path),
            Err(IngestError::InvalidUtf8(_))
        ));
    }

    /// Windows spelling of the same guard: an unpaired surrogate is not valid
    /// text and must not be folded into U+FFFD.
    #[cfg(windows)]
    #[test]
    fn source_key_rejects_a_name_that_is_not_utf8() {
        use std::os::windows::ffi::OsStringExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut wide: Vec<u16> = "bad".encode_utf16().collect();
        wide.push(0xD800);
        wide.extend(".md".encode_utf16());
        let path = tmp.path().join(std::ffi::OsString::from_wide(&wide));
        assert!(matches!(
            source_key_for(tmp.path(), &path),
            Err(IngestError::InvalidUtf8(_))
        ));
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
