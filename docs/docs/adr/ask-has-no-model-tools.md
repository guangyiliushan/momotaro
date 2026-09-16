---
sidebar_position: 5
sidebar_label: ADR 0005
---

# ADR 0005 · 0.x Ask does not expose tools to the model

Revises D7.

Ask is a code pipeline: retrieve, build context, one provider turn. `retrieve` / `kb_search` / `kb_read` are engine functions, not LLM tools. A one-shot tool call cannot feed the same turn's answer; a second turn would violate the single-provider-turn rule. `tool_calls` and pointer-style `kb_read` wait for multi-turn compression or the task graph.
