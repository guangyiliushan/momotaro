---
sidebar_position: 27
sidebar_label: ADR 0027
---

# ADR 0027 · graph_links rows are a rebuilt projection of as-written links

Closes what architecture §8.2 left to W2.4: row identity and replacement for
`graph_links`. Refines D41's build matrix and D40's per-file unit; consistent
with ADR 0014 (a rename is delete+create).

**Row identity is the as-written tuple.** A row is one
`(from_source_key, raw_target, edge_kind, link_source, anchor_kind, anchor_raw)`
six-tuple. `raw_target` and `anchor_raw` are what the author typed: distinct
writings of the same target (`[[X]]`, `[[docs/X]]`) are distinct rows, and a
dangling row — `to_source_key` NULL — keeps its identity through every
resolution change. `to_source_key` is a resolution product of the raw target
against the current namespace, never part of identity. A file that links the
same target twice in the same way has one row: the set is idempotent, in the
same sense D42 exempts content-addressed sets from its no-merge rule.

**Replacement is per source, in the file's transaction.** Re-processing a file
replaces every row with that `from_source_key` in one DELETE+INSERT, committed
in the same transaction that writes the file's chunks (D40's unit). Link
sources are not replaced separately: one parse produces one row set. The
projection-only replacements (a file nothing else changed) commit together in
one batch transaction at the run's data sweep — still wholesale per source,
just one commit for the batch — and that sweep also prunes the rows of sources
the walk no longer named: a rebuild would never produce them, so leaving them
would break the reproduction above. An anchor that cannot be verified (no
`outline` in the target's blob) is not a stale anchor and reports no code.

**`created_at` is the revision's, not the row's.** A row's `created_at` is the
`ingested_at` of the source revision whose content carries the link. A derived
table carries no build history: deleting `graph_links` and re-running ingest
reproduces every row byte for byte, `created_at` included, and the acceptance
test asserts exactly that. The row records "this is where the edge came from",
not "when the writer ran".

Why not occurrence rows: nothing reads occurrence counts, and accumulating
duplicates would make both the rebuild comparison and the per-file replace lie.
Why not a UNIQUE constraint: SQLite treats NULLs as distinct in UNIQUE indexes
and the anchor columns are NULL-able by contract — the identity is enforced in
the writer (a set per file) and witnessed by the rebuild test.

Consequences: resolution follows the vault, so an edge's `to_source_key` can go
dangling again when its target is renamed or deleted (ADR 0014) — the next
`index` re-projects every file and says so honestly; resolution-rule changes
are replayable offline from the stored raw forms (`metadata_json` `links` /
`link_losses` / `outline` keys) without re-reading the vault.
