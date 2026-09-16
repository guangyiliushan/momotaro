# Momotaro

A local-first research workbench for serious STEM learners. Momotaro turns
notes and papers into a traceable, verifiable desk: people judge, annotate,
and write; the agent retrieves, compares, and proposes sparks.

> Deterministic first. AI where it matters. Every answer traceable.
> Humans author. Agents assist.

This is not a general-purpose research agent and not a chat product.

## Status

The project is targeting its `0.x.y-beta` phase:

1. local-first notes and paper indexing;
2. deterministic Tantivy BM25 retrieval (lexical search uses zero LLM tokens);
3. traceable AI answers;
4. projects, annotations, and proposed sparks;
5. a Rust CLI that later powers a Tauri desktop app.

The `1.0.0` release is a cross-platform desktop app (Windows, macOS, Linux)
plus CLI. Mobile is a later read-mostly companion. There is no product web
backend.

## Repository layout

Turborepo owns JavaScript apps and docs. Cargo owns the Rust workspace.
Runnable surfaces live under `apps/` with a thin `package.json` that invokes
`cargo` or `tauri`. Rust libraries live under `crates/` and are not pnpm
packages.

```text
apps/cli          CLI (pnpm shim + Cargo binary)
apps/desktop      product UI frontend (Vite/React; Tauri wraps it at v0.6)
packages/         JS libraries (ui; app-ui and ipc planned)
crates/           Rust engine
docs/             this documentation site
```

See [Architecture](docs/docs/architecture.md) for the full map.

## Documentation

Published at:

<https://guangyiliushan.github.io/momotaro/>

Work on docs locally:

```bash
pnpm install
pnpm --filter docs start
```

## License

See [LICENSE](LICENSE).
