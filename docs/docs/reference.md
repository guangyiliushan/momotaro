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
3. `question.v1`;
4. `verification.v1`;
5. `summary.v1`;
6. `spark.v1`.

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

An LLM turn records exactly one provider request.

It preserves provider, model, request hash, input and output token usage,
terminal status, and a raw reference when the provider exposes one.

### Answer

An answer is the final durable output of a run.

It is separate from process events. Citation evidence lives in
`answer_citations`, not only in the answer text.

### Answer citation

An answer citation binds one claim in the answer to exact source evidence.

Only `source_key + revision_hash + chunk_id + chunk_hash + locator` can make a
citation verifiable.

### Verification

A verification is an independent result produced by deterministic code.

`kind` may be `citation`, `source_integrity`, or reserved `math`. A model
cannot declare its own citations valid.

### Model slot

A model slot is a semantic reference to one OpenAI-compatible endpoint. The
enum is closed: `answer | utility`. Modes map to slots statically and the
mapping is frozen into the RunSpec together with a thinking-tier snapshot
(`off | low | medium | high`). An unconfigured utility slot falls back to
answer; an empty string disables it explicitly. The verifier has no slot and
never spends tokens.

## Event vocabulary

Stable run events include:

```text
run.accepted
run.started
run.finished
run.failed
run.cancelled
retrieval.started
retrieval.finished
context.built
llm.started
llm.finished
tool.started
tool.finished
citation.validated
answer.finalized
spark.proposed
spark.pinned
spark.dismissed
```

`math.validated` remains reserved. Citation verification ships first; a CAS
backend is a later seam, not a 1.0 requirement.

`llm.finished` carries a cache summary (`cached_tokens` / `billable_tokens`
when the provider reports them). Stage-level trace names intentionally align
with the OTel GenAI semantic conventions (`gen_ai.conversation.id` ←
session_id, `conversation.compacted` ← summary present, `previous_response.id`
← preceding run); keep the export seam, rename only if the standard diverges.

### Ingest / feed events (not run events)

Index, feed poll, and note rename emit store events, not Runs:

```text
ingest.started
ingest.finished
ingest.coverage
source_moved
feed.poll.started
feed.poll.finished
```

`source_moved(old_key, new_key)` is a hint. It must not rewrite stored
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
