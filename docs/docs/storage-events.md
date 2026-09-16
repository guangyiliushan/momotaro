---
sidebar_position: 7
---

# 会话事件日志与事实元数据存储

## 结论

Momotaro 的存储分两个面，不混在一起：

| 面 | Canonical | Derived / 可重建 |
|---|---|---|
| 会话管理 | SQLite 内的 `runs` + `run_events` + `history_items` 事件流 | JSONL 导出、备份、迁移 |
| 论文与事实元数据 | SQLite 的 `source_revisions`、`chunks`、papers、projects 等关系表 | Tantivy BM25；后置 LanceDB 向量投影 |

会话管理可以参考“不可变事件日志 + 查询索引”结构，但事件流留在 SQLite 内，
JSONL 不作为运行时 source of truth。论文和事实元数据以 SQLite 为 canonical，
LanceDB 只做可重建的向量检索投影。

这保留事件日志模式真正有价值的部分：不可变、可重放、可重建投影，同时避免为
JSONL 单独实现投影折叠、单写者协调和 crash recovery。

## 为什么会话事件日志不让 JSONL 当 source of truth

把 JSONL 当 canonical 日志、SQLite 当索引的模式在通用 Agent harness 里成立。但
Momotaro 的会话不是只读 transcript，还要和事实元数据频繁做关系连接：

- citation 需要校验 `source_key + revision_hash + chunk_id + chunk_hash + locator`；
- retrieval 需要区分 `retrieval_hits`、`run_context_items`、`answer_citations`；
- Project / Annotation / Spark 需要关联、过滤和事务；
- 索引重建不得影响 run 历史。

SQLite + WAL 原生提供这些事务和一致性。JSONL 若要成为权威，就需要额外的投影折叠、
重建检测、单写者协调和 crash recovery，正好是 DeepSeek Harness 为了解决
“按会话文件日志”而专门做了 `session-persistence` seam、generation 命名和
torn-tail 恢复的原因。Momotaro 当前不需要重复这一整套机制。

## 三个真实变体

### A. JSONL 为 source of truth，SQLite 为索引

代表：Codex CLI、DeepSeek Harness 默认后端、Bernstein、Pi。

它们适合“一个会话一份日志、整段重放、外部查看”的 Agent harness：

- Codex 把 rollout 写进 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`，
  `state_*.sqlite` / `logs_*.sqlite` 是 cache、state 和遥测；
- DeepSeek Harness 的 JSONL 后端保存 immutable generation 文件，可压缩、可恢复；
- Bernstein 的 episodic layer 是 append-only JSONL，semantic layer 是 SQLite FTS5；
- Pi 默认用 JSONL session 文件，并把 SQLite backend 作为独立可选实现。

### B. SQLite 为 source of truth，JSONL 为备份 / 导出

代表：Hermes Agent。

Hermes 的 `~/.hermes/state.db` 是 canonical，保存 session metadata、messages 和
FTS5 索引；`~/.hermes/sessions/*.jsonl` 是 gateway transcript 和请求 breadcrumb。
它的文档明确写“state.db is canonical”，JSON snapshot 默认关闭。

### C. SQLite 内嵌 JSON，不额外做 JSONL 运行时

代表：ENGRAM、AISAC。

- ENGRAM 把 typed memory records 规范化为 JSON，连同 embedding 持久化在 SQLite；
- AISAC 用 SQLite + FAISS 组成 hybrid persistent memory，SQLite 负责关系历史。

这种变体最接近 Momotaro：关系字段进列，原始负载进 `data_json` / `payload_json`，
查询和一致性都留在 SQLite。

## 对 Momotaro 的具体落点

### 1. 会话管理：事件日志留在 SQLite

现有模型已经是逻辑上的 event sourcing：

```text
runs            -> 被接受的工作，聚合和终态
run_events      -> append-only process event stream
history_items   -> append-only transcript stream
retrievals/...  -> 检索和上下文事实
answers/...     -> 最终输出和引用
```

写入规则：

- `run_events` 和 `history_items` 只追加，不修改、不删除；
- 每个事件带 `run_id + seq + type + data_json + created_at`；
- `run.finished` / `run.failed` / `answer.finalized` 必须原子落库；
- 索引重建永远不触碰这些表。

### 2. 会话 JSONL 只做导出和导入

如果未来需要给用户一个可读、可 grep、可备份的文件，用 `momotaro export` 从
canonical 表**投影生成** JSONL，而不是在写路径上双写。导出事件建议用一个稳定 envelope：

```json
{"type":"run.accepted","seq":1,"run_id":"...","timestamp":"...","data":{}}
{"type":"retrieval.finished","seq":2,"run_id":"...","timestamp":"...","data":{}}
{"type":"citation.validated","seq":3,"run_id":"...","timestamp":"...","data":{}}
```

导入只用于迁移或恢复，并且必须先校验 schema、hash 和目标库状态，再进入
`momotaro-store` 的 canonical 事务。运行时查询绝不直接读 JSONL。

### 3. 论文与事实元数据：SQLite canonical，LanceDB 只做向量投影

论文、笔记、chunk、project、annotation 这些关系事实全部进 SQLite。它们需要事务、
外键、唯一约束和 citation 连接，LanceDB 不适合承担这层。

LanceDB 定位为后置的 derived 向量索引：

```text
SQLite source_revisions / chunks  ->  embed  ->  LanceDB vectors
                                             ->  Tantivy BM25
```

向量库中的记录必须保留回 SQLite 的稳定键：
`source_key + revision_hash + chunk_id + chunk_hash`。语义检索命中后，引用和
`answer_citations` 仍以 SQLite canonical chunk 为准；LanceDB 不单独定义权威事实。

当前先用 Tantivy BM25，LanceDB 是向量检索后置项。`run_events.data_json`、
`history_items.payload_json` 可保存原始 provider payload，需要全文检索的是 source
chunk，不要因为参考了 Hermes / Bernstein，就把 SQLite FTS5 或 LanceDB 变成主检索。

### 4. 借 DeepSeek 的持久化语义，不借后端抽象

最有价值的三条是：

- append-only，先写完整事件再 publish 后继；
- 每个物理 event 可校验，撕裂尾部不能到达 reader；
- 每个会话一个单写者。

Momotaro 可以用 SQLite 事务和 `seq` 唯一约束实现同样语义。当前**不要**做
persistence backend registry 或 JSONL/SQLite 双后端抽象，YAGNI；等真需要迁移时再在
`momotaro-store` 暴露一个窄接口。

## 与现有决策的对账

Architecture 已拒绝“JSONL sidecar 作为运行时状态”。本文不推翻该决策，只补充两个
允许的周边动作：

- 导出 JSONL 是 derived artifact，删除后可重建；
- 导入 JSONL 是显式迁移动作，完成后 JSONL 不继续作为运行时读取路径。

这比“JSONL + SQLite 双写为权威”更少代码、更少一致性风险，也更符合 Momotaro
本地优先、可审计、可重建的原则。

崩溃恢复、单写者与并发模型的具体规则见 [Harness 工程](harness.md)；检索评测与
LanceDB 投影的引入门槛见 [上下文与检索工程](context-retrieval.md)。

2026-09-15 会话管理调查复核：Codex 用 JSONL + 两座 SQLite 投影才达到的效果，
Momotaro 一座 SQLite 直接拥有——「SQLite canonical」决策被双库对照再证；
要学的是**投影可重建**（如 Tantivy 会话索引从 history_items 重建），不是
存储形态。`occurred_at` / `created_at` 双时间戳在公开同类中无先例，保留并在
重放/导入断言中明确「occurred_at 保持事件时刻」。

## 参考

- [ENGRAM](https://arxiv.org/abs/2511.12960)：SQLite 内持久化 normalized JSON +
  embedding 的轻量记忆编排。
- [AISAC](https://arxiv.org/abs/2511.14043)：SQLite + FAISS 的透明科研助手
  持久化。
- [Codex CLI session schema](https://github.com/neochoon/agenthud/blob/main/docs/schemas/codex-session.md)：
  rollout JSONL 为 source of truth，SQLite 为 cache/state。
- [Hermes Sessions](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/sessions.md)：
  SQLite canonical，JSONL transcript。
- [Bernstein SessionMemory](https://github.com/sipyourdrink-ltd/bernstein/blob/main/docs/memory/session-memory.md)：
  episodic JSONL + semantic SQLite FTS5。
- [DeepSeek Harness session](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/session/README.md)：
  durable session persistence seam，JSONL 为默认后端。
