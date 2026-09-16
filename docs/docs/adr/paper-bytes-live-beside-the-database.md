---
sidebar_position: 19
sidebar_label: ADR 0019
---

# ADR 0019 · Paper bytes live in the workspace data directory, not the Vault

Revises D2 / D31.

`paper add` writes PDFs under `{workspace}/.momotaro/papers/`. They are canonical revision bytes (D2) stored beside the database, not Vault files. A PDF inside the Vault is an unindexed attachment: no `note:` key, not an `arxiv:` Source. Dot-directories are already watcher-blacklisted, so a data dir nested in the Vault is not ingested. Missing bytes at `local_path` are `source_missing` / `source_integrity`, never an empty paper.
