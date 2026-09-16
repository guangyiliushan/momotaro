---
sidebar_position: 6
---

# Light-skills 融合边界

## 结论先行

[`Light-skills`](https://github.com/Light0305/Light-skills) 是一套面向科研、竞赛与创新项目的
**Python + Markdown 外部技能包**，不是一个 Rust 库，也不是 Momotaro 的运行时组件。

它不能、也不应该被直接塞进 Momotaro 核心：

1. Momotaro 核心语言是 Rust，明确拒绝 Python 运行时和通用 plugin loader。
2. Momotaro 的 `ask` / `search` 是有限生命周期的 pipeline，不是自由 tool loop。
3. Light-skills 描述的是“调研、实验、论文、投稿”的完整科研 Agent 流程，而
   Momotaro 定位是“人做判断，Agent 打下手”的本地知识工作台。

因此，融合方式是两条不混在一起的线：

- **外部 Agent 技能线**：把 Light-skills 当作开发期和用户研究流程的辅助能力，
  放在 `.agents/skills` 或仓库外的技能目录里，供 Codex/Claude Code/OpenCode 调用，
  不进入 Cargo 依赖。
- **核心契约参考线**：把 Light-skills 里对“引用验证、来源状态、证据边界、覆盖
  诚实性”的纪律，吸收为 Momotaro `momotaro-contracts` / `momotaro-verify` /
  `momotaro-papers` 的语义，而不是吸收其 Python 引擎。

## Light-skills 是什么

它把科研主线拆成可审计阶段，共 23 个 skill：

| 模块 | 代表 skill | 与 Momotaro 的关系 |
|---|---|---|
| 总控与连续性 | `light-orchestrator`、`light-memory-pm`、`light-file-reading`、`light-project-structure` | 边界参考；不能替代 `Project` / `Run` / `history_items` |
| 想法与文献 | `light-literature-search`、`light-idea-generation`、`light-idea-critique`、`light-research-plan` | 前两者有契约重叠；后两者属于未来产品灵感 |
| 数据与实验 | `light-data-engineering`、`light-experiment-coding`、`light-result-analysis` | 0.x 不做，仅留方向 |
| 论文交付 | `light-paper-writing`、`light-citation`、`light-consistency`、`light-typesetting`、`light-venue-matching`、`light-review-rebuttal` | `citation` / `consistency` 可进入验证契约；写作和排版非 0.x |
| 图表与展示 | `light-figure`、`light-frontend-design`、`light-system-design` | 产品 UI 自行实现；不引入其 Python/R 管线 |
| 诚信与转化 | `light-research-ethics`、`light-patent-disclosure`、`light-software-copyright` | 非 0.x |

Light-skills 的纪律是 ACT / ASK / NEVER：确定性工作自己做，关键决策问用户，
不臆造事实、不假装读全、不让模型自己判定自己的引用或新颖性。这套纪律与
Momotaro 的原则高度一致，但“技能执行器”部分不是 Momotaro 核心。

## 应该借用的契约概念

### 1. 引用验证

`light-citation` 的核心状态机可以直接映射到 Momotaro 的 `verifications`：

| Light-skills 状态 | Momotaro 语义 |
|---|---|
| `CONFIRMED` | citation 通过确定性校验 |
| `CONFIRMED-MISSING` | 引用事实不存在，`failed` |
| `UNAVAILABLE` | 上游或网络不可用，`unsupported` / `skipped`，不能当作空结果 |
| `UNRESOLVED` | 证据不完整，不能标 `passed` |

Momotaro 的引用必须绑定
`source_key + revision_hash + chunk_id + chunk_hash + locator`。Light-skills 的
“不能把网络失败当成文献不存在”“不能把元数据确认当成 claim support”也直接适用于
Momotaro 的 citation verification。

### 2. 来源适配器状态

`light-literature-search` 和 `light-citation` 都强调真实 HTTP 状态和覆盖度。
Momotaro 已规定 paper/feed source adapter 只返回：

```text
success | no_results | upstream_error | parse_error
```

这条已经覆盖了 Light-skills 中“没结果”和“源不可用”是两个事实的纪律，不需要再引入
Python 的 `domain_map.py` / `search_normalize.py`。

### 3. 文件理解覆盖

`light-file-reading` 的“输入分诊、结构导航、覆盖缺口、绝不把抽取当理解”可以指导
Momotaro 后续的 PDF/Markdown ingest：

- `Source -> Revision -> Chunk` 是 canonical 事实；
- 抽取失败、扫描页、公式丢失要显式记录，不能把部分提取当成完整索引；
- 未读/不可读范围必须写进 ingest event，而不是静默跳过。

这块对应 Momotaro 的 `source_revisions` / `chunks` 和未来 ingest event，不是
`light-file-reading` 的 Python 脚本。

### 4. 项目记忆与人的工件

`light-memory-pm` 的 `.light/` 台账不能替换 Momotaro 的 `projects` /
`annotations` / `history_items`，但其中“项目状态只追加、外部可变事实带快照、
交接前机器自检”的纪律，与 Momotaro “History 不重写、AI 产出永远是 proposed”
一致。若未来做跨会话研究记忆，应先在 Rust 数据模型上做，不把 `.light/passport.yaml`
当运行时事实源。

## 0.x 不引入的内容

以下 Light-skills 能力是完整科研 Agent 的工作流，Momotaro 0.x 明确不做：

- 自动生成 idea、自动判断创新性；
- 自动写论文、自动生成图表；
- LaTeX / R / PDF 编译全流程；
- 期刊匹配、审稿回复、专利交底、软著材料；
- `light-orchestrator` 的 DAG / passport 自由工作流；
- 多智能体角色、通用工具注册表。

其中“自动写论文、自动出图”与 Momotaro “人是作者，Agent 是工人”直接冲突；
`Spark` 是启发卡，不是论文生成器。

## 推荐落地路径

### 阶段 0：外部技能，不碰 core

在 Momotaro 项目开发期间，可以把 Light-skills 安装到
`momotaro/.agents/skills`，仅用于：

- 调研领域文献；
- 审查论文引用；
- 规划研究问题；
- 写会议/论文材料；
- 做设计决策前的证据梳理。

这个目录只影响 agent harness，不进入 `Cargo.toml`、不进入运行时。仓库内不要复制
全部 23 个 Python skill；需要哪个装哪个，或直接引用外部仓库目录。

### 阶段 1：核心契约自然吸收

当实现 `momotaro-papers`、`momotaro-verify`、`momotaro-ingest` 时，按 Momotaro
自己的数据模型实现：

1. source adapter 四态；
2. citation 验证状态机；
3. ingest coverage / failure event；
4. Spark 默认 `proposed`，空 citations 为 `unverified`。

这些字段在 [Reference](reference.md) 和 [Architecture](architecture.md) 中已有
对应语义，不需要新增 Python sidecar。

### 阶段 2：未来产品能力，原生 Rust

若以后确实要做更完整的科研流程，应作为 Momotaro 的原生 `Run` mode 或独立 CLI
命令逐步加入，而不是引入 Light-skills 的 Python 引擎。优先级仍应遵循现有 roadmap：
先稳定 citation、paper、feed、Project/Annotation，再做可选任务图。

## 升级原则

- 外部技能升级：更新 `.agents/skills` 里的 skill，不影响 Rust core。
- 契约升级：只改 `momotaro-contracts` 和对应测试，不复制 Python 逻辑。
- 不把 Light-skills 的 `.light/`、`passport.yaml`、findings JSON 当作 Momotaro
  的 canonical 数据；如果将来需要，先把它们的语义映射为 Rust 类型。

Python 在项目中的完整定位（开发期工具、类型投影、后置 CAS 接缝）见
[Python 与 Rust 协作边界](python-rust.md)。

一句话：**借纪律，不借运行时；借契约，不借引擎。**
