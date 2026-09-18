---
sidebar_position: 25
sidebar_label: ADR 0025
---

# ADR 0025 · The vault must live inside the workspace

Revises D42. This is decision Q1 of the v0.2 contract queue.

`momotaro.toml` and `.momotaro/` define the **workspace**; `[vault].path` names a
directory that must resolve **inside** it (`vault == workspace root` is legal).
Containment is decided between canonicalized paths
([ADR 0024](local-path-is-workspace-relative.md)), so a symlink, junction or
mount point cannot smuggle a vault out of the workspace.

Why: `local_path` is stored relative to the workspace root, and the paper-bytes
policy ([ADR 0020](paper-bytes-live-beside-the-database.md)) keeps downloaded
bytes beside the database. A vault outside the workspace would push `local_path`
back to absolute paths — exactly what [ADR 0024](local-path-is-workspace-relative.md)
exists to prevent — or make the records depend on two independent roots.

**The cost is disclosed here because it is a real product cost.** Comparable
products (Obsidian, Logseq, Zotero, DEVONthink, Joplin, Foam, SilverBullet,
Anytype) all let the user point a content root anywhere. The closest precedent
for two roots is Zotero's linked-attachment base directory, which Zotero itself
labels advanced and explicitly does not support. The consequence is an
onboarding obligation: **the sync scope must be narrowed to the vault
sub-directory**, because SQLite's WAL does not work on network filesystems, and
syncing the database is the fastest way to corrupt it.

Implementation: `momotaro_ingest::path` (`is_within`, `local_path_in`,
`resolve_local_path`) and `run::vault_scope`, which turns a violation into
`RunError::VaultOutsideWorkspace`; `doctor` reports it as `invalid` instead of
failing hard, preserving its non-destructive contract.
