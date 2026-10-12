---
title: Ask with citations
description: Turning a question into a recorded, verifiable run — sessions, replay-safe retries, and what a citation guarantee means.
type: task
sidebar_position: 2
---

# Ask with citations {/* #ask-with-citations */}

`ask` retrieves in code, builds the context in code, makes at most one model
turn, and binds the answer to bytes. This page covers what comes back and the
three knobs around it.

## What comes back {/* #what-comes-back */}

- the answer, with numbered citations;
- the [run](../glossary.md#run) id, plus a ready-to-paste `momotaro trace
  <run_id>`;
- the verdict of each [citation](../glossary.md#citation) check — a citation
  that failed is shown as failed, never quietly dropped.

If the run failed, the command exits `1` — and still prints the answer and its
explicit failure marks, so a script and a person both see the same thing.

## Sessions {/* #sessions */}

Without `--session`, every ask opens a fresh session. To continue one, pass
its id (both outlets of the previous answer print one):

```bash
momotaro ask --session <session_id> "and what about the noise term?"
```

The id must name a session that exists — a typo is an error rather than a
silently new conversation. If the previous round was interrupted by a crash,
the next command sweeps the stale round first and continues; there is nothing
to clean up by hand.

## Replay-safe retries {/* #retries */}

Automation that retries gets exactly-once behavior through the client request
id:

```bash
momotaro ask "…" --client-request-id retry-7 --allow-network
```

Replaying the same id returns the **same** run — no duplicate work, no
duplicate spend. Reusing an id with a different request is refused by name
(the idempotency key conflicts), and an empty id is refused. Let the id name
the logical request, not the attempt.

## The network gate {/* #network */}

Asking talks to a model provider, so it is refused unless the network is
explicitly allowed — `--allow-network` on the command, or once in the config:

```toml title="momotaro.toml"
[policy]
allow_network = true
```

A refusal happens **before** any connection: nothing is recorded, and the
exit code is `1`. `--no-llm` refuses `ask` outright — there is no in-between
mode where it would answer without a model.

## When a citation fails {/* #failure */}

A failed citation fails the run. That is the design: the answer is not
"cleaned up" to hide the failure, and the trace shows exactly which quote
could not be found in the recorded revision. Treat a failed run as a signal
about the material, not about the command — the check did its job.

## Where to go next {/* #next */}

- [Inspect a run](trace-a-run.md) — see everything the model saw.
- [The citation loop](../concepts/retrieval-and-citations.md) — the concept
  behind the marks.
- [The glossary](../glossary.md#verification) — what "verification" means
  precisely.
