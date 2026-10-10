---
sidebar_position: 10
sidebar_label: ADR 0010
---

# ADR 0010 · Session Ask replays Q&A text and retrieves fresh; it does not kb_read

0.x Ask still has one provider turn and no model tools. A later turn therefore puts prior user/assistant text in history and runs retrieve again. Replaying old evidence blobs fights the 8192 budget; emitting unresolved pointers is a dead feature. `summary.v1` / gist+lookup wait until a turn is allowed to call `kb_read`.

**Status (0.x, W3.11)**: the *record* half is in place — `momotaro-context` selects history, validates `summary.v1` fail-closed against its stored text, and evaluates the compaction trigger (`0.85` / `12000`). Nothing writes a summary row yet, so the sentence above still governs. What waits for `kb_read` is **generation** and **pointer resolution**, not the trigger or the record shape.
