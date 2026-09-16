---
slug: /
sidebar_position: 1
---

# Momotaro

Momotaro 是一个**本地优先、给人用的科研工作台**。它把笔记、论文和后续网页来源连成可检索、可引用、可验证的知识网。人做判断、批注、选题和写作；Agent 只打下手。

> Deterministic first. AI where it matters. Every answer traceable.
> 人是作者。Agent 是工人。

它不是全能科研 Agent，也不是聊天产品。核心资产是：

1. 统一来源模型：笔记、论文、后续网页都变成 `Source -> Revision -> Chunk`。
2. 人的书桌：`Project`、`Annotation`、`Spark`（启发卡，默认 `proposed`）。
3. 确定性优先：检索、分块、引用校验尽量不消耗 LLM token。
4. 可审计执行：每次 `ask` / `search` 都能回到当时的 query、证据、prompt、模型和验证结果。
5. 可升级边界：同一套 Rust 服务函数供给 CLI、桌面和后置的移动端。

## 和全能科研 Agent 的区别

| | 全能科研 Agent | Momotaro |
|---|---|---|
| 主角 | Agent 把流程跑完 | 人做判断；Agent 检索、对照、给启发 |
| 成功标准 | 任务闭环、自动报告 | 人能追溯证据、留下批注、把启发收进项目 |
| 默认动作 | 自己搜、自己下、自己写 | 先展示；写入、下载、入库要人点头 |
| 知识 | 模型说了就算 | 人手写才是 `owner` 事实；AI 产出永远是 `proposed` |
| 工作单元 | 自由 tool loop | `Project` 是书桌；`Run` 是一次可审计劳动 |

## 发布方向

### `0.x.y-beta`

用 CLI 把引擎契约跑硬：

1. 本地 Markdown / Obsidian 索引；
2. Tantivy BM25 检索（普通搜索零 LLM）；
3. 可追溯的 `ask`；
4. arXiv 论文入库（有确认门）；
5. Feed 轮询 + OPML 导入；
6. Project / Annotation / Spark；
7. run 级 trace 回放。

### `1.0.0`

第一个正式用户版本是跨平台**桌面应用**（Tauri 2）加 CLI。移动端是后置的只读伴侣，不是 1.0 阻塞项。

产品 Web 不做引擎后端。文档继续本站（Docusaurus / GitHub Pages）。营销独立站（`apps/www`）后置，与产品 UI、文档站分开。

仓库从第一天按 Turborepo 惯例组织：`apps/` 放可运行面，`packages/` 放 JS 库，`crates/` 放 Rust 库。

## 从这里开始

- [Usage](usage.md)
- [Development](development.md)
- [Reference](reference.md)
- [Architecture](architecture.md)
- [Decision records](adr/chunks-are-a-retained-projection.md)
- [Harness 工程纪律](harness.md)
- [上下文与检索工程](context-retrieval.md)
- [Python 与 Rust 协作边界](python-rust.md)
- [任务图（后置）](multi-agent.md)
- [Light-skills 融合边界](light-skills-integration.md)
- [JSONL 与 SQLite 的存储边界](storage-events.md)
- [规模与数据生命周期](scale-lifecycle.md)
- [产品信任面与分发工程](distribution-trust.md)
- [摄取安全与 Vault 互操作](ingest-interop.md)

## 决策索引

详细决策见 [Architecture](architecture.md) 的决策记录与上述专题文档。结论索引：

- 检索与图路线：BM25 优先、零 LLM 索引、机制级引用、评测门槛；
- 上下文工程：有限资源预算、缓存第一等约束、四层分离
  （evidence/history/constraints/artifacts）、provenance 安全边界、评估闭环；
- LLM Wiki：形态学 LLM Wiki、治理学 Momotaro（D21/D22；proposed wiki 层 +
  零 LLM 治理工具包）；
- 会话管理：会话骨架零项被推翻；压缩触发合并裁决（0.85 默认）、fork=复制前缀
  （D23）、黑板 ADD-only 治理（D24）；
- 模型层：槽位封闭枚举与静态映射（D25/D26/D27/D28；compact=answer+low）、
  thinking 语义四档、key 分层读取链；
- 规模与数据生命周期：SQLite 容量恐惧证伪、归档≠删除（D29–D32）、快照单向流
  同步、备份四步法与检测四层；
- 产品信任与成本：同库互检差异化「首个组合」（D33）、信任三表面 UX（D34）、
  无 key 首启（D35）、unknown 成本红线（D36）；
- 解析与供给链安全：解析默认层=纯 Rust 同步子进程（D37）、feed 五件套（D38）、
  cargo 审计基线（D39）；
- 文件监控与 vault 互操作：事件是提示对账是真相（D40）、建边矩阵（D41）、
  source_key=NFC 身份键（D42）；
- 分发与更新：动态薄层更新管线（D43）、macOS 公证 + Windows OV 签名（D44）、
  零遥测（D45）、迁移单事务+强制备份（D46）。

既有决策 0 项被证伪；修订集中在 evidence 排序、压缩触发、记忆分层、会话分叉、
key 读取链、同步边界与解析隔离形态。
