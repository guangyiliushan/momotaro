---
sidebar_position: 14
sidebar_label: ADR 0014
---

# ADR 0014 · Note rename is a new Source; keys in history are never rewritten

Revises D40 / D42.

`note:` identity is the vault-relative path. A rename is delete+create (two rescans). Watcher rename hints may emit `source_moved(old_key, new_key)` and must not UPDATE any stored `source_key`. Old citations and annotations stay on the old key and fail as `source_missing` / `source_integrity`, not as a silent miss. A directory rename is the same fact at scale: UI aggregates those failures by directory.
