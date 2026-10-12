---
title: Keep the store current
description: How the store tracks a changing vault — a watcher whose events are only hints, and the one-pass reconciliation they all share.
type: task
sidebar_position: 3
---

# Keep the store current {/* #keep-the-store-current */}

A vault is never still: files get edited, renamed, deleted, dropped in from
elsewhere. The store stays truthful through two commands that share one
primitive — a full walk of the vault that decides, from each file's own bytes,
what actually changed.

## Watch {/* #watch */}

```bash
momotaro watch
```

A long-running process for active writing sessions. It opens the file watcher
first and only then runs one full walk, so nothing that happens during startup
is missed; after that it serves events until you stop it with `Ctrl-C`.

**Events are hints, never truth.** Whatever a filesystem event claims, the
watcher treats it as "rescan that path" and lets the rescan decide. It also
reconciles the whole vault again every 24 hours, immediately when the platform
reports dropped events, and when a single burst names more than 256 paths —
at that point one full walk is cheaper, and it is the only thing that can be
sure.

With `--json`, each line is one object tagged `kind` (`batch`, `reconciled`,
`degraded`, `network_volume`, …), so a script can tell them apart. A watch on
a network drive says so and is **not** promised to be real-time.

## Reconcile {/* #reconcile */}

```bash
momotaro reconcile
```

One full walk, on demand — the same primitive the watcher runs at startup,
daily, and after dropped events. Run it by hand whenever you suspect the store
and the vault have drifted: a file deleted while the watcher was down, a
rename it missed, an index interrupted half-way.

With `--json` it prints a single summary object: `batch_id`, `scanned`,
`changed`, `removed`, `events`. Each file it touches is recorded in the audit
log, so what the pass did is reviewable afterwards.

## Which one when {/* #which */}

- Writing most days and wanting search results to stay fresh as you save?
  Leave `watch` running.
- Coming back after a break, or after anything that touched files while the
  tool was not running? One `reconcile`.
- Scripts and other machines: `reconcile` is the safe, finite call; `watch`
  runs until stopped.

## Interrupting is safe {/* #interrupt */}

Stopping either command — even at the worst moment — needs no cleanup. Each
file's rows commit together with its data, so a batch interrupted part-way
simply has not happened yet, and the next startup reconciles the difference.
You never have to repair state by hand.

## Where to go next {/* #next */}

- [Rebuild and recover](rebuild-and-recover.md) — the other repair path: the
  search index.
- [The domain model](../concepts/domain-model.md) — how revisions and chunks
  relate to files.
