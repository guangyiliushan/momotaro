---
sidebar_position: 24
sidebar_label: ADR 0024
---

# ADR 0024 · `local_path` is a workspace-relative fact, checked on every resolve

Revises D42.

All `local_path` values are relative to the workspace root (the directory that holds `momotaro.toml` and the database), never mixed with vault-relative tails. Resolve is `workspace_root.join(local_path)` after normalization; the result must stay inside the workspace, including no symlink escape (D42's `..` re-check, applied to this column). Imports are a trust boundary. The column records which file was meant at ingest time; a missing file is `source_missing`, not a fallback to a conventional location.
