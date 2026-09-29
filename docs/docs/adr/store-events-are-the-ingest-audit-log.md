---
sidebar_position: 26
sidebar_label: ADR 0026
---

# ADR 0026 · Store events are the ingest audit log

`store_events` is the append-only log of what the **store** did on its own
authority: ingest batches, feed polls, renames, annotations, typeahead. It is
schema v4, and it exists so that "what happened to this file" has one answer that
is not a fresh scan of the vault.

## Why it is not `run_events`

`run_events` records a **Run** — the process of an accepted piece of work
([harness](../harness.md)), bounded by a `RunSpec`, whose status must reach a
terminal value. Indexing, polling a feed, renaming, annotating and serving
typeahead are not Runs: nobody accepted them, they carry no budget, and their
failure is a per-file outcome rather than a failed unit of work. Folding them into
`run_events` would hand every `index` command a run id and a status machine it has
no use for. The two tables stay apart, and ingest gets the small log it needs.

## The table

```sql
CREATE TABLE store_events (
  id INTEGER PRIMARY KEY,            -- rowid; no AUTOINCREMENT (SQLite's own advice: extra CPU/memory/disk/IO for nothing)
  occurred_at INTEGER NOT NULL,      -- when it happened (a replay or an import is not when it was written)
  recorded_at INTEGER NOT NULL,      -- transaction time: store-assigned once per
                                     -- `append_store_events` call — one index run
                                     -- issues many of them, so rows sharing a
                                     -- `batch_id` may carry different transaction
                                     -- times — and never below the previous row's
                                     -- (max(now, last)): wall-clock NTP steps must
                                     -- not move it backwards. No CHECK exists for
                                     -- this: SQLite cannot express a cross-row
                                     -- constraint, so the store enforces it. The
                                     -- read sits outside the write transaction,
                                     -- which is sound under the single-writer
                                     -- premise this ADR states.
  batch_id TEXT NOT NULL,            -- one index, watch, reconcile, feed poll or rename
  seq INTEGER NOT NULL,              -- monotonic inside the batch
  domain TEXT NOT NULL,              -- ingest | feed | vault | annotate
  type TEXT NOT NULL,                -- ingest.* / vault.reconcile.* / source_moved / feed.poll.*
  source_key TEXT,                   -- set when the event is about one source
  outcome TEXT,                      -- success | no_results | upstream_error | parse_error
  reason_code TEXT,                  -- frontmatter_parse_error / empty_feed / …
  message TEXT,
  data_json TEXT NOT NULL,
  external_id TEXT,                  -- provider-issued, for deduplication only
  UNIQUE (batch_id, seq)
);
CREATE UNIQUE INDEX idx_store_events_external
  ON store_events(domain, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX idx_store_events_source ON store_events(source_key) WHERE source_key IS NOT NULL;
```

`UNIQUE (batch_id, seq)` already serves `WHERE batch_id = ?` through its leftmost
prefix, so no second index on those columns is added.

### One column, two vocabularies

`outcome` and `reason_code` stay text so a new adapter code needs no migration,
and each **domain** owns its set:

| domain | `outcome` | `reason_code` |
|---|---|---|
| `ingest` | `indexed` \| `skipped` \| `metadata_refreshed` \| `error` | the `IndexFileErrorKind` spelling (`key_collision`, `invalid_utf8`, `outside_vault`, `invalid_source_key`, `io`, `store`, `index`, `other`) — or, on a non-`error` row (`indexed` / `metadata_refreshed` / `skipped`), a degradation code (D41's ten; all ten have had producers since W2.4's edges landed) — or a warning code (`onedrive_placeholder` / `icloud_placeholder` / `windows_incompatible_name` / `source_removed`; `momotaro_contracts::Warning`, the section below says why) |
| `feed` | `success` \| `no_results` \| `upstream_error` \| `parse_error` | provider codes (`response_too_large`, `empty_feed`, …) |
| `vault` | `indexed` \| `skipped` \| `metadata_refreshed` \| `error` — the same four as `ingest`, because a reconcile is the same loop under a different event family | the same two vocabularies as `ingest` |
| `annotate` | not implemented | not implemented |

The column cannot catch a third spelling; the writers keep the vocabularies
closed instead — `FileOutcome` renders the outcome, `IndexFileErrorKind` renders
the error kinds, and a **degradation code is not an error kind**: it rides on an
`indexed` row, and it is `momotaro_contracts::Degradation`, whose `as_str` is the
one writer. A file can lose more than one thing at once, and the shape of that
lives in `ingest-interop.md` §5.5.

The `ingest.finished` payload carries the report's counters, and they do **not**
map one-to-one onto the rows: `files_indexed` counts both `indexed` and
`metadata_refreshed` rows, `files_skipped` counts `skipped`, and `errors` counts
`error`. An outcome breakdown is deliberately absent from the payload — the rows
are the breakdown.

### A warning is not a degradation

D41's ten codes say what a file **lost** on the way into the index. D42's three
warnings say what will not survive **another platform**: a cloud placeholder
whose bytes are not on this device (`onedrive_placeholder`), an iCloud eviction
stub standing in for a note that is not here (`icloud_placeholder`), and a name
Windows' naming rules reserve (`windows_incompatible_name`). They
are `momotaro_contracts::Warning`, not more `Degradation` variants: nothing was
lost by the file, the ten stay ten, and a reader counting degradations is not
silently taught a fourth kind of loss.

A warning rides the same `reason_code` column and the same rows, with the two
homes `ingest-interop.md` §5.5 第 8 条 gives a degradation list: `reason_code`
carries the first degradation when the row has one, and the first warning when
it does not (each list in its enum's declaration order — the order the payload
and the counts carry); `data_json` carries `{"degradations": […], "warnings": […]}` and,
for a `windows_incompatible_name` row, `{"windows_incompatible_names": […]}` —
the components the warning is about, so the row witnesses what this build
flagged (each key only when its list is non-empty). A reader that meets a code
on a non-`error` row must consult **both** vocabularies before calling it
unknown: an unknown code is still reported as a failure, and a known warning is
not — it is counted into `CoverageReport.warnings`, per code and per row state
exactly as `CoverageReport.degradations` counts, and rendered by
`doctor --coverage`. The warning rides every read-state of a file's row
(`indexed` / `metadata_refreshed` / `skipped`): the name is a fact about the
file whether or not this run read its bytes, and a report that dropped it on a
no-op re-run would call the vault clean.

### A collision is refused, and the refusal is a row

Two distinct files in one batch can derive the same `source_key` (an NFC/NFD
pair, or two paths to one object). The batch refuses the second claimant and
writes its `ingest.coverage` row with `outcome = error` and
`reason_code = key_collision`; the fuller D42 policy (keep both entries with a
disambiguating suffix) arrives with the vault-syntax work. Until then the refused
file's *raw spelling* is the only thing carried by `message`: the refusal itself is
never only text — it is a row with `outcome = error` and `reason_code = key_collision`.
Stated here rather than left to be discovered.

## `ORDER BY id DESC` rests on three premises

The read path (latest batch, coverage for a batch) is `ORDER BY id DESC LIMIT ?`.
That is a truthful "latest" only while all three hold:

1. **One writer** — the database has a single owning process (WAL, workspace not
   shared), so nothing else appends.
2. **Nothing is ever deleted** — governance is move-only: archiving goes through
   `VACUUM INTO` into an archive file that stays canonical. This is D30's rule for
   `run_events` and `history_items`, applied here for the same reason: an audit
   log with holes is not an audit log.
3. **No out-of-order imports** — an append is one transaction. The API accepts
   events in any order (a caller may hand a batch over in whatever sequence it
   built them, and the tests do), so `id` is insertion order and nothing rewrites
   it; a reader that wants the batch's own order asks for `ORDER BY seq`, which is
   why that clause is in the read path rather than left to the query planner.

If a premise falls, the read path orders by `(recorded_at, id)` and the premises
are renegotiated — not quietly patched into the query.

## A move is a hint, not a rename

`source_moved` records that a file was seen at a new path. It does **not** rewrite
a `source_key`: a renamed note is a new source
([ADR 0014](note-rename-is-a-new-source.md)), and the key is the name. The event
exists so a later tool can offer the user a "these look like the same note"
decision; making that decision is not part of this contract.

## Consequences

- One `ingest.coverage` row per file makes a batch's work visible and measurable —
  `indexed | skipped | metadata_refreshed | error` per source, beside the batch's
  own counters. The row is small. Since W2.4 the coverage rows are appended in
  one **event sweep** after the batch's data is settled (the `stale_anchor` code
  is decided against the settled state, `§5 第 4 条`), each row its own
  transaction: the loss window is the batch tail — a crash before or during the
  sweep loses the unwritten coverage rows while every data write survives, never
  the reverse.
- **Events never claim work that is not there.** Each append is one transaction,
  and the ingest path writes a file's data *before* the event that describes it,
  so a crash can lose an event but never invent one. The other half of that
  window — work that landed without its event — is what reconciliation exists
  for (D40: events are the hint, reconciliation is the truth).
- A batch-wide transaction was considered and rejected: it would hold the write
  lock across a 1000-file run and turn one crash into a lost batch. **One file
  is one transaction**: `commit_source_file` (`unit.rs`) writes the revision,
  its chunks and its `graph_links` rows — and the file's event, when the caller
  supplies one — in a single `BEGIN IMMEDIATE`, the caller's guard first. That
  is D40's unit; when a caller supplies the event it closes the *lose* half
  outright. No production caller does today: the watcher path once supplied its
  rows this way, and the nine-dimension review (2026-09-29) showed a multi-path
  debounce batch then decided `stale_anchor` mid-walk — the same walk-order
  dependence the index batch avoids by design. Rows settle at the tail sweep in
  both shapes now, so every `stale_anchor` verdict is decided against the
  settled state (`§5 第 4 条`) and both shapes keep exactly the tail window the
  first bullet states. The *never invent* half holds in both shapes, because
  every data write precedes its event.
- A batch that aborts mid-way leaves `ingest.started` with no terminal event, so a
  reader sees "running" rather than "crashed". That is stated here rather than
  discovered later; the fix, when a consumer needs it, is an `ingest.failed`
  written on the error path — and a reconcile pass supersedes the batch anyway.
- Removing rows from `store_events` requires a migration rather than a prune path;
  there is no retention policy to tune until archiving exists, and archiving is
  `VACUUM INTO`, never `DELETE`.
