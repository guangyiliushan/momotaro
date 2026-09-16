---
sidebar_position: 15
sidebar_label: ADR 0015
---

# ADR 0015 · arXiv vN is a new revision of `arxiv:<id>`, not a new Source

`source_key` is `arxiv:<id>` with no version suffix. Ingest is idempotent on `(arxiv_id, version)`. A later journal DOI is related metadata (IsVersionOf), not a second key — 0.x still has no `doi:` scheme. Treating v2 as another Source would split annotations; overwriting the old revision would break immutability.
