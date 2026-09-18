---
sidebar_position: 2
sidebar_label: ADR 0002
---

# ADR 0002 · Source keys are scheme-prefixed logical ids, not filesystem paths

Revises D42.

A Source key is `note:<vault-relative NFC path>` or `arxiv:…` (later schemes added explicitly). 0.x allows only those two schemes: DOI is metadata, not a key; `kind=web` may exist as an enum and must not be written. Absolute paths, display names, and raw bytes live in other columns. Absolute-path keys break when a vault moves; mixing unprefixed paths with `arxiv:` invites collisions. D42's NFC / no-fold / collision rules apply to the tail after the scheme, never to an OS absolute path.

## Implementation note (2026-09-16)

The tail is **never rewritten**. `/` is the only separator, but it is the separator the vault walker *builds* when it joins path components — every other byte, including `\`, is part of the name and is preserved verbatim, so the single file `docs\ml\a.md` and the nested path `docs/ml/a.md` stay two identities. The only equivalence classes the key layer collapses are the ones the contract blesses: NFC-equivalent spellings, single-dot components, empty components, and a trailing separator.

Absolute forms — leading `/`, UNC `\\server\share`, drive-absolute `C:/` or `C:\` — and any `..` component are rejected outright rather than stripped and re-checked.

Collision policy for two distinct raw names that still land on one key (see [ingest-interop](../ingest-interop.md) §6.4: keep both entries, add a disambiguating suffix, emit `key_collision`) is **not the full D42 policy yet**. The store event table exists (schema v4, [ADR 0026](store-events-are-the-ingest-audit-log.md)), so a refusal is already a row; until disambiguating suffixes land, the indexer still refuses the second claimant in a batch and writes `ingest.coverage` with `reason_code = key_collision` — a temporary guardrail, not the contract. **解决者不必看自由文本**：拒绝本身是一行结构化事件（`outcome = error` + `reason_code = key_collision`）；只有**被拒文件的原样拼写**由 `message` 独家携带。Until that policy arrives, a refused file's *raw spelling* is the only thing carried by `message` — the refusal itself is never only text. Stated here rather than left to be discovered.

## Storage note (2026-09-17)

The key scheme is recorded per store as `schema_meta.key_scheme = "v2"`. Old dev
databases whose rows carry bare relative paths (no scheme prefix) are **not
migrated**: at `0.y.z` a rebuild is the contract (SemVer 2.0.0 §4), and mixing
two key species in one database is the silent corruption this ADR exists to
prevent. Such a file is refused with `StoreError::LegacyStore { found }` (the
refusal order and the version carrier are [distribution-trust.md](../distribution-trust.md) §8's).

The serialization is deliberately a scheme prefix plus the NFC tail, with `/` as the only separator, and the tail is rejected if it contains a NUL byte. A bare-concatenated or NUL-terminated form would make the mapping non-injective — two distinct tails could serialize to one key — and no hash function or HMAC added later can repair a non-injective encoding; it has to be injective at the source.
