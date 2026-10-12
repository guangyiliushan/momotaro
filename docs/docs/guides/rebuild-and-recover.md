---
title: Rebuild and recover
description: What to do when the search index is missing or unreadable — and how to read the command that tells you so.
type: task
sidebar_position: 5
---

# Rebuild and recover {/* #rebuild-and-recover */}

The search index is a derived artifact: everything in it can be built again
from canonical storage. This page is the recovery path for when it cannot be
opened — and the exit-code rules that make scripting against it safe.

## The recovery walk {/* #walk */}

1. **Ask the workspace what it thinks:**

   ```bash
   momotaro doctor
   ```

   A healthy workspace says `workspace is ready`. An unhealthy one exits `1`
   and — when the problem is the index — names the command that fixes it.

2. **Rebuild:**

   ```bash
   momotaro rebuild-index
   ```

   The old index is discarded and built again from canonical storage. Canonical
   data is untouched. If a stale index (older schema, truncated file, missing
   segments, or an empty leftover directory) was replaced, the command and its
   JSON report **say so** — losing a derived artifact is fine, losing it
   silently is how "my results changed" becomes a mystery.

3. **Confirm:**

   ```bash
   momotaro search "the query that failed"
   ```

## Never indexed is not broken {/* #never-indexed */}

A workspace that was never indexed is *ready*, not invalid — nothing is wrong
yet. `search` on it tells you to run `index`. Keep the two states apart:
`doctor` failing means something is wrong; `search` finding no index in a
fresh workspace is just morning.

And `index` cannot repair a missing or unreadable index: it only adds files
the store has not seen. When the index itself is the problem, the command is
`rebuild-index` — the message says so instead of quietly indexing zero files.

## Exit codes for scripts {/* #exit-codes */}

- `0` success, `1` failure — with one deliberate exception: `doctor` gates on
  **health**, so an uninitialized or invalid workspace exits `1` even though
  the report printed fine. (`doctor --coverage` is a *report*, not a check:
  it exits `0` whenever the log could be read, including "nothing has
  happened yet", where `--json` prints `null`.)
- A usage error (unknown flag or subcommand) is `2` — and even `--json` gets
  no JSON body, because parsing fails before the program's error outlet runs.
- `index` per-file errors do **not** fail the batch: a summary with
  `N errors` (N > 0) still exits `0`, because every failure was recorded and
  the batch completed.

## Where to go next {/* #next */}

- [Keep the store current](keep-the-store-current.md) — the other repair
  path: keeping up with vault changes.
- [The domain model](../concepts/domain-model.md) — why the index is
  rebuildable by design.
