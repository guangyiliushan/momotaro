---
sidebar_position: 23
sidebar_label: ADR 0023
---

# ADR 0023 · 0.x snapshots are the database; papers are excluded

Revises D30 / D32.

D32's four-step backup remains `VACUUM INTO` of the SQLite file. The snapshot manifest records `papers: excluded`. A full copy of `.momotaro/papers/` is a later, explicit backup, not the default product. Restore never deletes revisions because bytes are missing: search, citations, and trace still work from chunk text; opening the PDF is a local `source_missing` / `source_integrity` failure. Putting PDFs in SQLite BLOBs is refused (engine and WAL constraints). Folding papers into the VACUUM path would turn a short, crash-safe transaction into a long copy.
