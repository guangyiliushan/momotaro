//! Workspace-relative path facts (ADR 0024).
//!
//! Containment is only ever decided between two *canonicalized* paths: the
//! lexical `Path::starts_with` check does not resolve `..` and happily accepts
//! `C:\ws\..\..\secret.md`. Because `canonicalize` fails with ENOENT for paths
//! that do not exist yet, a candidate is canonicalized through its nearest
//! existing ancestor.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::IngestError;

/// Renders `path` relative to `workspace_root` for the `local_path` column.
///
/// Canonicalizes a workspace root for path facts.
///
/// Resolving many files under one root should canonicalize once and then use
/// [`local_path_in`]: canonicalizing per file is one extra syscall per file.
pub fn canonical_workspace_root(workspace_root: &Path) -> Result<PathBuf, IngestError> {
    canonical_root(workspace_root)
}

/// Renders `path` relative to an already canonicalized workspace root.
///
/// The value is `/`-joined (the column is a portable string, not a platform
/// path) and never leaves the workspace; the root itself renders as the empty
/// string. Callers pass files.
///
/// `canonical_root` must come from [`canonical_workspace_root`]: containment is
/// decided by comparing canonical forms, so a root that is not canonical makes
/// the comparison meaningless — it fails closed with `OutsideWorkspace`, never
/// silently.
pub fn local_path_in(canonical_root: &Path, path: &Path) -> Result<String, IngestError> {
    let candidate = canonicalize_existing_ancestor(path)?;
    let relative = candidate
        .strip_prefix(canonical_root)
        .map_err(|_| outside(&format!("{path:?} is outside workspace {canonical_root:?}")))?;
    slash_joined(relative)
}

/// Resolves a stored `local_path` against the workspace root.
///
/// Absolute values and `..` components are rejected outright, and containment
/// is re-checked after canonicalization, so a symlink or junction cannot walk
/// the value out of the workspace either. The returned path is rooted at the
/// canonicalized root, so on Windows it carries the `\\?\` verbatim prefix —
/// strip it before showing the path to a human.
pub fn resolve_local_path(workspace_root: &Path, local_path: &str) -> Result<PathBuf, IngestError> {
    let relative = Path::new(local_path);
    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(outside(local_path));
            }
        }
    }

    let root = canonical_root(workspace_root)?;
    let joined = root.join(relative);
    let candidate = canonicalize_existing_ancestor(&joined)?;
    if !candidate.starts_with(&root) {
        return Err(outside(local_path));
    }
    Ok(joined)
}

/// Whether `candidate` lies inside `workspace_root` (both canonicalized).
pub fn is_within(workspace_root: &Path, candidate: &Path) -> Result<bool, IngestError> {
    let root = canonical_root(workspace_root)?;
    let resolved = canonicalize_existing_ancestor(candidate)?;
    Ok(resolved.starts_with(&root))
}

fn outside(value: &str) -> IngestError {
    IngestError::OutsideWorkspace(value.to_owned())
}

/// Canonicalizes the workspace root: containment means nothing unless the root
/// itself resolves.
fn canonical_root(workspace_root: &Path) -> Result<PathBuf, IngestError> {
    fs::canonicalize(workspace_root).map_err(|error| {
        IngestError::Io(io_error(
            error,
            format!("workspace root {}", workspace_root.display()),
        ))
    })
}

/// Canonicalizes `path` through its nearest existing ancestor.
///
/// `fs::canonicalize` fails with ENOENT for a path that does not exist yet —
/// and a `local_path` may name a file that is not there — so the deepest
/// existing ancestor is canonicalized and the missing tail re-appended
/// unchanged.
fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf, IngestError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(IngestError::Io)?.join(path)
    };

    let mut missing: Vec<OsString> = Vec::new();
    let mut current: &Path = &absolute;
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(IngestError::Io(error));
                };
                missing.push(name.to_os_string());
                match current.parent() {
                    Some(parent) => current = parent,
                    None => return Err(IngestError::Io(error)),
                }
            }
            Err(error) => return Err(IngestError::Io(error)),
        }
    }
}

/// Joins path components with `/` for the portable column value.
///
/// A name that is not valid UTF-8 is refused rather than lossily folded: the
/// column is text, `to_string_lossy` would render a non-UTF-8 byte as U+FFFD,
/// and two different names could then collapse onto one stored path.
fn slash_joined(relative: &Path) -> Result<String, IngestError> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Some(part) = component.as_os_str().to_str() else {
            return Err(IngestError::InvalidUtf8(
                component.as_os_str().to_string_lossy().into_owned(),
            ));
        };
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// Wraps an IO failure with the value it happened on.
fn io_error(error: std::io::Error, context: String) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ws() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp workspace")
    }

    /// The canonical root — what a caller obtains once per run.
    fn canonical(dir: &tempfile::TempDir) -> PathBuf {
        canonical_workspace_root(dir.path()).expect("canonical root")
    }

    #[test]
    fn local_path_is_workspace_relative() {
        let root = ws();
        let file = root.path().join("notes").join("a.md");
        fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
        fs::write(&file, "x").expect("write file");
        assert_eq!(
            local_path_in(&canonical(&root), &file).expect("relative"),
            "notes/a.md"
        );
    }

    #[test]
    fn local_path_for_the_workspace_root_itself_is_empty() {
        let root = ws();
        assert_eq!(
            local_path_in(&canonical(&root), root.path()).expect("relative"),
            ""
        );
    }

    #[test]
    fn file_outside_the_workspace_is_rejected() {
        let root = ws();
        let other = ws();
        let file = other.path().join("a.md");
        fs::write(&file, "x").expect("write");
        assert!(matches!(
            local_path_in(&canonical(&root), &file),
            Err(IngestError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn resolve_rejects_absolute_and_parent_dir_values() {
        let root = ws();
        let mut bad = vec![
            "/etc/passwd",
            "..",
            "../a.md",
            "a/../../b.md",
            "./../../b.md",
        ];
        #[cfg(windows)]
        bad.extend([
            r"C:\ws\a.md",
            r"\\srv\share\a.md",
            r"\a.md",
            r"a\..\..\b.md",
            "C:a.md",
        ]);
        for value in bad {
            assert!(
                matches!(
                    resolve_local_path(root.path(), value),
                    Err(IngestError::OutsideWorkspace(_))
                ),
                "{value} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_accepts_well_formed_relative_values() {
        let root = ws();
        let canonical_root = root.path().canonicalize().expect("canonical root");
        for value in [
            "a.md",
            "notes/a.md",
            "notes/deep/a.md",
            "./a.md",
            "papers/2401.12345/v1.pdf",
        ] {
            let resolved = resolve_local_path(root.path(), value).expect(value);
            assert!(
                resolved.starts_with(&canonical_root),
                "{value} -> {}",
                resolved.display()
            );
        }
    }

    #[test]
    fn resolve_accepts_the_workspace_root_as_the_empty_value() {
        // Round trip with `local_path_for`: the root renders empty, and the
        // empty value resolves back to it. Nothing else may.
        let root = ws();
        let resolved = resolve_local_path(root.path(), "").expect("the root");
        assert_eq!(
            resolved,
            root.path().canonicalize().expect("canonical root")
        );
    }

    #[test]
    fn the_workspace_root_itself_is_within_the_workspace() {
        let root = ws();
        assert!(is_within(root.path(), root.path()).expect("containment"));
    }

    #[test]
    fn nothing_outside_the_workspace_is_within_it() {
        let root = ws();
        let other = ws();
        assert!(!is_within(root.path(), other.path()).expect("containment"));
    }

    #[cfg(windows)]
    #[test]
    fn drive_case_difference_does_not_break_containment() {
        // A lexical comparison says `C:\WS` != `C:\ws`; canonicalizing both
        // sides lets the filesystem's real case decide.
        let root = ws();
        fs::write(root.path().join("a.md"), "x").expect("write");
        let text = root.path().to_string_lossy().to_string();
        let upper_drive = format!("{}{}", text[..1].to_uppercase(), &text[1..]);
        assert!(is_within(root.path(), Path::new(&upper_drive)).expect("containment"));
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_refused_not_folded() {
        use std::os::unix::ffi::OsStrExt;

        let root = ws();
        let file = root.path().join(std::ffi::OsStr::from_bytes(b"caf\xff.md"));
        fs::write(&file, "x").expect("write");
        assert!(
            matches!(
                local_path_in(&canonical(&root), &file),
                Err(IngestError::InvalidUtf8(_))
            ),
            "the column is text: a lossy fold could collide two different names"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let root = ws();
        let outside = ws();
        fs::write(outside.path().join("secret.md"), "s").expect("write");
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).expect("symlink");
        assert!(matches!(
            resolve_local_path(root.path(), "link/secret.md"),
            Err(IngestError::OutsideWorkspace(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn junction_escape_is_rejected() {
        let root = ws();
        let outside = ws();
        let link = root.path().join("link");
        // Junctions need no elevation (symlinks usually do), so this is the
        // Windows escape shape that exists in the wild.
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            created,
            "this test needs to create a junction; a silent skip would make \
             \"did not run\" look exactly like \"passed\" — if an environment \
             genuinely cannot, mark it ignored explicitly instead"
        );
        fs::write(outside.path().join("secret.md"), "s").expect("write");
        assert!(
            matches!(
                resolve_local_path(root.path(), "link/secret.md"),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "a junction must not walk a stored path out of the workspace"
        );
        assert!(
            matches!(
                local_path_in(&canonical(&root), &link.join("secret.md")),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "and the writer must not produce such a value either"
        );
    }
}
