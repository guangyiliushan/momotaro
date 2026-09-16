---
sidebar_position: 4
sidebar_label: ADR 0004
---

# ADR 0004 · v0.3 same-library retrieval is mixed BM25, not 1-hop attachment

Revises D33.

Notes and papers share one Tantivy index. One query returns mixed `origin_class` hits with origin boosts. That is the v0.3 meaning of 同库互检. Graph 1-hop “paper hit brings related notes” waits on `graph_links` (v0.5) and must not appear in current positioning. A second query per paper hit would change ranking and golden-set methodology without a documented need.
