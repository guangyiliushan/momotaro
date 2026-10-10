---
sidebar_position: 22
sidebar_label: ADR 0022
---

# ADR 0022 · Chunks are a retained projection

Revises D1 / D2.

Source revisions are the canonical bytes. `chunk_id` / `chunk_hash` must be a pure function of `(source_key, revision_hash, ordinal, text)`. Chunk rows are neither undeletable audit facts nor disposable Tantivy-class derived data: after a `(source_key, revision_hash)` is chunked, a row stays while any `answer_citations` or `run_context_items` row whose five-tuple carries that `(source_key, revision_hash)` still names its `chunk_id`, and stays as it is — an incoming row under the same id never rewrites its `chunk_hash`, so a five-tuple cannot silently go stale under a citation. Retrieval hits do not pin rows: `retrieval_hits` is the audit of what a query once returned, and pinning every searched chunk would make deleting unreferenced rows impossible. Same policy, same revision → idempotent replace. New policy → new ingest only. Deleting unreferenced rows is allowed. Wiping the table is disaster recovery with the current policy and must emit drift, not silent re-verification: the replace that finds a named id without its row reports drift. Treating rows as undeletable would freeze chunker bugs in the audit log; wiping on every policy change would make old citations unverifiable.
