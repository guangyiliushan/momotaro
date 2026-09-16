---
sidebar_position: 22
sidebar_label: ADR 0022
---

# ADR 0022 · Chunks are a retained projection

Supersedes ADR 0001. Revises D1 / D2.

Source revisions are the canonical bytes. `chunk_id` / `chunk_hash` must be a pure function of `(source_key, revision_hash, ordinal, text)`. Chunk rows are neither undeletable audit facts nor disposable Tantivy-class derived data: after a `(source_key, revision_hash)` is chunked, those rows stay while any `answer_citations` / `run_context_items` / retrieval hit still names them. Same policy, same revision → idempotent replace. New policy → new ingest only. Deleting unreferenced rows is allowed. Wiping the table is disaster recovery with the current policy and must emit drift, not silent re-verification. Treating rows as undeletable would freeze chunker bugs in the audit log; wiping on every policy change would make old citations unverifiable.
