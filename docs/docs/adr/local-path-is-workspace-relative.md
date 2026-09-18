---
sidebar_position: 24
sidebar_label: ADR 0024
---

# ADR 0024 · `local_path` is a workspace-relative fact, checked on every resolve

Revises D42.

All `local_path` values are relative to the workspace root (the directory that holds `momotaro.toml` and the database), never mixed with vault-relative tails. Resolve is `workspace_root.join(local_path)` after normalization; the result must stay inside the workspace, including no symlink escape (D42's `..` re-check, applied to this column). Imports are a trust boundary. The column records which file was meant at ingest time; a missing file is `source_missing`, not a fallback to a conventional location.

## Implementation note (2026-09-17)

Containment is decided **only between canonicalized paths**: the lexical
`Path::starts_with` check does not resolve `..` (measured:
`C:\ws\..\..\secret.md` starts with `C:\ws`), so `momotaro_ingest::path`
canonicalizes both sides first. `canonicalize` fails with ENOENT for a path that
does not exist yet, so a candidate is canonicalized through its **nearest
existing ancestor** and the missing tail is re-appended. `vault == workspace
root` is legal ([ADR 0025](vault-must-live-inside-the-workspace.md) presumes the
data directory may sit inside the vault), and the workspace root is inside
itself.

`resolve_local_path` rejects absolute forms (POSIX, drive-letter, UNC) and any
`..` component outright — never "strip and re-check" — and re-runs the
containment test after canonicalization, which is what catches symlinks,
junctions and mount points (CWE-1386). There is **no inode-level identity
check**: Rust has no stable `file_index` or volume serial, so the path is the
identity, and 8.3 short names plus drive-letter case are normalized by
`canonicalize` rather than by this crate.

The path `resolve_local_path` returns is for **filesystem use**: on Windows it
may carry the `\\?\` verbatim prefix and its drive-letter case is whatever the
volume reports. User-visible output renders the stored `local_path` instead
(see [architecture.md](../architecture.md) and
[ingest-interop.md](../ingest-interop.md) for that rule).
