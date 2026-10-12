---
title: Quickstart
description: From an empty directory to a searchable workspace — initialize, index notes, search, and a first cited answer.
type: task
sidebar_position: 1
---

# Quickstart {/* #quickstart */}

This page walks from nothing to a searchable workspace and a first cited
answer. The beta is a command-line tool built from the project's source;
prebuilt installers arrive with the first full release.

## 1. Build the command line {/* #build */}

You need a Rust toolchain. From a checkout of the project:

```bash
git clone https://github.com/guangyiliushan/momotaro
cd momotaro
cargo build --release -p momotaro-cli
```

The binary lands in `target/release/` — `momotaro-cli`, or `momotaro-cli.exe`
on Windows. The rest of this page writes it as `momotaro`.

## 2. Point a workspace at your notes {/* #workspace */}

A [workspace](../glossary.md#workspace) is a directory that contains a
`momotaro.toml` file and owns exactly one database. Create one with a notes
folder inside it:

```bash
mkdir study && cd study
mkdir notes
```

and give it a config that names the notes folder:

```toml title="momotaro.toml"
[vault]
path = "notes"
```

Then initialize:

```bash
momotaro init
```

```text
workspace initialized
```

Two rules keep the workspace safe: the vault must live **inside** the
workspace, and when you sync, sync the **notes folder** — never the
`.momotaro/` directory. Databases do not survive sync services.

Already have a notes tree — an Obsidian folder, a research directory?
Point `[vault] path` at it instead of creating a fresh one: the workbench
reads plain Markdown, keeps your front matter intact, and never rewrites
your files.

## 3. Index the notes {/* #index */}

Put a couple of Markdown files into `notes/`, then:

```bash
momotaro index
```

```text
indexed 2 of 2 files (2 new revisions, 0 skipped), 2 chunks, 0 errors (0 key collisions) in 90 ms
```

Indexing is deterministic — no account, no key, no network — and it reports
exactly what it did. Re-runs only process what changed: a file whose bytes
and location are unchanged adds no new revision, so re-indexing a large
library stays cheap. Checking the workspace any time is one command:

```bash
momotaro doctor
```

```text
workspace is ready (SQLite 3.53.2)
```

## 4. Search {/* #search */}

```bash
momotaro search "spectral theorem PCA"
```

```text
1. "note:pca.md"#0  "PCA and the spectral theorem"  score=11.700
    Principal component analysis diagonalizes the covariance matrix. The spectral theorem is why the eig...
2. "note:fourier.md"#0  "Fourier transform"  score=0.369
    The Fourier transform decomposes a signal into frequencies. In the discrete setting, the sampling th...
```

Lexical search spends zero model tokens and works offline. Only `ask` and
expanded search ever reach a provider; everything else — index, doctor,
trace, rebuild — runs without one. Add `--json` when a script should read
the result.

## 5. Ask, with citations {/* #ask */}

Asking spends one model turn, so it needs a provider. The config names
environment variables; the values stay in your environment:

```toml title="momotaro.toml"
[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"
```

```bash
export MOMOTARO_BASE_URL="https://api.example.com/v1"
export MOMOTARO_API_KEY="sk-…"
momotaro ask "how is the spectral theorem related to PCA?" --allow-network
```

The answer comes back with citations, the [run](../glossary.md#run) id, and a
ready-to-paste next command. Every citation has already been checked against
the recorded bytes — [retrieval and citations](../concepts/retrieval-and-citations.md)
explains what happens when a check fails.

## Where to go next {/* #next */}

- [Ask with citations](ask-with-citations.md) — sessions, retries, and the
  network policy.
- [Keep the store current](keep-the-store-current.md) — watch the vault and
  reconcile it.
- [Inspect a run](trace-a-run.md) — replay what the model saw.
- [Rebuild and recover](rebuild-and-recover.md) — when the index needs to be
  replaced.
