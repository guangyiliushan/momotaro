---
sidebar_position: 5
---

# Reference

The glossary is `CONTEXT.md` at the repo root (not a site route).

## Core objects

### Source revision

A source revision is the immutable version of a note, paper, or web source.

Key fields:

1. `source_key` (scheme-prefixed: `note:<vault-relative NFC path>` or `arxiv:<id>`; renaming a note is a new Source, old keys are not rewritten);
2. `revision_hash`;
3. `kind`;
4. `origin_class`;
5. `local_path` (workspace-relative);
6. ingestion metadata.

Missing bytes leave the row in place; opening fails as source integrity.

### Chunk

A chunk is a retained projection of one source revision: the retrieval and citation locator.

Once written for a `(source_key, revision_hash)`, rows stay while any citation or context still points at them. Same policy and revision → idempotent replace. Changing the hash function is a new index policy. A full wipe is disaster recovery and must emit drift.

`chunk_hash` identifies and deduplicates; it does not prove that this store's history was never rewritten. A check that reads only the current bytes cannot tell "rewritten" from "never so": source integrity means the bytes named by the row are present and match the hash, not that an earlier row was never deleted.

A chunk records:

1. source key;
2. revision hash;
3. ordinal;
4. heading path;
5. text;
6. locator;
7. deterministic chunk hash.

### Retrieval hit

A retrieval hit records what search found.

It is not the same as model context.

### Context fragment

A context fragment records a bounded piece of information prepared for the
model.

Fragments are typed and versioned, for example:

1. `policy.v1`;
2. `evidence.v1`;
3. `question.v1`.

`summary.v1` is not a fourth type: it is a **content kind**, labelling the
body of a `history` fragment on `run_context_items.content_kind`
(`context-retrieval.md` §3.1).

### Run

A run is an auditable unit of work.

It preserves the query, frozen run specification, retrieval result, model
context, provider turn, verification, and terminal status. `tool_calls` is a
contract reservation; 0.x Ask does not produce tool calls.

0.x modes are `ask`, `search` (explicit submit), `paper_add`, and `spark`.
Index, feed poll, annotate, and typeahead are not Runs.

Accepting a run is not the same as executing a model call.

### Project

A project is a human desk drawer: a named collection of sources (and later
annotations / pinned sparks). Deleting a Project does not delete Sources. It
is not a multi-tenant workspace.

### Annotation

A human note pinned to `source_key + revision_hash + locator`.

When the source gains a new revision, old annotations stay on the old
revision. The UI may show that annotations are stale relative to `is_current`.

### Spark

A short, citation-backed inspiration card produced by an explicit `spark` run.

Status is only `proposed | pinned | dismissed`. Pin is a human act and does
not require citations. Retrieval eligibility is `pinned` plus at least one
citation five-tuple pointing at a live chunk. Empty citations make the card
`unverified`. 0.x actions are pin and dismiss only.

### Source adapter log

A source adapter log records the outcome of a discovery operation against one
paper or feed source.

Outcomes are only `success`, `no_results`, `upstream_error`, or
`parse_error`. A missing result and an unavailable source are different facts
and must not be collapsed into the same UI or query result.

### Context scope

A context scope decides which retrieval hits may enter `run_context_items`.

`P1` is the current Project plus explicitly selected sources. With no Project,
`P1` is the whole Vault and the P1/P2 split is a no-op. `P2` is the remaining
searchable vault. `ask` and `spark` default to `P1`; `search` covers `P2`.
Wider scope can be requested explicitly, but `P2` evidence ranks behind `P1`
and remains subject to the token budget.

### Tool call

A tool call records one bounded execution against the frozen tool contract.

It preserves the tool name, validated arguments, full structured value,
model-visible output, replay policy, tool version, and terminal status.

### LLM turn

An LLM turn records exactly one provider exchange: one client call, however
many attempts it took.

It preserves provider, model, request hash, input and output token usage,
terminal status, and a raw reference when the provider exposes one. A
client-side retry that fires before any observable output is part of the same
exchange — it is not a second `llm_turns` row. The per-exchange attempts
count is carried on the client's typed result (`LlmOutput::attempts` /
`LlmError::attempts`); `llm_turns` has no column for it yet (`未实现` — a
persisted attempts count lands with a schema change, in the same commit as
the column).

### Answer

An answer is the final durable output of a run.

It is separate from process events. Citation evidence lives in
`answer_citations`, not only in the answer text.

### Answer citation

An answer citation binds one claim in the answer to exact source evidence.

Only `source_key + revision_hash + chunk_id + chunk_hash + locator` can make a
citation verifiable.

The surface protocol: the answer cites evidence fragments inline as `[f0001]`-style
markers and ends with a `Citations:` block, one line per citation — `[f0001] the
exact quoted span from that fragment`. The quote must appear verbatim in the cited
chunk (architecture §14.1.3). Failures are explicit: a marker whose quote is missing
or not locatable is a `text_not_locatable` failure, a block line that is not
`[fDDDD] …` becomes a `citation_invalid` claim rather than prose, and a marker
naming no context item is `citation_invalid` (§14.1.4). The marker format belongs to
the run layer; `momotaro-verify` treats the id as opaque. (Shape note: quote-carrying
citations follow the `cited_text` family of provider citation APIs rather than
numeric-only `[1]` markers, because §14.1.3 requires the quote to be locatable.)

### Verification

A verification is an independent result produced by deterministic code.

`kind` may be `citation`, `source_integrity`, or reserved `math`. A model
cannot declare its own citations valid.

### Model slot

A model slot is a semantic reference to one OpenAI-compatible endpoint. The
enum is closed: `answer | utility`. Modes map to slots statically and the
mapping is frozen into the RunSpec together with a thinking-tier snapshot
(`off | low | medium | high`). An unconfigured utility slot falls back to
answer; an empty string disables it explicitly. (**1.x only** — this build has no
`[model_slots.*]` configuration surface at all, so neither branch is reachable
yet and every mode is pinned to `answer`; §20 registers the slot matrix as
awaiting its consumer.) The verifier has no slot and
never spends tokens.

## Event vocabulary

Stable run events include:

```text
run.accepted
run.started
run.finished
run.failed
retrieval.started
retrieval.finished
context.built
llm.started
llm.finished
citation.validated
answer.finalized
```

Six more spellings are reserved with no producer yet — `run.cancelled`
(it has a `TerminalEvent` variant and a round-trip test; only `finished`
and `failed` are ever written), `tool.started`,
`tool.finished`, `spark.proposed`, `spark.pinned` and `spark.dismissed`.
**This list is the reservation** — it is what stops a seam from landing under a
different name. Of the six, only `run.cancelled` is *declared* in code
(`TerminalEvent::Cancelled`, with a round-trip test and no producer); the
other five are names and nothing more until their seams land, and a seam that
lands must take these spellings rather than invent its own. Until something
writes them they are not stable events.

`math.validated` remains reserved. Citation verification ships first; a CAS
backend is a later seam, not a 1.0 requirement.

`context.built` is the one event here whose `data_json` carries a payload
worth naming, because it is where §4's 观测三指标 will be counted from:

```jsonc
{
  "fragments": 12,                 // fragments the build kept
  "compaction": {
    "triggered": true,             // did this build cross the line?
    "raw_total": 1640,             // the estimate, before the coefficient
    "adjusted_total": 2050,        // …after it — what the line is compared against
    "max_context_tokens": 8192,    // the ceiling the verdict is priced in
    "trigger": 1638,               // min(max_context_tokens - reserve, …)
    "reserve": 0                   // min(compact_reserve_abs, 25% × budget)
  }
}
```

The numbers above are **illustrative at an invented `[policy]`** (ratio 0.2,
reserve 0), not at the documented defaults — at the documented defaults (budget and
coefficient from `context-retrieval.md` §2.1, trigger ratio and reserve from
§4) the same build would report `trigger: 6144`, `reserve: 2048` and
`triggered: false`, which shows every field moving but is a duller picture.
The shapes and the relationships are what the sample pins.

`triggered` is a **recorded verdict, not a derivation**: `adjusted_total >=
trigger` is how it was decided, but the fact is written down so a later
metric can count firings without re-running the arithmetic against a budget
that has since changed. **Both sides are `adjusted`** — `trigger` is
expressed in `max_context_tokens`, which is the *adjusted* ceiling (the same
unit `exceeded` and the budget's own claims use); comparing `raw_total`
against it would move the firing point by the coefficient, and at
`context-retrieval.md` §2.1's default 1.25 a line documented as 75% of the
budget would only fire at
93.75%. `adjusted_total` rides along so the verdict can be checked against
the number it was decided from. `max_context_tokens` rides along for the
mirror reason: it is the ceiling that comparison is priced in, and neither
`adjusted_total` nor `triggered` can be read back out of. It is also the only
thing in the payload that says **which** budget the run was frozen against —
`trigger` and `reserve` are derived from it, so a later reader holding a
changed `[policy]` can still tell what the line meant at the time.

Nothing compacts yet — [ADR 0010](adr/session-ask-retrieves-fresh.md)'s status
marker says generation waits for `kb_read` — so today the field is always an
observation of a line the build did not act on.

`llm.finished` carries a cache summary (`cached_tokens` / `billable_tokens`
when the provider reports them). Three OTel GenAI names are **reserved, not
emitted**: this build has no export seam, no dependency and no referent for
`gen_ai.conversation.id` (session_id), `conversation.compacted` (summary
present) or `previous_response.id` (preceding run) — an export seam, when one
lands, should adopt them; rename only if the standard diverges by then.

### Ingest / feed events (not run events)

Index, watch, and reconcile emit store events, not Runs:

```text
ingest.started
ingest.finished
ingest.coverage
source_moved
vault.reconcile.started
vault.reconcile.coverage
vault.reconcile.finished
```

Two more spellings are reserved with no producer yet — `feed.poll.started`
and `feed.poll.finished`. This build has no feed polling at all (`[feeds]`
is unimplemented, architecture §7), so the only places they are written are
tests that pin ADR 0026's vocabulary; they are listed in that vocabulary for
the same reason `run.cancelled` is listed among the run events above.

`index` and `watch` both write the `ingest` domain — a watcher batch is an
ingest batch, so `doctor --coverage` reports whichever of them wrote last.
The four reconciliation triggers write the `vault` domain.

`source_moved(old_key, new_key)` is a hint, written only for a rename the
debouncer actually paired (a path that vanished with no partner is a
deletion, and the batch's removal row says so). It must not rewrite stored
`source_key` values. Missing bytes at `local_path` are `source_missing` /
`source_integrity`.

## Verification states

| State | Meaning |
|---|---|
| `passed` | Verification succeeded. |
| `failed` | Verification explicitly failed. |
| `unsupported` | The system cannot verify this target. |
| `skipped` | Verification was disabled or intentionally omitted. |

For citations, only exact `source_key + revision_hash + chunk_hash` matches
can produce `passed`. A model cannot declare its own citations valid.

## Surfaces

| Surface | Role |
|---|---|
| CLI (`apps/cli`) | Engine proof, automation, debug |
| Desktop (`apps/desktop`) | Primary product at 1.0 |
| Mobile (`apps/mobile`) | Later read-mostly companion |
| Docs (`docs/`) | This site, GitHub Pages |
| www (`apps/www`) | Later marketing site; no engine |

There is no product web backend.

## Release labels

| Phase | Meaning |
|---|---|
| `0.x.y-beta` | Engine and CLI, then desktop main path |
| `1.0.0` | First formal cross-platform desktop + CLI |
| after 1.0 | Mobile companion, optional task graph, marketing site |

## Detailed architecture

See [Architecture](architecture.md).
