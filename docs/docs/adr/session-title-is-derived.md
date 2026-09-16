---
sidebar_position: 21
sidebar_label: ADR 0021
---

# ADR 0021 · 0.x session titles are derived from the first user message

`title_source` stays `derived | user | llm` in the schema. 0.x only writes `derived` (truncate the first user message by characters, including CJK; normalize whitespace). A user rename is never overwritten. LLM titles wait for the utility slot and are not on the Ask path — an extra turn would break no-key Ask and the single-turn rule.
