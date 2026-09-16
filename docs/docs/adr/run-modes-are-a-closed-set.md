---
sidebar_position: 17
sidebar_label: ADR 0017
---

# ADR 0017 · 0.x Runs are ask, explicit search, paper_add, and spark

Revises D3 / D7.

A Run is accepted knowledge work with a frozen RunSpec and a trace. Index and feed poll are writes on the same SQLite queue and emit ingest/feed events; they are not traces. Annotate is a store write. Desktop typeahead is not a Run; submitting search is. `runs.mode` has no `annotate` or `ingest`. Coverage honesty still lives in ingest events.
