---
title: Configuration
description: The momotaro.toml file — vault location, model endpoint wiring, and the network policy — with secrets kept in the environment.
type: reference
sidebar_position: 2
---

# Configuration {/* #configuration */}

A Momotaro workspace is a directory that contains a `momotaro.toml` file and
owns one database (`.momotaro/`). The config file is short by design: it
names locations and environment variables, never secrets.

```toml title="momotaro.toml"
[vault]
path = "notes"

[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"

[policy]
allow_network = false
```

## `[vault]` — where the notes are {/* #vault */}

- `path` — the notes tree to index, relative to the workspace (or absolute,
  but it must resolve **inside** the workspace; `vault == workspace root` is
  legal). Stored records are workspace-relative, which is why the vault cannot
  live elsewhere.

## `[model]` — the provider endpoint {/* #model */}

- `model_id` — the model to ask for.
- `base_url_env` — the **name** of the environment variable that holds the
  API root (for example `MOMOTARO_BASE_URL`). The value itself stays in your
  environment, not in the file.
- `api_key_env` — the name of the environment variable holding the key;
  omit it for endpoints that need no key.

The endpoint is only needed the moment something actually talks to a model —
`ask` and `search --mode expand`. Until then, the rest of the toolchain is
offline by construction.

## `[policy]` — what is allowed by default {/* #policy */}

- `allow_network` — Beta ships conservative: network access is **off** unless
  you allow it, per command (`--allow-network`) or once here. While it is off,
  `ask` and `search --mode expand` are refused before any connection is made.

## Two habits that keep a workspace healthy {/* #habits */}

- **Secrets never enter the file.** Only environment-variable names do; the
  values live where secrets belong.
- **Sync the notes, not the workspace.** The `.momotaro/` directory holds a
  database; sync services corrupt databases. Sync the vault folder only.

## Where to go next {/* #next */}

- [Command line reference](cli.md) — every command and its options.
- [Quickstart](../guides/quickstart.md) — all of this in action.
