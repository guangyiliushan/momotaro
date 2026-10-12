---
title: Design decisions
description: The product-visible decisions behind Momotaro — what each one bought, and what it costs.
type: concept
sidebar_position: 5
---

# Design decisions {/* #decisions */}

These are the choices a user of the workbench can feel — the shape of the
product, not its internals. Each entry names the decision, the reason, and
the price paid. How each decision is implemented is engineering, and
engineering changes; what is written here is what you can rely on.

## Deterministic first {/* #deterministic-first */}

**Decision.** Searching, chunking and citation checking run without any model
involvement. The model is called only where judgment is genuinely needed.

**Because.** Reproducible behavior, offline operation, honest costs: a search
costs nothing, and the same query gives the same result on any machine.

**At the cost of.** No semantic paraphrasing in 0.x — if your words and the
document's words do not overlap, lexical search will miss. The escape hatch,
embeddings, is deliberately postponed until evidence demands it rather than
added "just in case".

## Every answer traceable {/* #traceable */}

**Decision.** Answers carry citations; every citation is checked against the
recorded revision before you see it, and a failure is shown as a failure.

**Because.** A trustable system cannot ask to be believed. Bytes can be
checked; a model's recollection cannot. The mechanics are on [retrieval and
citations](retrieval-and-citations.md).

**At the cost of.** Some answers read less fluently, because claims that fail
verification are not smoothed over — and the system stops talking when the
evidence fails, even if the model sounded confident.

## People author {/* #people-author */}

**Decision.** Only human writing is fact. Agent output stays proposed until a
person pins it, and nothing is ever written into your notes automatically.

**Because.** The desk is yours: evidence, judgment and authorship stay in
human hands. A [spark](../glossary.md#spark) becomes part of the library only
by human action.

**At the cost of.** Convenience — automations that "just file things away"
are off the table, by design.

## Local-first, zero telemetry {/* #local-first */}

**Decision.** The workbench is files on your machine plus one local database.
No account is needed to start; no telemetry, ever.

**Because.** A reading and writing trail is research data, and research data
belongs to the researcher. Nothing about your library is collected, counted
or phoned home.

**At the cost of.** No cloud sync, no cross-device magic in 0.x — moving a
workspace is copying a directory. That is a deliberate trade for
verifiability and privacy.

## One library, not two {/* #one-library */}

**Decision.** Notes and papers share one index and one result list.

**Because.** The two kinds of material are most useful side by side — a
paper hit and the note that disagrees with it, in one ranked view. Splitting
them into separate searches would hide exactly the connections the workbench
exists to surface.

**At the cost of.** More material to rank away; the origin weights in
[retrieval and citations](retrieval-and-citations.md) are the compensation.

## Archive is not deletion {/* #archive-not-deletion */}

**Decision.** Retiring material archives it; destroying it is a separate,
explicit act. Migrations take a backup before they touch anything.

**Because.** Research data accumulates history that later matters — the old
annotation, the superseded revision. Irreversibility is the enemy; a system
that can lose your work silently cannot be trusted with it.

**At the cost of.** Disk usage grows until you decide otherwise, and
housekeeping stays a human decision rather than a background "cleanup".

## No web backend {/* #no-web-backend */}

**Decision.** Desktop and the command line are the product. There is no
server-side product, and no company-held copy of your library.

**Because.** The workbench lives with the researcher — the files are already
on your machine; a backend would only move the truth away from them.

**At the cost of.** No "open it from any browser" story, and sharing means
publishing files yourself. Both are future, explicit topics rather than
silent defaults.

## Where to go next {/* #next */}

- [What Momotaro is](positioning.md) — the posture these decisions serve.
- [The domain model](domain-model.md) — the nouns they shape.
- [Retrieval and citations](retrieval-and-citations.md) — the loop they
  protect.
