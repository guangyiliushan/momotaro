---
sidebar_position: 16
sidebar_label: ADR 0016
---

# ADR 0016 · Annotations are not retrieval hits; owner-note boost does not mention them

`retrieval_hits` require the citation four-tuple. Annotation rows have no chunks. Index boost 2.0 applies only to `origin_class=owner` note chunks. Opening an annotation locates `locator` in that revision's bytes, independent of the chunker. Promoting a spark to an annotation is a later human command (`annotations.spark_id`), not a 0.x path and not a way to sneak agent text into the index.
