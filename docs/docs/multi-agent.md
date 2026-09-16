---
sidebar_position: 15
---

# 多智能体任务图与可视化控制规划

本文规划 Momotaro 的封闭任务图：仿照 Orchestrator–Subagent 分工，以**任务图（Task Graph）**为一等结构。它与 [Architecture](architecture.md) 兼容——每个智能体动作仍然是一个可审计的 Run，而不是游离于 Run 模型之外的自由循环。

**这不是 1.0 阻塞项。** 主线是 Rust 核心 → CLI → 桌面书桌 → 移动伴侣。任务图从 v0.9 起作为可选加速层。结构埋点（可空 `graph_id` / `node_id`）可以更早出现，但不要为此先写 Graph Console 或 HTTP API。

产品定位仍是**给人用的科研工作台**，不是全能科研 Agent。图帮人把「检索 → 确认入库 → 汇总」跑完；人决定是否采纳、是否写成批注、是否 pin spark。

## 1. 定位与边界

### 1.1 与「明确拒绝的设计」的关系

Architecture 第 21 节拒绝的是「通用多 Agent 框架」。本规划引入的是**封闭角色、确定性调度**的内部编排层：

1. Agent 角色是固定枚举，不是插件，不接受用户自定义 Agent。
2. 调度器是确定性代码（拓扑排序 + 结算），LLM 只在受约束的 Planner 阶段出现。
3. 图在 accepted 时冻结；运行中不允许动态自我扩图，重规划 = 追加新图修订。
4. 每个节点执行 = 一个 Run：冻结 RunSpec、durable events、有界上下文。

因此：`ask` 的单轮 pipeline 是任务图的退化形式（单节点图）。多智能体不是推翻 0.x 设计，而是它的自然延伸。

### 1.2 映射

| 概念 | Momotaro 对应物 |
|---|---|
| 自然语言目标 | `graph request`：mode + query + 策略 |
| 计划评审 | `GraphPlan`：冻结的节点 DAG，accept 前可审可改 |
| Orchestrator | `Planner`（出图）+ `GraphRunner`（调度、结算、预算） |
| 专家团队 | 固定角色：Researcher / Scholar / Verifier / Composer（Solver 后置） |
| Subagent 并行 | 就绪节点并行，逐节点 durable settlement |
| 自验证 | Verifier 节点（确定性，不消耗 token） |
| Task Report | `Answer` + 每节点 run trace 汇成的图级报告 |
| 进度可视化 | 桌面图画布（后置）+ CLI `graph` |

## 2. 角色分工

初始角色集合封闭（新增角色必须走 schema 版本升级，不做运行时注册）：

| 角色 | 职责 | 工具集 | LLM |
|---|---|---|---|
| `planner` | 把请求编译为任务图 | 无（读 policy/profile） | v1 纯模板；v2 受约束 DSL |
| `researcher` | 本地检索与证据整理 | `retrieve` `kb_search` `kb_read` | 可选（查询扩展） |
| `scholar` | 论文检索与入库 | `paper_search` `paper_add`（带 gate） | 否 |
| `verifier` | 引用验证 | 无（确定性函数） | **否，永远不消耗 token** |
| `composer` | 汇总证据生成带引用回答 | 无（只读 run_context_items） | 是，单 provider turn |

`solver` 与 CAS 一起后置，不进 1.0。

角色边界：

1. 每个节点冻结自己的 RunSpec。
2. 角色之间不互相对话。交换只通过持久化工件：`retrieval_hits`、`run_context_items`、`answers`、`verifications`。SQLite 就是黑板。
3. `verifier` 不是 LLM。引用 `passed` 只来自确定性校验。
4. Agent 产出仍是 `origin_class=agent, status=proposed`（含 spark）。
5. LLM 不能开启 toolset 之外的能力，不能修改 plan、policy、budget，不能把网页升级为 owner 事实。
6. **权限三不变量**（业界标准语言，2026-09-15 会话管理调查）：子 Run 的工具/数据访问 ⊆ 父 Run 声明；任何 agent 消息不得充当权限批准；被拒动作不得经第三者中转。嵌套深度恒为 1——图 accepted 即冻结，需要两层就画进图里。

## 3. 顶层模型

```text
GraphRequest (mode + query + policy)
      |
      v
Planner -> GraphPlan (DAG, frozen at accept, plan_hash)
      |
      v
GraphRunner -> 就绪集调度 -> 每节点一个 Run(parent_run_id, graph_id, node_id)
      |                         每节点: 检索/工具 -> Context -> 0..1 provider turn
      |                         -> durable settlement -> 释放后继节点
      v
Gates (awaiting_approval) / Retry / Budget
      |
      v
Graph Report = Answer + 节点 traces + verification 汇总
```

1. `graph 1 - N node`，`node 1 - N run`（attempt 递增）。
2. 单节点图与 v0.3 的 `ask` 行为一致。
3. 图结构是 canonical；布局坐标是 derived。

## 4. 数据模型扩展

全部为新增表与 `runs` 上已预留的可空列，不改既有 canonical 表语义。

```sql
CREATE TABLE graphs (
  graph_id TEXT PRIMARY KEY,
  parent_graph_id TEXT,
  session_id TEXT,
  project_id TEXT,
  root_mode TEXT NOT NULL,
  request_json TEXT NOT NULL,
  plan_hash TEXT NOT NULL,
  plan_json TEXT NOT NULL,
  status TEXT NOT NULL,            -- draft | accepted | running | finished | failed | cancelled
  budget_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);

CREATE TABLE graph_nodes (
  graph_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,              -- retrieve | search_papers | ingest_paper | verify_citations | compose | gate
  agent_role TEXT NOT NULL,
  depends_on_json TEXT NOT NULL,
  requires_approval INTEGER NOT NULL DEFAULT 0,
  budget_json TEXT NOT NULL,
  status TEXT NOT NULL,
  omitted_reason TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (graph_id, node_id)
);
```

Derived：

```sql
CREATE TABLE graph_layout (
  graph_id TEXT NOT NULL,
  layout_version TEXT NOT NULL,
  node_id TEXT NOT NULL,
  x REAL NOT NULL,
  y REAL NOT NULL,
  PRIMARY KEY (graph_id, node_id)
);
```

事件词汇（append-only）：

```text
graph.accepted
graph.started
graph.finished
graph.failed
graph.cancelled
graph.replanned
node.ready
node.started
node.finished
node.failed
node.skipped
node.cancelled
gate.raised
gate.approved
gate.rejected
budget.exhausted
```

重建索引永远不触碰图与事件。

## 5. 契约类型

`momotaro-contracts` 新增纯类型，无 IO：

```text
AgentRole = planner | researcher | scholar | verifier | composer
NodeBudget { max_tokens, timeout_ms, on_over_budget: fail | skip }
TaskNode { node_id, kind, agent_role, depends_on, args, requires_approval, budget }
GraphPlan { plan_id, root_mode, request, nodes, graph_budget, policy_version, index_revision }
```

校验（纯函数，不通过则 `plan_invalid`，绝不带病执行）：

1. DAG 无环；`depends_on` 引用存在。
2. `kind` 与 `agent_role` 在固定兼容表内。
3. 节点 toolset ⊆ 当前 Profile。
4. 存在唯一汇节点（compose 或终态 verify）。

### 5.1 三个提前埋点（v0.3 起生效，免以后迁移；2026-09-15 调查）

即使 v0.9 才实现图，以下字段/语义现在埋：

1. **工件决策元数据**：`answers` / `run_context_items` 加
   `decision_rationale`（≤50 token）/ `assumptions` / `supersedes`——把
   「动作携带隐式决策」显式化（Cognition 实践），下游节点读工件即可，无需
   重放轨迹；每节点成本几十 token，远低于共享完整轨迹（多智能体 ≈15× chat
   token，Anthropic 实测）。
2. **图级 epoch**：工件行携带写入时 epoch；重规划 epoch+1，旧 epoch 工件
   默认不可读——陈旧性语义免迁移（与 Context Epoch 同构）。
3. **budget_json 三层缺省**：`input_cap / explore_cap / output_cap`，output
   缺省 2000 token（subagent 蒸馏返回 1,000-2,000 token 的工业锚点）；图级
   预算 = 等价单 ask 的 4-15 倍，向用户显式报价后才 accept。
4. **黑板工件治理（D24）**：工件带 `superseded_at` 双时间戳（支持「按时间点
   读黑板」只读查询）；agent 角色对共享工件 **ADD-only**（新行 + supersedes
   指针，不可改删他人工件，清理属于人的控制面）；隔离级别明文——会话间只
   共享已提交工件，私有上下文与中间 token 永不入共享层。共享的是「携带决策
   元数据的蒸馏物 + 可回溯指针」，轨迹本体不共享（AgentDiet：删 40-60% 轨迹
   性能不变）。runs 的 `result_digest` / `failure_note` / `deadline_at` 三列
   同期生效（失败必标注部分产出，不静默吞）。

### 5.2 交接包 handoff.v1（v0.8，fork 与跨模型续写共用）

固定 schema 的交接工件（「交接本身是一等失败源」，The Handoff Tax）：

```text
handoff.v1 { decision_state:   已定决策
             open_items:       未决事项
             artifact_pointers: 黑板工件引用（run_id / chunk_hash）
             caveats:          交接方声明的坑 }
```

同模型续写可只带指针；跨模型（如换更强模型重开会话）必须带完整交接包。
工件级的 `decision_rationale` / `assumptions` / `supersedes` 是其节点级子集，
方向一致。1.x 若做任务 chip（用户点击才派生会话）与 `scheduled_jobs`
（prompt 快照自包含、绑定图模板版本号），派生权始终在人。

## 6. 执行模型

`momotaro-graph` 是确定性 crate，不膨胀 `momotaro-run` 的 ask 路径。

1. 拓扑调度：入度为零且依赖已 settlement 的节点进入就绪集。先串行，再按就绪集并行。
2. Durable settlement：节点结果落库后才 `node.finished` 并释放后继。崩溃恢复不重放已结算节点。
3. 每节点至多一次 provider turn；工具在 LLM 之前由代码调用。
4. 节点 failed → 后继 `skipped`（`omitted_reason=dependency_failed`）。可选节点可 `on_over_budget=skip`。
5. 重试只新建 Run（attempt+1）。只允许在可观察输出前自动 retry。
6. 取消映射到 `run.cancelled`，终态必达。
7. 预算按节点分配；估算与实际 usage 分开；价格未知显示 unknown。

## 7. 控制面

| 控制 | 语义 | 约束 |
|---|---|---|
| 计划评审 | accept 前查看/修改 GraphPlan | accept 后 plan_hash 冻结 |
| Gate 审批 | `awaiting_approval` | 如下载论文、任何写操作 |
| 取消 | 取消节点或整图 | 只有 runner 改状态，UI 只发命令 |
| 重试 | 同节点新 Run | `replay=never` 必须人工重试 |
| 重规划 | 新图，`parent_graph_id` 指向旧图 | 旧图永久保留 |
| 预算调整 | 仅 draft | accepted 后不可改 |

## 8. 可视化

### 8.1 CLI（有图之后即可）

```bash
momotaro graph <graph_id>
momotaro graph <graph_id> --json
momotaro graph <graph_id> --mermaid
momotaro graphs
```

### 8.2 桌面图画布（后置，不是独立 Web 产品）

宿主是 `apps/desktop`，不是单独的产品 Web，也不是营销站。

1. DAG 画布（布局写入 `graph_layout`，derived）。
2. 节点卡片：角色、状态、耗时、token、attempt；gate 内联审批。
3. 实时更新走 Tauri event，不要先做 FastAPI SSE。
4. 节点详情复用 Trace Viewer。
5. UI 不直连 SQLite，不拼 prompt，不执行业务规则。

若以后需要浏览器里看图，才加 `momotaro serve`（`127.0.0.1`）。那是投影，不是第二个业务层。**0.x / 1.0 默认不做这个 HTTP 面。**

## 9. 分阶段路线图

与 Architecture §20 对齐。v0.1–v0.8 不因本规划推迟。

| 阶段 | 版本 | 内容 | DoD |
|---|---|---|---|
| M0 结构埋点 | v0.1–v0.3 | `runs.graph_id/node_id/attempt` 可空；可选建 `graphs` 表 | 单节点 ask 行为不变 |
| M1 确定性 DAG | v0.9 | `research`：search → gate(add) → ingest → retrieve → compose；Planner v1 模板 | 无 LLM 也能跑完 DAG；gate 拒绝无半截 source |
| M2 Runner | v0.9 | 就绪集、预算、失败传播、重试 | fake-LLM 图测试绿；崩溃不重放已结算节点 |
| M3 桌面只读图 | v0.9+ / 1.x | 画布 + 节点 trace | 打开画布零写操作 |
| M4 控制 | 1.x | gate / 取消 / 重试 / 重规划 UI | 非法 plan 100% `plan_invalid` |
| M5 角色贯通 | 1.x | Researcher/Scholar/Composer；图级报告 | 找论文→审批入库→带引用综述 |

`solver`、LLM Planner v2、独立 HTTP Console 更后。

## 10. 与既有架构分析的对账

| 来源 | 采纳点 | 落点 |
|---|---|---|
| opencode | provider turn 显式；可观察输出前才 retry | §6.3 / §6.5 |
| lobehub | 证据是关系数据；上下文分阶段 | 角色间走表；节点内沿用 `build_context` |
| Lody | 接受时冻结配置；不自我扩界 | plan_hash；toolset 随 RunSpec |
| pi | tool content/details；replay；append-only | `replay=never` 强制 gate |
| crush | RunID 终态；session/run 分层 | graph/node/attempt |
| codex | canonical/derived；protocol 纯类型；core 克制 | 图 canonical、布局 derived；runner 独立 crate |

2026-09-15 上下文调查的加固（报告 06）：封闭任务图设计逐条命中 MAST
（NeurIPS 2025，596 引）三大失败类的防御——图冻结防规格漂移、SQLite 黑板
防 agent 间失配、verifier 零 LLM 防验证失败（Co-Failure 定理：确定性代码
不在任何模型的 co-failure 空间内，是突破 ensemble 上限的唯一成分）。「共享
完整轨迹」与「只共享工件」的张力用工件决策元数据化解（§5.1）。多智能体
≈15× chat token 的成本预期写进图级预算报价。黑板式优于消息式有单点实证
（bMAS +13-57%）+ 工业同向，但无 RCT——保持黑板路线不变。

2026-09-15 会话管理调查的加固：黑板路线进一步升级为 50 年谱系
（Hearsay-II/BB1）+ 2026 多点同向（PatchBoard 校验式状态变更、DeLM 共享已
验证上下文、MetaGPT 结构化工件）；跨会话调用不需要新实体——图节点派生 Run
即 ephemeral subagent 的正确同构（Claude Code/Manus/LangGraph 同指）。
Nature MI 2026（能力足够的单 agent 可反超 MAS，并行团队可损害串行规划）
支持任务图保持「可选加速层」定位不变。

## 11. 明确不做

继承 Architecture 第 21 节，另加：

1. 不做通用多 Agent 框架、用户自定义 Agent。
2. 不做 Agent 间自由消息总线。
3. 不做节点内不受限的 agent loop。
4. 不做运行中无界扩图。
5. 不做云端 Console；不做 1.0 之前的 `apps/api`。
6. 图画布不成为第二个业务层。
7. 不把任务图当成「代替人做完整个科研流程」的产品。

## 12. 测试

1. Plan 校验：环、悬空依赖、非法 kind/role、toolset 越界。
2. fake LLM 图集成：fixture vault 上 research 全图。
3. 由 run_events + graph events 重建时间线。
4. 崩溃恢复不重复 `replay=never` 副作用。

## 13. 决策记录草案

- **G1**：图与节点是 canonical；布局是 derived。
- **G2**：角色封闭枚举；编排是确定性 crate，不是通用框架。
- **G3**：图画布属于桌面控制面，1.0 可不做；不做云端 Web 产品。
- **G4**：Verifier 永远是确定性代码。
- **G5**：重规划是追加新图，与 source revision 同构。
- **G6**：任务图服务于人的书桌，不把 Project/Annotation/Spark 的所有权交给 Agent。
