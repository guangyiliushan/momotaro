---
sidebar_position: 13
sidebar_label: ADR 0013
---

# ADR 0013 · Pin without a citation five-tuple is allowed and is not indexed

Revises D24.

Pin is a human act. Retrieval eligibility is a separate fact: at least one `source_key + revision_hash + chunk_id + chunk_hash + locator` pointing at a live chunk. Forbidding pin-without-citations leaves no drawer except dismiss. Treating `run_id` / `history_id` as provenance would launder the episodic layer into a retrieval source.
