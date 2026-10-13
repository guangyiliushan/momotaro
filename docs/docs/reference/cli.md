---
title: Command line reference
description: Every Momotaro command with its arguments and exit behavior — the stable surface of the 0.x tool.
type: reference
sidebar_position: 1
---

# Command line reference {/* #cli */}

All commands share one binary and the same three global options. This page
lists what exists, what it takes, and how it exits — deliberately not every
flag a future version may add.

## Commands {/* #commands */}

| Command | Does |
|---|---|
| `momotaro init` | Initialize the local store in the current directory and validate the config. |
| `momotaro index [PATH]` | Index Markdown notes (a `PATH` overrides the configured vault). Deterministic, offline. |
| `momotaro search [QUERY]…` | Lexical search over the index. Zero model tokens by default. |
| `momotaro ask [QUERY]…` | Answer from the notes with citations; at most one model turn. |
| `momotaro trace <RUN_ID>` | Replay one run: retrieval, context, citations, verdicts. |
| `momotaro doctor` | Check workspace health without mutating anything. |
| `momotaro stats` | Print store counts: status, database path, sources, chunks. |
| `momotaro watch` | Keep the store current while the vault changes (long-running). |
| `momotaro reconcile` | Bring the store up to date with the whole vault in one pass. |
| `momotaro rebuild-index` | Rebuild the search index from canonical storage. |

Command-specific options worth knowing:

- `search`: `-n/--top <N>` limits hits (default 10; `0` is legal and returns
  nothing). `--mode lexical|expand` picks pure BM25 (default) or one LLM
  rewrite before retrieval.
- `ask`: `--session <id>` continues an existing session; `--client-request-id
  <id>` makes a retried request replay-safe. `ask` refuses `--mode expand`: an
  answer always takes at most one model turn.
- `doctor`: `--coverage` reports what the most recent index run did to each
  file, instead of checking health.

## Global options {/* #global-options */}

| Option | Meaning |
|---|---|
| `--json` | Emit machine-readable JSON on stdout. |
| `--no-llm` | Forbid every model call; `ask` and `search --mode expand` refuse. Lexical `search` is unaffected. |
| `--allow-network` | Allow network access for commands that need it. Beta keeps it off by default. |
| `-h`, `--help` / `-V`, `--version` | Help and version. |

## Exit codes {/* #exit-codes */}

- **`0`** success, **`1`** failure — with one deliberate exception: `doctor`
  gates on *health*, so an uninitialized or invalid workspace exits `1` even
  though the report printed fine. (`doctor --coverage` is a report, not a
  check: it exits `0` whenever the log could be read.)
- A usage error (unknown flag or subcommand) exits **`2`** — and even
  `--json` gets no JSON body, because parsing fails before the program's
  error outlet runs.
- `index` per-file errors do not fail the batch: a summary with `N errors`
  (N > 0) still exits `0`.

The reasoning behind these rules — and the recovery path they exist for — is
on [rebuild and recover](../guides/rebuild-and-recover.md).

## Example: `stats` {/* #example-stats */}

```text
status: ready
schema version: 9
database: …/.momotaro/momotaro.db
initialized: true
sources: 2
chunks: 2
```

`--json` carries the same fields. A count that is not yet known prints
`unknown` on both outlets — never a fabricated `0`.

## Where to go next {/* #next */}

- [Configuration](configuration.md) — the `momotaro.toml` file.
- [Quickstart](../guides/quickstart.md) — the commands in sequence.
