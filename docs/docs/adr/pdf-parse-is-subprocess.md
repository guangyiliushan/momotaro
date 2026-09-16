---
sidebar_position: 6
sidebar_label: ADR 0006
---

# ADR 0006 · Default PDF parse is an out-of-process pure-Rust job from the first paper-add

Revises D18 / D37.

Canonical library rows are not rebuildable from a crash. Abort-class failures (stack overflow, OOM) are not containable in-process even in pure Rust, so the default parser is a spawned process with the versioned stdin/stdout protocol — the CLI spawns the same binary; it does not wait for a Tauri sidecar. D18's “in-process default, subprocess only for GROBID/Marker” is superseded for the default layer; high-end parsers still use that same protocol.
