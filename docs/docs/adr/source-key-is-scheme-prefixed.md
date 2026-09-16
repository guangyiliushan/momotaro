---
sidebar_position: 2
sidebar_label: ADR 0002
---

# ADR 0002 · Source keys are scheme-prefixed logical ids, not filesystem paths

Revises D42.

A Source key is `note:<vault-relative NFC path>` or `arxiv:…` (later schemes added explicitly). 0.x allows only those two schemes: DOI is metadata, not a key; `kind=web` may exist as an enum and must not be written. Absolute paths, display names, and raw bytes live in other columns. Absolute-path keys break when a vault moves; mixing unprefixed paths with `arxiv:` invites collisions. D42's NFC / no-fold / collision rules apply to the tail after the scheme, never to an OS absolute path.
