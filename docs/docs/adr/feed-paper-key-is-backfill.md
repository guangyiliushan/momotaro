---
sidebar_position: 20
sidebar_label: ADR 0020
---

# ADR 0020 · Feed `paper_key` is a derived backfill from a successful paper add

`paper add` still takes an arXiv id. On success, matching `feed_items` with a NULL `paper_key` are filled from the bare id (version stripped, per [ADR 0015](arxiv-version-is-a-revision.md)). Unmatched rows stay candidates. There is no add-by-feed-item-id path. Poll may retry that NULL lookup; it must not guess by title.
