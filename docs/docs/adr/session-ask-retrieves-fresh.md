---
sidebar_position: 10
sidebar_label: ADR 0010
---

# ADR 0010 · Session Ask replays Q&A text and retrieves fresh; it does not kb_read

0.x Ask still has one provider turn and no model tools. A later turn therefore puts prior user/assistant text in history and runs retrieve again. Replaying old evidence blobs fights the 8192 budget; emitting unresolved pointers is a dead feature. `summary.v1` / gist+lookup wait until a turn is allowed to call `kb_read`.
