---
sidebar_position: 2
---

# Momotaro 架构方案与版本目标

## 1. 定位

Momotaro 是一个**本地优先、给人用的科研工作台**。它把本地笔记、论文和后续网页资料统一成可检索、可引用、可验证的知识网。人做判断、批注、选题和写作；Agent 检索、对照、给启发。

版本目标：

1. **`0.x.y-beta`**：以 CLI 验证核心引擎——索引、检索、问答、论文、项目/批注/启发、trace 和契约稳定性。
2. **`1.0.0`**：跨平台正式软件，主界面是 Tauri 2 桌面窗口；CLI 是高级用户和调试入口，不是唯一使用方式。移动端是后置伴侣，不是 1.0 阻塞项。

一句话定位：

> Deterministic first. AI where it matters. Every answer traceable.
> 人是作者。Agent 是工人。

系统不是通用 Agent 框架，也不是全能科研 Agent，也不是聊天产品。核心资产是：

1. 统一来源模型：笔记、论文、后续网页都变成 `Source -> Revision -> Chunk`。笔记与论文**同库互检**是现有系统都没有的差异化资产——一个查询在同一 BM25 索引上返回混合 `origin_class` 的命中并按 origin 加权（v0.3）；图 1-hop「论文命中带出相关笔记」等 `graph_links`（v0.5）落地后才进定位（[ADR 0004](adr/same-library-retrieval-is-mixed-bm25.md)）。调查过的 PaperQA2 / OpenScholar 等系统只索引论文。
2. 人的书桌：`Project`、`Annotation`、`Spark`。
3. 确定性优先：检索、分块、引用校验尽量不消耗 LLM token。
4. 可审计执行：每次 `ask` / `search` / `spark` 都能回到当时的 query、证据、prompt、模型和验证结果。
5. 可升级边界：CLI、桌面、后置移动端复用同一套 Rust 服务函数。

### 1.1 和全能科研 Agent 的区别

| | 全能科研 Agent | Momotaro |
|---|---|---|
| 主角 | Agent 把调研→实验→论文跑完 | 人做判断；Agent 打下手 |
| 成功标准 | 任务闭环、自动报告 | 人能追溯证据、留下批注、把启发收进项目 |
| 默认动作 | 自己搜、自己下、自己写、自己扩图 | 先检索/展示；写入、下载、入库要人点头 |
| 知识 | 模型说了就算 | `owner` 笔记/批注才是事实；AI 产出永远是 `proposed` |
| 工作单元 | 自由 tool loop / 通用多智能体 | `Project` 是书桌；`Run` 是一次可审计劳动 |
| 表面 | 聊天窗口 + 工具目录 | 桌面/移动工作台 + CLI |
| 不做 | — | 代替做实验、代替写论文、无人值守发表、把网页升级成事实 |

Agent 可以帮你读、搜、对照、给 spark；不能替你认领一个科学结论。

## 2. 范围

### MVP / 0.x 必须做

1. 本地 Markdown / Obsidian 笔记索引。
2. Tantivy BM25 检索。
3. 带引用的 `ask`（单次 provider turn）。
4. 基于规则优先的 context pipeline。
5. `trace <run_id>` 完整回放。
6. arXiv 论文检索与入库（有确认门）。
7. Feed 轮询（RSS / Atom / JSON Feed）与 OPML 导入。
8. Project / Annotation / Spark。

### 0.x 明确不做

1. 产品 Web 后端（浏览器里跑引擎）。营销独立站和文档站不是产品运行时。
2. MCP server / MCP client。
3. 通用多 Agent 框架、用户自定义 Agent、节点内自由 tool loop。
4. 插件系统。
5. Notion / Obsidian 双向同步。
6. 通用多 provider 抽象。
7. 流式 token 持久化。
8. 自动写入用户笔记；Spark 自动变成 owner 事实。
9. 无边界知识图谱自动生成。实体抽取图仅在 schema 限定（方法/数据集/任务/指标）且检索基线证明多跳需求真实后才考虑（见 D19）。
10. WebSub 实时推送（桌面没有稳定公网 callback）。
11. BM42 / SPLADE 作为默认索引。
12. 把「文献调研 → 实验 → 写论文发表」做成一个超级 Run。

桌面 GUI 属于 `1.0.0` 的强制目标，在 0.x 后半段接入，不在 v0.1 阻塞引擎。

## 3. 技术选型

| 领域 | 选择 | 理由 |
|---|---|---|
| 核心语言 | Rust | 本地优先、可审计运行时、多端同一二进制图 |
| JS 应用编排 | pnpm + Turborepo | 官方惯例：`apps/` 应用，`packages/` 库 |
| Rust 编排 | Cargo workspace | Rust 依赖图以 Cargo 为准 |
| 多语言桥 | 可运行面的薄 `package.json` | [Turbo 多语言指南](https://turborepo.dev/docs/guides/multi-language)：对无稳定原生支持的语言，用 scripts 调工具链。**不**把 `experimentalCargoWorkspaces` 当地基 |
| CLI | clap + 终端渲染 | 薄 CLI |
| 桌面 | Tauri 2 + Vite/React | 系统 WebView；Rust Core 管状态和 IO |
| 存储 | SQLite + WAL | 关系事实、事务、单文件 |
| 关键词检索 | Tantivy BM25 | 本地倒排；索引可重建 |
| PDF | 双层解析：**同步子进程（纯 Rust）默认** + 高配外部解析接缝 | 默认层即 D18 版本化子进程接缝（Tauri sidecar），纯 Rust crate、OS 资源帽、纯 Rust 下 abort 类风险进程内不可防——结构性崩溃隔离从第一版就有（D37） |
| LLM API | OpenAI-compatible endpoint | MVP 只支持一种协议 |
| 测试 | cargo test + fake LLM | 先验证行为，不依赖真实模型 |
| 数学验证 | 后置接缝 | 1.0 先做 citation；CAS 不拖 Python sidecar 进核心 |

不引入 LangChain、LlamaIndex、向量数据库服务、工作流引擎、通用 plugin loader、LangGraph、PySide6、Python 核心运行时。

Python 只允许出现在两个非运行时位置——`tools/` 下的开发期评测/迁移脚本与
`.agents/skills` 下的外部 agent 技能——细则见
[Python 与 Rust 协作边界](python-rust.md)。产品运行时只有 Rust。

## 4. 顶层架构

```text
Notes / Papers / Feeds
      |
      v
Source Revisions
      |
      v
Chunks / Tantivy / Graph / later Vector
      |
      v
Retrieval Hits
      |
      v
Context Fragments + Token Budget
      |
      v
LLM Provider Turn
      |
      v
Citation Verification
      |
      v
Answer + Run Trace
      |
      +--> Annotation (human)
      +--> Spark (proposed, optional pin)
      +--> Project (human desk)
```

## 5. 模块边界

### 5.1 仓库布局

按 Turborepo 惯例：可运行程序进 `apps/`，JS 库进 `packages/`，Rust 库进 `crates/`。Turbo 不支持嵌套 package，因此 Tauri 的 `src-tauri` **不得**再放 `package.json`。

```text
momotaro/
  package.json
  pnpm-workspace.yaml          # apps/* , packages/* , docs
  turbo.json                   # 先不开 experimentalCargoWorkspaces
  Cargo.toml                   # virtual workspace
  Cargo.lock
  rust-toolchain.toml
  apps/
    cli/                       # @momotaro/cli：shim + Cargo bin
      package.json
      Cargo.toml
      src/main.rs
    desktop/                   # @momotaro/desktop：产品 UI
      package.json
      src/                     # React
      src-tauri/               # 仅 Cargo member
        Cargo.toml
        src/lib.rs
    mobile/                    # 后置，与 desktop 同构
    www/                       # 后置营销站，不跑引擎
  packages/
    ui/
    app-ui/
    ipc/
  crates/
    momotaro-contracts/
    momotaro-store/
    momotaro-ingest/
    momotaro-retrieve/
    momotaro-feeds/
    momotaro-papers/
    momotaro-context/
    momotaro-llm/
    momotaro-tools/
    momotaro-verify/
    momotaro-run/
  docs/                        # 本站；GitHub Pages
```

根 `Cargo.toml` members：

```toml
[workspace]
resolver = "2"
members = [
  "crates/*",
  "apps/cli",
  "apps/desktop/src-tauri",
]
```

`pnpm-workspace.yaml`：

```yaml
packages:
  - "apps/*"
  - "packages/*"
  - "docs"
```

启动：

```bash
pnpm turbo run dev --filter=@momotaro/cli
pnpm turbo run dev --filter=@momotaro/desktop
cargo test --workspace
```

现有脚手架里的 `apps/web` 并入 `apps/desktop` 的前端，不保留「能跑后端的产品 Web」。

### 5.2 Crate 依赖方向

```text
momotaro-contracts          纯类型，无 IO
        ^
momotaro-store              SQLite schema / migration / 事务
        ^
momotaro-ingest / retrieve / feeds / papers
        ^
momotaro-context / llm / tools / verify
        ^
momotaro-run                search / ask / ingest / trace / spark
        ^
apps/cli  |  apps/desktop/src-tauri  |  后置 apps/mobile
```

硬性规则：

1. `momotaro-contracts` 不 import rusqlite、CLI、LLM、Tauri。
2. `apps/cli` 不写业务逻辑。
3. `momotaro-llm` 不知道 SQLite、vault、citation。
4. `momotaro-retrieve` 不渲染 prompt。
5. `momotaro-context` 不访问网络。
6. `momotaro-feeds` 只产生 `FeedItem` / `PaperRecord` 候选，不写 library。
7. CLI 参数对象不进入核心层。
8. 桌面 / 移动 / 未来 `serve` 只能调用 `momotaro-run` 的服务函数。
9. Desktop GUI 不直连 SQLite，不组装 prompt，不执行业务规则。
10. 跨 crate 依赖必须在 `Cargo.toml` 显式声明。
11. 换模型不允许偷偷改变 toolset。
12. 缺价格时 stats 显示 `unknown`，不要显示 `0`。

后置 `momotaro-graph` 依赖 `momotaro-run`，不反向依赖 UI。

## 6. 核心原则

### 6.1 确定性优先

能用代码解决的不交给模型：

1. 分词、索引、查询展开：代码。
2. 关键词检索：代码。
3. context budget：代码。
4. citation 校验：代码。
5. 论文 metadata：API。
6. Feed 轮询与去重：代码。

LLM 只处理自然语言理解、综合、解释、查询扩展（显式模式）和 spark 生成（显式触发）。

### 6.2 来源不可变

本地 Markdown 和 PDF 是用户事实源。SQLite 是运行事实库。Tantivy 是可重建索引。

每个来源必须生成：

```text
source_key     = note:<vault-relative NFC path> | arxiv:<id>
revision_hash  = sha256(normalized source bytes)
```

绝对路径不是身份键；DOI 是 metadata，不是键。`local_path` 另存且相对 workspace root（[ADR 0002](adr/source-key-is-scheme-prefixed.md) / [ADR 0024](adr/local-path-is-workspace-relative.md)）。来源更新时新增 revision，不覆盖旧 revision。

### 6.3 引用绑定 revision

一个引用必须包含：

```text
source_key
revision_hash
chunk_id
locator
chunk_hash
```

不允许只引用数据库自增 ID 或文件路径。

### 6.4 Run 是可审计事实

`Run` 不是普通函数调用。它表示一次被系统接受、可恢复、可追踪的知识工作。**请求被接受 ≠ 模型开始执行。**

每个 run 必须记录：

1. 原始 query。
2. 冻结的 RunSpec。
3. 检索命中。
4. 实际进入模型上下文的 fragment。
5. 每次 provider turn。
6. 每次工具调用。
7. 引用验证结果。
8. terminal status。

一次 Provider Turn = 一次 LLM API 调用。MVP 的 `ask` / `spark` 各至多一次。不要做「模型想调 retrieve 就调 retrieve」的泛化循环。

### 6.5 History 不重写

`history_items` 和 `run_events` 只追加，不修改，不删除。压缩时追加 summary 和保留边界。

### 6.6 模型上下文必须有界

每个 `ContextFragment` 必须有稳定 `content_kind`、明确 role、token estimate、来源或 `source_ref=None`、`visibility=provider|trace_only`。

单项目标小于 2,000 token。工具完整结果进 `value`；给模型的是 bounded `model_output`。省略必须显式记录。

### 6.7 人的工件高于模型产出

- Annotation 是人手写，`origin_class=owner`。
- Spark 是 `origin_class=agent`、`status=proposed`，直到被 pin 或写成笔记。
- LLM 不能修改 Profile、RunSpec、index policy，不能开启未冻结的 toolset，不能把网页升级为 owner 事实。

## 7. 配置模型

配置文件使用 `momotaro.toml`。

```toml
[vault]
path = "./notes"
name = "default"

[index]
lexical = "tantivy_bm25"
hybrid = false
cjk_tokenizer = "cjk"
max_chunk_tokens = 512
chunk_overlap_tokens = 64

[model]                             # == [model_slots.answer]（D25：answer 槽别名，单模型用户零负担）
provider = "openai-compatible"
model_id = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env = "MOMOTARO_API_KEY"    # 或 api_key_keychain = "momotaro.answer"（D28 分层读取链）
context_window = 32768
max_output_tokens = 4096
thinking = "medium"                  # 语义四档 off|low|medium|high（D26）；"auto" = preset 默认
supports_tools = false

[model.pricing]                      # 四元组，缺项 unknown（不写 0）
input_per_million = "unknown"
output_per_million = "unknown"
cache_read_per_million = "unknown"
currency = "unknown"

# 可选第二槽（D25 三态：未配置=回退 answer；空串=显式禁用）
# [model_slots.utility]
# model_id = "qwen3-flash"           # 命中内置 preset 时其余字段可省略
# thinking = "low"

# 压缩例外槽位（默认 answer+low，两向可翻转；D25）
# [slots.compact]
# slot = "answer"
# thinking = "low"

[privacy]
prefer_local_endpoint = false        # true 时两槽都要求指向 localhost 端点（local 是配置形态，不是第三槽）

[policy]
allow_network = false
allow_write = false
max_context_tokens = 8192
max_tool_result_tokens = 1200

[papers]
allow_network = false
providers = ["arxiv"]

[feeds]
poll_interval_s = 3600
```

规则：

1. API key 从分层读取链获取（D28 修订）：显式参数（测试）→ 环境变量（CI/headless 首选）→ OS keychain（桌面默认，`keyring` 4.2，per-slot service = `momotaro.<slot>`）→ 配置引用 + 明文警告（最后一级）；不写入明文配置，除非用户显式选择并收到警告。
2. 价格未知时显示 `unknown`，不显示 `0`。
3. `RunSpec` 在 run accepted 时冻结。
4. run 期间不允许修改 model、toolsets、policy 或 index revision。
5. 移动端用更小的 `max_context_tokens`，同一字段，不是另一套模型。
6. `[vault].path` 必须落在 workspace 内（`vault == workspace root` 合法，检查用 canonicalize 双验——[ADR 0025](adr/vault-must-live-inside-the-workspace.md) / D31）；存储的 `local_path` 一律相对 workspace root。

## 8. 数据模型

### 8.1 Canonical data

这些数据是系统事实，不允许因为索引重建而删除。

#### sessions

```sql
CREATE TABLE sessions (
  session_id TEXT PRIMARY KEY,
  parent_session_id TEXT,
  project_id TEXT,                   -- 可空；创建时绑定，此后不可变
  title TEXT NOT NULL,
  title_source TEXT NOT NULL DEFAULT 'derived',  -- derived | user | llm；user 改名后永不被自动标题覆盖；0.x 只写 derived
  profile_id TEXT,                   -- 0.x 不写；无 profiles 表
  first_user_message TEXT,          -- 列表预览与检索喂料
  archived INTEGER NOT NULL DEFAULT 0,
  archived_at INTEGER,
  pinned INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_activity_at INTEGER          -- recency 排序，区别于 created_at
);
```

MVP 的 `ask` 可以自动创建一个 session。换书桌 = 开新 session（[ADR 0003](adr/session-binds-zero-or-one-project.md)）。`parent_session_id` 是谱系字段；v0.8 分叉 = **复制前缀**（D23：子会话复制父会话截至边界的 `history_items`，首条写 fork 元数据；父不可变，两支独立演化）。`tokens_used` / `message_count` 类计数是**派生投影**（`usage_projections`，可从 history_items / llm_turns 重算），不做 canonical 列，防漂移。

#### projects

```sql
CREATE TABLE projects (
  project_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE project_memberships (
  project_id TEXT NOT NULL,
  source_key TEXT NOT NULL,
  added_at INTEGER NOT NULL,
  PRIMARY KEY (project_id, source_key)
);
```

删除项目不删除 source。

#### source_revisions

```sql
CREATE TABLE source_revisions (
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  kind TEXT NOT NULL,              -- note | paper | web
  title TEXT,
  uri TEXT,
  local_path TEXT,
  origin_class TEXT NOT NULL,      -- owner | paper | web | agent | system
  metadata_json TEXT NOT NULL,
  ingested_at INTEGER NOT NULL,
  is_current INTEGER NOT NULL,
  PRIMARY KEY (source_key, revision_hash)
);
```

`origin_class`：

| 值 | 含义 |
|---|---|
| `owner` | 用户手写笔记或批注，可信 |
| `paper` | 论文原文，外部但结构化 |
| `web` | 网页，不可信 |
| `agent` | AI 生成内容，必须标为 proposed |
| `system` | 系统说明 |

#### chunks

```sql
CREATE TABLE chunks (
  chunk_id TEXT PRIMARY KEY,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  heading_path TEXT,
  text TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  token_estimate INTEGER NOT NULL
);
```

`chunks` 是保留型投影：只要有 `answer_citations` / `run_context_items` / retrieval hit 指向该 `(source_key, revision_hash)`，行就留着；同 policy 同 revision 重复 ingest = 幂等替换，换 policy 只对新 ingest 生效。`chunk_id` / `chunk_hash` 是 `(source_key, revision_hash, ordinal, text)` 的纯函数。整表擦除 = 灾难恢复且必须报 drift（[ADR 0022](adr/chunks-are-a-retained-projection.md)）。

#### annotations

```sql
CREATE TABLE annotations (
  annotation_id TEXT PRIMARY KEY,
  project_id TEXT,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  body TEXT NOT NULL,
  spark_id TEXT,
  created_at INTEGER NOT NULL
);
```

批注钉在旧 revision 上。新 revision 出现时，UI 可以提示 stale，不得偷偷改绑。

#### sparks

```sql
CREATE TABLE sparks (
  spark_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  project_id TEXT,
  status TEXT NOT NULL,            -- proposed | pinned | dismissed
  body TEXT NOT NULL,
  citations_json TEXT NOT NULL,
  source_refs_json TEXT,           -- 检索资格用：指向 live chunk 的五元组；可空
  supersedes_spark_id TEXT,        -- 取代链：新卡取代旧卡时指向被取代者（D24）
  created_at INTEGER NOT NULL
);
```

未 pin 的 spark 不进入检索。空 citations 视为 unverified。**pin 不需要引用，检索资格才需要**：检索资格 = `pinned` 且至少一条指向 live chunk 的五元组；只有 `run_id` / `history_id` 的 pin 不算 provenance。未 pin 或无五元组的 pinned spark 都不进索引（[ADR 0013](adr/pin-without-five-tuple-is-not-indexed.md) / [ADR 0018](adr/spark-actions-are-pin-and-dismiss.md)）。pin 是人审动作，不改变信任构成（2026-09-15 调查：记忆投毒 OVERWRITE/ENTRENCH；Mem0 已回撤到 ADD-only）。

**取代链语义（D24）**：取代 = 新行 + `supersedes_spark_id` 指针，被取代行保留
（可加 `superseded_at` 投影）；检索命中已被取代的工件时自动改投链头或降权，
provenance 面板显示「该断言已被 X 取代」。

**记忆检索的 boost 阶梯**（per-document origin 类权重 —— 是**每文档的类权重**，不是字段权重：类相同的文档在任何查询下都按同一比例缩放（[ADR 0004](adr/same-library-retrieval-is-mixed-bm25.md)）；**乘性**，0.x 起生效；实现见 `crates/momotaro-retrieve/src/boost.rs`）：

```text
owner note chunk                  ×2.0
paper                             ×1.0
web（含抓取的网页正文）             ×0.6
agent / system                    不进索引（查询侧也不可检索）
pinned spark（≥1 五元组）          ×1.5 —— v0.5，尚未实现
```

annotation 不进这张表（[ADR 0016](adr/annotations-are-not-retrieval-hits.md)）。`wiki_pages` 不在 0.x/1.0 索引与 boost 阶梯里。

boost 是检索相关性先验，**不是信任级显示**——固化/蒸馏产物的信任级显示不得
高于其 evidence_refs 中最低源档（provenance 洗白防御，D24）；pin（人审）授予
检索资格，不改变信任构成。

不建独立 memory 表：`history_items`（episodic 证据层，只追加）/ `sparks`（断言
层）/ `annotations` + Profile（人的事实层）三表分工已覆盖跨会话记忆语义；原始
历史本身就是强基线（Letta 纯文件存历史 74.0%，记忆提炼增益很小），跨会话记忆
操作刻意只暴露 append / pin / dismiss / summarize 四个，不提供 LLM 驱动的
update / delete。

#### runs

```sql
CREATE TABLE runs (
  run_id TEXT PRIMARY KEY,
  session_id TEXT,
  parent_run_id TEXT,
  project_id TEXT,
  graph_id TEXT,
  node_id TEXT,
  attempt INTEGER NOT NULL DEFAULT 1,
  mode TEXT NOT NULL,              -- ask | search | paper_add | spark（封闭集）
  query TEXT NOT NULL,
  status TEXT NOT NULL,            -- accepted | running | finished | failed | cancelled
  client_request_id TEXT UNIQUE,
  run_spec_hash TEXT NOT NULL,
  run_spec_json TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  context_window INTEGER,
  max_output_tokens INTEGER,
  result_digest TEXT,               -- 图节点蒸馏结果（v0.9 起；1-2K token 上限）
  failure_note TEXT,                -- 失败 run 的部分产出+标注，不静默吞（图节点共识）
  deadline_at INTEGER,              -- 显式超时（图节点）
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
```

`session_id` / `graph_id` / `node_id` 初期可空。`client_request_id` 负责幂等，`run_id` 负责终态追踪，二者不要混用。`index` / feed poll / annotate 是 store 写与 ingest/feed 事件，不是 Run（[ADR 0017](adr/run-modes-are-a-closed-set.md)）。

#### run_events

```sql
CREATE TABLE run_events (
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  data_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (run_id, seq)
);
```

标准事件见 [Reference](reference.md)。过程性 UI 进度可以不落库；`run.finished` / `run.failed` / `answer.finalized` 必须落库。

#### history_items

```sql
CREATE TABLE history_items (
  history_id TEXT PRIMARY KEY,
  session_id TEXT,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  role TEXT NOT NULL,
  content_kind TEXT NOT NULL,
  body TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  source_ref_json TEXT,
  token_estimate INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  occurred_at INTEGER,             -- 事件真实发生时间（重放/导入时≠created_at）
  UNIQUE (run_id, seq)
);
```

长期对话和 provider context 的事实层。压缩只追加 summary。

#### retrievals / retrieval_hits / run_context_items

```sql
CREATE TABLE retrievals (
  retrieval_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  query TEXT NOT NULL,
  normalized_query TEXT NOT NULL,
  strategy_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE retrieval_hits (
  retrieval_id TEXT NOT NULL,
  rank INTEGER NOT NULL,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  chunk_id TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  score REAL,
  included INTEGER NOT NULL,
  omitted_reason TEXT,
  PRIMARY KEY (retrieval_id, chunk_id)
);

CREATE TABLE run_context_items (
  run_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  fragment_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  role TEXT NOT NULL,
  content_kind TEXT NOT NULL,
  body TEXT NOT NULL,
  source_ref_json TEXT,
  token_estimate INTEGER NOT NULL,
  body_hash TEXT NOT NULL,
  PRIMARY KEY (run_id, ordinal)
);
```

区分三件事：检索到、进入模型上下文、被回答引用。

#### tool_calls / llm_turns / answers / answer_citations / verifications

这些表是 `momotaro-store` 的 canonical 表，和 `runs`、`run_events` 一样不可由索引重建删除。字段是 MVP 的最小闭合集：

```sql
CREATE TABLE tool_calls (
  tool_call_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  name TEXT NOT NULL,
  args_json TEXT NOT NULL,
  value_json TEXT NOT NULL,
  model_output TEXT NOT NULL,
  status TEXT NOT NULL,            -- running | finished | failed
  replay TEXT NOT NULL,            -- safe | never
  tool_version INTEGER NOT NULL,
  started_at INTEGER NOT NULL,
  finished_at INTEGER
);

CREATE TABLE llm_turns (
  turn_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  status TEXT NOT NULL,            -- started | finished | failed
  input_tokens INTEGER,
  output_tokens INTEGER,
  cached_tokens INTEGER,           -- provider 缓存命中的输入 token（0.3 起）
  billable_tokens INTEGER,         -- 实际计费口径（折扣后），未知为 NULL
  slot TEXT,                       -- answer | utility（RunSpec 冻结快照，D25）
  reasoning_tokens INTEGER,        -- 思考 token（按输出价计费口径，D26）
  raw_ref TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);

CREATE TABLE answers (
  answer_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  text TEXT NOT NULL,
  prompt_hash TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE answer_citations (
  answer_id TEXT NOT NULL,
  citation_index INTEGER NOT NULL,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  chunk_id TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  quoted_text TEXT NOT NULL,
  verified INTEGER NOT NULL,
  PRIMARY KEY (answer_id, citation_index)
);

CREATE TABLE verifications (
  verification_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  answer_id TEXT,
  kind TEXT NOT NULL,              -- citation | source_integrity | math
  status TEXT NOT NULL,            -- passed | failed | unsupported | skipped
  details_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
```

`verifications.kind` 初期以 `citation` 和 `source_integrity` 为主；`math` 留枚举。`answers.text` 是最终回答，`answer_citations` 是回答的引用证据，`verifications` 是独立于模型声称的验证结果。

#### feeds

```sql
CREATE TABLE feed_sources (
  source_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,              -- rss | atom | jsonfeed
  url TEXT NOT NULL,
  title TEXT,
  etag TEXT,
  last_modified TEXT,
  hub_url TEXT,                    -- WebSub 接缝，MVP 不订阅
  poll_interval_s INTEGER NOT NULL,
  enabled INTEGER NOT NULL,
  last_status TEXT,
  last_error TEXT
);

CREATE TABLE feed_items (
  item_id TEXT PRIMARY KEY,
  source_id TEXT NOT NULL,
  external_id TEXT NOT NULL,
  title TEXT,
  url TEXT,
  published_at INTEGER,
  paper_key TEXT,
  raw_hash TEXT NOT NULL
);
```

Feed 项不是论文事实。解析出 doi/arxiv 后仍要经过 paper add gate 才能入库。

OPML 不是表：导入时把 `outline[@type=rss]/@xmlUrl` 写成 `feed_sources`。

### 8.2 Derived data

可删除后重建：

```text
tantivy index directory
embeddings
bm42 / splade indexes
graph_links
graph_layout
usage_projections
```

MVP 只实现 Tantivy BM25。中文用 CJK tokenizer，不要把 SQLite FTS5 当主引擎。SQLite 留下做关系事实。

## 9. 契约类型

`momotaro-contracts` 只包含纯类型和纯校验。示意（Rust；语义与下列字段一致）：

```text
SourceRevision
Chunk
Citation
RetrievalHit
ContextFragment
RunSpec
ToolResult { value, model_output, trace_ref, token_estimate, truncated, replay }
Answer
Project
Annotation
Spark { status: proposed | pinned | dismissed }
VaultScope { root, name }
FeedSource
FeedItem
```

`RunSpec.mode`：`ask | search | paper_add | spark`。`solve` 后置。

工具结果：完整领域结果进 `value`；给模型的是 bounded projection。引用和验证永远基于 `value` 或库里的 chunk，不基于模型转述。

## 10. Run 生命周期

### 10.1 ask

```text
1. validate request
2. create run: accepted
3. freeze RunSpec
4. append run.started
5. retrieve()
6. save retrievals + retrieval_hits
7. build_context()
8. save run_context_items + context.built
9. call LLM once
10. save llm_turns
11. validate citations
12. save verifications
13. save answer + history_items
14. append answer.finalized
15. finish run
```

成功定义为：LLM 结束、正文非空、每条引用能解析、hash 匹配、`answer.finalized` 已写。任一项失败则 `run.status = failed`，不得把失败 retry 伪装成成功。

终态必达、崩溃恢复、取消、超时与重试、并发模型等工程纪律见
[Harness 工程](harness.md)。规则要点：terminal 事件与 status 更新同一事务原子落库；
崩溃后清扫悬挂 run 为 `failed(interrupted)`；副作用幂等（`paper add` 以
`arxiv_id + version` 幂等，ingest 以 `source_key + revision_hash` 幂等）；
自动 retry 只发生在可观察输出产生之前。

### 10.2 search

```text
lexical: query -> normalize -> Tantivy BM25 -> ranked hits
expand:  one short LLM rewrite -> N queries -> BM25 each -> RRF
```

默认 lexical，零 token。expand 必须显式。

### 10.3 paper add

```text
resolve id / query
  -> user confirms
  -> fetch metadata and PDF
  -> hash source revision
  -> parse sections
  -> create chunks
  -> rebuild Tantivy
  -> record ingestion events
```

不自动生成论文摘要。摘要必须显式触发。

### 10.4 spark

```text
explicit trigger
  -> retrieve within project or current source
  -> one LLM turn
  -> 3–7 bounded cards with citations
  -> status=proposed
```

Spark 失败不是 ask 失败。未 pin 不进 FTS。

## 11. Retrieval

MVP 检索：

1. **Tantivy BM25**：主检索。
2. **显式 query expand**：一次短 LLM，然后确定性检索。
3. **Link expansion**：第一实现是 **wiki-link 确定性解析**（v0.5，D22）——笔记 `[[双链]]` 在 ingest 时解析入 derived `graph_links`，`link_score` 是 1-hop 小额**乘性**加成（`×(1+δ)`，离线 recall 校准，不进加性项）；LLM 只能建议链接，不能建立链接。引用网络边在 paper add 时从参考文献与 S2AG/OpenAlex 类免费引用 API 确定性抽取。**PPR 定位为「种子发现面板」**（对标 Inciteful），不进 `final_score`——HippoRAG 消融显示 PPR 增益依赖 LLM 抽取图，确定性图上仅存产品级证据。
4. **dense / BM42 / SPLADE**：派生索引，默认关闭。接缝是 `retrievers: [bm25]`。

排序因子：

```text
final_score = bm25_score × class_weight
```

`class_weight` 是乘性因子（Q4 裁决；owner ×2.0 / paper ×1.0 / web ×0.6）。
**加性公式（`bm25 + weight`）已被否决**：同一个常数在不同查询上改变排名的幅度
不同（一次项查询值 +48.7%，六次项只值 +8.1%），乘法因子才是"同一个类在任何
查询下都按同一比例缩放"。（两个百分比出自
`research/2026-09-16-六问决策复核/agents/01-boost实现口径-引擎与文献.md` §3.1 的
**示意模型**——k1=1.2、b=0.75、N=1000、avgdl=100 的自算 BM25，不是本仓基准测试。）
recency 与 link 两项**后置**（v0.5+；link 见 D22，recency 尚无对应决策项）：进入
时同为乘性因子（`link_score` 写作 `×(1+δ)`），不回到加性。

| Source | 是否进索引 |
|---|---|
| `owner` / `paper` / `web` | 进；权重数值**只在上面那张阶梯里**（×2.0 / ×1.0 / ×0.6，逐文档类权重，不分 curated/untrusted） |
| `agent` / `system` | 不进（非用户内容） |
| pinned spark（≥1 五元组） | 不实现（v0.5；阶梯里同款标注） |
| unpinned spark / annotation | 不进 |

## 12. Context Pipeline

`momotaro-context` 暴露纯函数 `build_context(...) -> ContextBuildResult`。

固定阶段：

```text
1. normalize_query
2. load_policy
3. select_history
4. select_evidence
5. render_fragments
6. deduplicate
7. rank
8. enforce_token_budget
9. render_provider_messages
10. validate_citations
```

每个阶段记录 `included` / `omitted` / `omitted_reason` / `token_delta` / `duration_ms`。

领域消息与 provider 消息两段 lowering：trace 保存完整 fragment；provider 只收到 `visibility=provider`。

**定界渲染（render_fragments 阶段，0.x 起生效）**：进入 prompt 的 `web` /
`agent` 来源 chunk 用统一开始/结束定界符包裹并携带来源标签（Spotlighting，零
模型成本）；prompt 措辞区分 privileged（system/policy）与 untrusted（检索
内容）——角色分离是弱保证（指令层级只是缓解），定界渲染 + 确定性校验才是
防线。`origin_class` 从检索命中到 citation 校验再到写入门全程携带，不中途丢弃。

### 12.1 Context Epoch

MVP 中一个 run 就是一个 epoch。run 内不改 system baseline。检索证据不是 system prompt。换模型、换 toolset 或压缩历史时开新 epoch。

### 12.1.1 Working set P1 / P2

旧 PaperSearchAgent 的工作集 P1/P2 不新建成独立表，而是作为 Context Pipeline 的 scope seam：

```text
P1 = 当前 Project 的来源 + 用户显式选择进入本次工作的来源
P2 = 当前 Vault 中其余可检索来源
```

默认 `ask` / `spark` 只使用 P1；`search` 覆盖 P2。未绑定 Project 时 P1 = P2 = 整个 Vault，拆分退化为 no-op；v0.3 的 ask 不因缺 Project 报 `retrieval_empty`（[ADR 0007](adr/unbound-ask-uses-whole-vault.md)）。当 `ask` 指定 `--scope wide` 或 RunSpec 允许扩大范围时，P2 命中仍排在 P1 之后，并受 `max_context_tokens` 约束。这个优先级只影响 `run_context_items`，不改变 `retrieval_hits`。

### 12.2 Token budget

估算 token 做裁剪；provider 实际 usage 做成本。两者分开。`adjusted_total = ceil(raw_total * 1.25)`。价格未知时成本是 `unknown`。

来源：`policy / question / history / evidence / tool_definitions / instructions`。

普通 search：0 LLM。expand search：1 次短 LLM。ask：1 次 provider turn。spark：显式 1 次。

### 12.3 上下文与检索的工程细则

请求确定性、缓存稳定、预算分配顺序、多轮 history 选择、压缩触发、分块策略、
增量索引、检索评测（BM25 基线的验收方式）与演进接缝，见
[上下文与检索工程](context-retrieval.md)。核心约束：检索可以换引擎（BM25 → dense），
只要命中仍带 `source_key + revision_hash + chunk_id + chunk_hash`；上下文可以换策略，
只要 fragment 契约不变。

## 13. Tool Contract

工具分 Spec / Runtime / View 三层。下列是**引擎函数（不是模型工具）**：

| 函数 | 说明 | Replay |
|---|---|---|
| `retrieve` | 本地检索 | safe |
| `kb_search` | 发现来源 | safe |
| `kb_read` | 读取指定 chunk | safe |

0.x Ask 的模型没有工具；`tool_calls` 表等 1.x 多轮压缩或任务图才写（[ADR 0005](adr/ask-has-no-model-tools.md) / [ADR 0010](adr/session-ask-retrieves-fresh.md)）。`paper_search` 先是 CLI/桌面命令，不是模型工具。`calculate` / CAS 后置。

统一入口：`validate_input → check_policy → execute → validate_result`。不做用户自定义 hook，不做 plugin loader。工具描述不能宣传当前 RunSpec 中被禁用的能力。

## 14. Verification

### 14.1 Citation

1. 引用 ID 必须存在于 `run_context_items`。
2. `source_key + revision_hash + chunk_hash` 必须匹配。
3. 引用文本必须能定位到原始 chunk。
4. 编造的引用标记 `citation_invalid`。
5. 引用失败时回答可以输出，但不是 fully verified。

### 14.2 Math

`verifications.kind=math` 留枚举。1.0 不实现 CAS。若以后接入，只有确定性计算可以给 `passed`；LLM 不得宣布数学正确。不要为了对标 SymPy 把 Python 运行时拖进核心。

### 14.3 Claim / Spark

AI 产出默认 `origin_class=agent, status=proposed`。用户确认或 pin 后才进入人的书桌。MVP 不做完整 claim 表。

## 15. Paper 与 Feed

### 学术发现

`paper search`：query → 适配器 → `PaperRecord` → 表格。不调用 LLM。v0.4 只做 arXiv；其它源后加，失败必须隔离并记 source_log（区分无结果与源失败）。

OA 三态：`open | closed | unknown`。有 PDF URL ≠ 一定能下载。

每个论文源适配器只返回四类结果：`success`、`no_results`、`upstream_error`、`parse_error`。失败必须写入 `source_log`，包含 `source_key`、`provider`、`operation`、`outcome`、`retryable`、`message`、`created_at`。UI 和 `paper search` 必须能区分“没有结果”和“这个源暂时不可用”，不能把上游失败渲染成空结果。

### paper add

确认 → 下载 → hash → 分块 → 索引。同一 `arxiv_id + version` 幂等。失败不得留下半初始化 source。内容是 `paper`，不是 `owner`。

### Feed

| 协议 | 职责 | MVP |
|---|---|---|
| RSS / Atom | 可轮询 feed | 做，统一成 `FeedItem` |
| JSON Feed 1.1 | 同样是 feed | 做；`hubs` 只记录 |
| OPML | 订阅列表交换，不是实时协议 | 只 import/export |
| WebSub | 需要公网 callback 与 lease | 不做；`hub_url` 可空 |

默认 `ETag / If-Modified-Since` 轮询。发现结果进候选，入库仍要 gate。

fetch/parse 安全五件套与四态 reason-code 映射（D38）见
[摄取安全与 Vault 互操作](ingest-interop.md)：quick-xml ≥0.41、scheme allowlist、
响应字节硬上限 10–15 MiB、总超时 20–30s、OPML 同管线；限长截断必须记
`response_too_large` 而非 parse_error。

## 16. LLM Provider

`momotaro-llm` 无状态：输入 model / messages / tools / max_tokens；输出 text / usage / finish_reason / raw_ref 或 typed failure。

不知道 vault，不访问 SQLite。只在可观察输出前 retry。不做多 provider registry。

缓存记账（2026-09-15 调查）：`usage` 结构带 `cached_tokens` / `billable_tokens`
（provider 报什么记什么，不报为 NULL）；tools 定义 canonical 序列化（列表排序 +
字段序固定）并纳入 `request_hash`——provider 的缓存键就是 tools JSON 的序列化
顺序。不做语义缓存（三线证伪：vCache「不可部署」、NDSS 投毒、ICML 碰撞劫持），
本地只做 `request_hash` → response 的精确缓存（后置，一张 SQLite 表）。

## 17. 表面：CLI / 桌面 / 移动 / 文档 / www

| 能力 | CLI | Desktop 1.0 | Mobile 后置 |
|---|---|---|---|
| init / doctor / stats | 全 | 设置页 | 有限 |
| index / ingest | 全 | 全 + 进度 | 否 |
| lexical search | 全 | 全 | 全 |
| expand search | 全 | 全 | 可选 |
| ask + citation + trace | 全 | 全，可点原文 | 简化 trace |
| paper search / add | 全 | 全，有 gate | 只收藏候选 |
| feeds + OPML | 全 | 全 | 只读已同步 |
| Project / Annotation | 命令 | 主 UI | 主 UI |
| Spark | 命令 | 卡片流 | 卡片流 |
| 会话 fork | 后置 | 后置 | 后置 |
| 任务图 | mermaid/json | 后置画布 | 只读列表 |
| LaTeX / 生图 | 否 | 更后置 | 否 |

CLI 命令（0.x）：

```bash
momotaro init
momotaro index ./notes
momotaro search "Fourier transform"
momotaro ask "解释谱定理和 PCA 的关系"
momotaro trace <run_id>
momotaro stats
momotaro doctor
momotaro paper search "..."
momotaro paper add arxiv:2401.12345
momotaro feed add <url>
momotaro feed import ./subscriptions.opml
momotaro project new "..."
momotaro annotate ...
momotaro spark --project <id>
```

全局参数：`--json --profile --no-llm --allow-network --allow-write --max-context-tokens --mode lexical|expand`。

桌面 IPC：Tauri command / event，不把本机 HTTP 当核心总线。`momotaro serve` 仅在后置 Graph Console 需要时出现，且绑 `127.0.0.1`。

文档站继续 `docs/` + GitHub Pages。`apps/www` 是后置营销独立站，可用 GSAP；与文档、产品 UI 分开。不把引擎暴露给浏览器。

## 18. Error taxonomy

| Category | Attribution | Retryable |
|---|---|---|
| `config` | user | false |
| `source_missing` | user | false |
| `index_unavailable` | system | true |
| `retrieval_empty` | harness | false |
| `llm_auth` | user | false |
| `llm_rate_limit` | provider | true |
| `llm_context_overflow` | user/provider | maybe |
| `network_disabled` | policy | false |
| `citation_invalid` | harness/model | false |
| `model_probe` | system | true |
| `user_cancelled` | user | false |

可预期错误写入 run_events，带 `category / severity / attribution / retryable / count_as_failure`。

## 19. Testing

```bash
pnpm turbo run test --filter=@momotaro/cli
cargo test --workspace
```

1. Contract：类型不可变；hash 稳定；RunSpec 校验失败有明确错误。
2. Storage：空库 migration；未来 schema version 拒绝打开；重建索引不改 run/history。
3. Retrieval：中英命中；空结果结构化；lexical 路径零 LLM。
4. Ask 集成：fixture vault → ingest → search → fake LLM → citation → trace replay。
5. Spark：未 pin 不进检索；无引用不得装成 verified。

不要一开始跑真实 API。

## 20. Roadmap

主线：**Rust 核心契约 → CLI 验引擎 → 桌面把人的书桌做完整 → 移动做只读伴侣。**

### v0.1 仓库骨架

1. Cargo workspace + 现有 pnpm workspace 并存。
2. `crates/momotaro-contracts` `store` `run`。
3. `apps/cli` shim；`momotaro init / doctor / stats`。
4. 文档与本文件一致（废 Python / PySide6）。
5. `apps/web` 并入 `apps/desktop` 前端，不接引擎。

验收：无 API key；`cargo test --workspace` 过。

### v0.2 本地笔记闭环

`index` / lexical `search` / Tantivy。重建索引不丢 canonical 数据。1,000 个 Markdown 可完成。

一致性协议从第一行代码做对（D40–D42）：事件=hint、重扫走 stat 双验 + racy 内容校验 + 单事务边界、对账四触发；source_key = NFC 规范化（P0 级主键）；wikilink 基础建边（D22/D41）。

### v0.3 可追溯问答

`ask`、`trace`、citation validation、run events、history items。无效引用不是 fully verified。fake LLM 集成测试绿。

槽位语义落地（D25/D26）：`[model]` = answer 槽；RunSpec 冻结 `slot` + `thinking` 语义档快照；`llm_turns` 增 `slot` / `reasoning_tokens` 列。单模型用户行为零变化。

### v0.4 论文与 Feed

arXiv search/add、PDF ingest、RSS/Atom/JSON Feed 轮询、OPML 导入。同一论文版本幂等；下载失败无半截 source；候选 ≠ 库。

PDF 解析按双层策略（2026-09 学术 RAG 调查结论 + D37 安全收紧）：**默认层 = 同步子进程（纯 Rust crate、OS 资源帽、入口硬 cap、来源记账、解析在写事务外）**——D18 接缝从第一版启用；高配层（GROBID 引用对齐 / VLM 版面解析）按需显式运行，永不阻塞默认层。Feed 落地 D38 五件套与四态 reason-code。引用网络元数据在 paper add 时从参考文献与 S2AG/OpenAlex 类免费引用 API 确定性抽取入 `graph_links`，为后置种子发现面板（D22）存料。cargo 供给链审计基线（D39）随 CI 落地。

### v0.5 Project / Annotation

项目 CRUD；来源加入项目；批注钉 revision+locator。删项目不删 source。换版本后旧批注仍打开旧 revision。

零 LLM 治理第一批（2026-09-15 LLM Wiki 调查 P0）：`[[wiki-link]]` 双链确定性解析入 `graph_links`（D22，Link expansion 第一步）；source-coverage 投影（各源被检索/进入上下文/被引用的统计，数据已在 `retrieval_hits`）。建边矩阵全量落地（D41：embed/canvas/frontmatter 链/歧义三级规则/降级十事件）；同步盘预警与平台测试矩阵 11 项进 CI（D42）。

### v0.6 桌面主路径

Tauri 包装产品 UI。库浏览、search、ask、citation 跳转、trace、项目、批注。长任务可取消。三平台可运行。GUI 与 CLI 同一契约。无 API key 时本地 search 仍可用。key 分层读取链落地（D28：keychain 桌面默认）。

信任面落地（D34–D36）：信任徽标内联引用处、hover 五元组→点击跳原文、trace 时间线以验证结果为终点、per-run 成本小字 + unknown 红线、首启无 key 即价值时刻 + key 配置五要素。崩溃 journal 落地（D45 零遥测）。

### v0.7 Spark

显式触发 3–7 张启发卡。pin / dismiss。未 pin 不进检索。token 单独记账。

零 LLM 治理第二批：pinned 工件 **code-lint**（孤儿/断链/无源断言/引用翻转，全部是图与表上的确定性查询，零 LLM）；pinned 引用**翻转率**健康指标（VitaminC：证据微小修订即可翻转主张判定）；Profile 文档化为 agent-facing schema 页。`wiki_pages`（Proposed Wiki 层，D21）评审后 Project 级试点。

utility 槽落地（D25/D27）：内置 curated preset 表（≤10 条，含 thinking 能力矩阵与价格四元组）+ `momotaro doctor --probe` 逐槽探测 + `[slots.compact]` 例外配置 + stats 按 slot 聚合成本。

### v0.8 会话分叉

分叉 = **复制前缀**（D23）：子会话复制父会话截至边界的 `history_items`（INSERT…SELECT），首条写 fork 元数据；父不可变，两支独立演化；回归测试断言子前缀逐条等于父截断前缀。跨会话引用是显式动作。压缩只追加 summary。跨模型续写带 `handoff.v1` 交接包。

### v0.9 封闭任务图（可选，见 [multi-agent](multi-agent.md)）

单节点图埋点 → 确定性 DAG → 只读可视化。不是 1.0 阻塞项。Planner v1 纯模板。

### v0.10 移动伴侣

复用 `packages/app-ui`。读库、search、ask、批注、spark inbox。无 ingest、无 feed 轮询。数据通路 = C2 快照单向流（D31）：桌面定时 `VACUUM INTO` 快照 → 用户同步目录 → 移动端只读打开；口径「小时级滞后」，不承诺准实时。

### v1.0.0

Windows / macOS / Linux 桌面安装包 + CLI。文档仍 GitHub Pages。营销站、产品 Web、MCP、BM42/SPLADE、WebSub、演进树、实验流水线、LaTeX 编辑器：**不进 1.0**。

分发落地（D43–D46）：更新管线（动态薄层 endpoint + stable/beta 通道 + 5% 灰度起步）、macOS 公证必选 + Windows OV 签名、迁移单事务 + 迁移前强制备份（D32 第一个强制调用场景）、beta/stable 分库目录。

### 1.0 之后的接缝

| 能力 | 接缝 | 触发 |
|---|---|---|
| GraphRunner | `graphs` / `graph_nodes` | ask/paper 契约稳定 |
| dense / BM42 / SPLADE | `retrievers[]` + derived 目录 | BM25 基线有了 |
| WebSub relay | `feed_sources.hub_url` | 真有公网回调 |
| LaTeX 编辑 | change_sets | 写作成为主路径 |
| 演进树 | citation graph derived | 库内边够 |
| 相关论文扩展（PPR） | 引用边入 `graph_links`（paper add 时抽取） | v0.4 起积累引用元数据；查询需求出现时再实现 |
| 高配 PDF 解析（GROBID/Marker） | 版本化子进程协议（见 [Python 与 Rust 协作边界](python-rust.md)） | 默认层解析质量被实测证明不足 |
| MCP | 无，不进 core | Answer/RunEvent 极稳之后 |
| local 端点 preset（Ollama 探测） | `[privacy].prefer_local_endpoint` + preset enricher | 1.x；隐私 vault 需求出现 |
| 槽内 provider fallback | 同槽位失败换 provider（可靠性机制，非质量路由） | 1.x 复议 |
| 元数据在线刷新 | models.dev/LiteLLM 24h 本地缓存，doctor 手动触发 | 1.x 可选，默认关（投毒先例） |
| `apps/www` | 独立站 | 需要营销页 |
| docs 迁入 www | 明确单一域名时 | 默认不迁 |

## 21. 明确拒绝的设计

1. 不做通用多 Agent 框架，不移植 32 工具注册表和 LangGraph。
2. 不在核心引入 Python 运行时。
3. 不做 plugin registry。
4. 不做 Dify / Notion / Obsidian 同步。
5. 不做通用 provider matrix。
6. 不做 workflow engine。
7. 不做 JSONL sidecar 作为运行时状态。
8. 不把流式 token delta 写入 SQLite。
9. 不让 AI 自动创建事实；不让 Spark 自动变笔记。
10. 不让检索命中直接等于模型上下文。
11. 不让 LLM 负责数学正确性或引用正确性。
12. 不把 WebSub / OPML / JSON Feed 当成三种检索后端。
13. 不把本机无认证 HTTP 当核心 IPC。
14. 不把 `apps/desktop` 写成第二个业务层。
15. 不为移动端提前做七层 Storage 抽象；`VaultScope { root, name }` 足够。
16. 整库 `.db/.wal/.shm` 与 Tantivy 索引目录进任何同步目录——SQLite 官方禁令；同步走快照单向流（D31）。
17. CRDT / 双向同步 / 多写者复制——单人单写者场景负资产；重评触发 = 真实多写者需求（D31）。
18. 对 canonical 审计表（runs / run_events / history_items 等）的任何 DELETE，以及 run_events 按月分区表——归档走 `VACUUM INTO` 搬移，永不删除（D30）。
19. 默认遥测与 opt-out 遥测——零遥测是立场不是开关；任何未来上报必须 opt-in 且首次显式二选一（D45）。
20. 默认层引入 C/C++ PDF 解析绑定（mupdf/pdfium 系）——AGPL + CVE 流 + SQLite 踩踏向量；需要保真度时走 D18 子进程接缝（D37）。
21. EV 代码签名证书——2024 年起对 SmartScreen 无加成，纯溢价（D44）。
22. 改写用户 vault 文件、键层大小写折叠或 NFKC、把索引/文本解析信任给 watcher 事件内容——事件是提示，对账是真相（D40–D42）。

## 22. 决策记录 {/* #decisions */}

Grill 裁定的详述在 [Decision records](adr/chunks-are-a-retained-projection.md) 目录；编号 `ADR 00xx` 在正文标题，不在文件名。

### D1：SQLite 存关系事实，Tantivy 存倒排

SQLite 是 canonical run/source 的家。Tantivy 是 derived 检索。重建倒排不得改 history。chunks 是保留型投影，见 [ADR 0022](adr/chunks-are-a-retained-projection.md)。

### D2：Markdown / PDF 是用户事实源

运行事实不可重建。被引用的 revision 必须保留。论文字节在 workspace 数据目录，不进 Vault（[ADR 0019](adr/paper-bytes-live-beside-the-database.md)）。chunks 见 [ADR 0022](adr/chunks-are-a-retained-projection.md)。

### D3：run / history / event 分离

`runs` 是被接受的工作；`run_events` 是过程；`history_items` 是可延续 transcript。

### D4：检索命中和模型上下文分离

`retrieval_hits` 记录找到什么；`run_context_items` 记录模型实际看到什么。

### D5：citation 绑定 revision

引用必须包含 `source_key + revision_hash + chunk_id + chunk_hash + locator`。

### D6：citation 验证先于 CAS

1.0 的权威验证是引用。数学验证是后置接缝，不拖 Python。

### D7：MVP 不做 Agent loop

`ask` 是 pipeline。0.x 模型没有工具（[ADR 0005](adr/ask-has-no-model-tools.md)）。后续图编排也必须先 durable settlement，每节点至多一次 provider turn。

### D8：MCP 延后

先把 `Answer`、`RunEvent` 和 citation contract 稳定。

### D9：Turborepo `apps/` + Cargo `crates/`

可运行面进 `apps/`，带薄 `package.json`。Rust 库进 `crates/`，pnpm 看不见。不把实验性 Cargo workspace 发现当地基。

### D10：核心 Rust，桌面 Tauri 2

废 Python CLI 核心和 PySide6 默认路线。现有 Vite/React 做产品 UI。

### D11：产品不是全能科研 Agent

人是作者。Project / Annotation / Spark 是人的工件。Agent 不能自我扩大执行边界。

### D12：无产品 Web 后端

文档站与后置营销站不是引擎表面。桌面 IPC 用 Tauri command。

### D13：Feed 轮询，WebSub 只留字段

OPML 是订阅列表交换格式。JSON Feed 的 `hubs` 只记录。

### D14：BM25 默认；BM42/SPLADE 是实验位

配置 `hybrid = false`。混合检索接口先留，不预实现模型推理栈。

### D15：终态必达，副作用幂等

terminal 事件与 `runs.status` 同事务落库；崩溃清扫为 `failed(interrupted)`；
`paper add` / ingest 幂等键保证重试不产生重复事实。细则见 [Harness 工程](harness.md)。

### D16：检索升级以回归指标为门槛，不以先进为理由

BM25 基线必须先有 fixture golden set 的 `recall@k` / `MRR` 数字，dense / hybrid /
rerank 的引入以回归不退步为验收。细则见 [上下文与检索工程](context-retrieval.md)。

### D17：contracts 单一事实源在 Rust，投影只出不进

TS（`packages/ipc`）与 Python（评测）的类型由 `momotaro-contracts` 投影生成，
方向永远 Rust → 其它语言。Python 不写 canonical、不进运行时。细则见
[Python 与 Rust 协作边界](python-rust.md)。

### D18：PDF 双层解析，外部解析器走子进程接缝

默认层 = 同步子进程（纯 Rust crate，版本化 stdin/stdout 协议），CLI 与桌面共用同一二进制；GROBID / Marker 走同一协议按需启用（[ADR 0006](adr/pdf-parse-is-subprocess.md) 取代「进程内默认」表述）。理由（2026-09 学术 RAG 调查）：解析端正在 VLM 化快速演进，把任何重型解析器捆进 Tauri 二进制都会制造分发负担与快速过时；JVM（GROBID）与 Python（Marker/MinerU）依赖都通过同一子进程协议隔离。分层哲学与 LazyGraphRAG 同构：便宜的处理覆盖全部，贵的处理延迟到需要时。 abort 类风险进程内不可防，故默认层从第一版就隔离（D37）。

### D19：图的正确用法是引用网络，不是实体抽取图

微软 GraphRAG 全量管线（社区摘要 + LLM 实体抽取）已被实证为事实型查询负
收益（GraphRAG-Bench, ICLR 2026）。Momotaro 的图路线：论文引用边在
paper add 时从参考文献元数据确定性抽取（人工标注结构，零 LLM 成本），
derived 存 `graph_links`；实体图只在 schema 限定（方法/数据集/任务/指标）
且检索基线显示多跳需求真实存在后才考虑。PPR 扩展是后置接缝，不是 1.0
承诺。

### D20：缓存是第一等工程约束；上下文常数是待校准参数

（2026-09-15 上下文工程调查，239 项文献）三条公理：上下文是有限资源且边际
收益为负（8192=32k 的 25% 恰落在 RAG 峰值填充区）；角色/指令分离是弱保证
（关键约束必须确定性代码执行）；可验证事实与可压缩叙述必须分层（evidence
原文永不压缩、约束常驻、AI 产出永远 proposed）。落地：前缀字节级稳定 +
tools canonical 序列化 + `cached_tokens` 记账 + session 内证据位置守恒
（CacheWeaver：rank 序是缓存失效模式）；压缩触发线（默认 0.85、早压 0.40 降为
实验档，见上下文工程文档 §4 的合并裁决）、估算系数 1.25、top-k 兜底均标注为
「假设 + 观测闭环」的可校准参数。语义缓存、软压缩、
运行时自演化上下文（ACE 式）、LLM 自主触发压缩：明确不采纳。细则见
[上下文与检索工程](context-retrieval.md)。

### D21：AI 综合页是 canonical 工件，不是事实层

（2026-09-15 LLM Wiki 调查）wiki/synthesis 页是显式 run 的产物（与 `answers`
同族，不是新记忆/事实层）：断言级引用绑定五元组、pin 前不入索引、页面变更
只能通过新 `page_revision`（append-only，diff 人审）、整合时只读源 chunk 与
既有 pinned 断言（切断自我反馈环）。拒绝四点：LLM 全权改写正文、无 claim 级
溯源、自我反馈整合、生成页自动入索引。`wiki_pages` 表 v0.7+ 评审后 Project
级试点——综合成本只在反复重访的项目库上为正。人审界面是 diff + 受限动作集
（Pin/Revise/Dismiss），不是聊天解释。

### D22：wiki-link 是确定性链接资产；PPR 是种子发现面板

`[[双链]]` 在 ingest 时确定性解析入 `graph_links`，是 §11 Link expansion 的
第一实现；LLM 只能建议链接，不能建立链接。`link_score` = 1-hop 小额加成
（`×(1+δ)`，离线 recall 校准，同为乘性因子）。PPR 不进 `final_score`：HippoRAG 消融（换非 LLM
抽取器后 PPR 增益近乎消失，72.9→58.4 R@5）说明其增益依赖 LLM 抽取图；确定性
引用图上仅存的产品级证据是 Inciteful 式「种子发现面板」。

### D23：会话分叉 = 复制前缀；交接走 handoff.v1

（2026-09-15 会话管理调查）fork 时子会话复制父会话截至边界的
`history_items`，首条写 fork 元数据 `{fork_point_run_id, parent_session_id,
copied_until_seq}`；父不可变，两支独立演化；回归测试断言子前缀逐条等于父
截断前缀。业界全场共识是复制前缀，无生产系统用「引用父前缀」。
跨模型续写与 fork 携带固定 schema 交接包
`handoff.v1 {decision_state, open_items, artifact_pointers, caveats}`——
跨模型交接本身是一等失败源。跨会话调用不需要新实体：任务图节点派生 Run
即 ephemeral subagent 的正确同构。

### D24：agent 对共享工件 ADD-only；取代链在检索层解析

Pin 与检索资格分离见 [ADR 0013](adr/pin-without-five-tuple-is-not-indexed.md)。

任何 agent 角色对黑板工件只可追加（新行 + supersedes 指针 + `superseded_at`
双时间戳），不可 UPDATE/DELETE 他人工件；清理属于人的控制面（Mem0 投毒后
整体回撤 ADD-only 的一手证据链）。检索命中已被取代的工件时自动改投链头或
降权。固化/蒸馏产物的信任级显示不得高于其源记录的最低信任档（provenance
洗白防御）；pin（人审）授予检索资格，不改变信任构成。会话间只共享**已提交
工件**（run 串行提交边界内），私有上下文与中间 token 永不入共享层。

### D25：模型槽位 = 封闭语义枚举 + 静态映射 + 三态回退

（2026-09 模型层调查）槽位枚举封闭：`answer | utility`（schema 版本升级才能
加）；`local` 不是第三槽，是指向本地端点的配置形态 + `[privacy]` 开关。
mode→slot 是**静态确定性映射**，不是 learned router：ask/spark→answer
（thinking=medium）、expand/标题/planner→utility（low）、**compact→answer+low**
（五家先例 + 压缩输入=全部历史、同模型保温 KV 前缀缓存；`[slots.compact]`
两向可翻转）、verifier 无槽位（永不耗 token）。三态回退：显式指定 / 空串
显式禁用（回退 answer）/ 未设置=自动——未配置 utility 时与单模型行为完全
一致，`[model]` 即 answer 槽别名。槽位是「带语义的端点引用」，不是 provider
registry，无运行时多 provider 切换；learned router / cascade / 模型自选 /
全量 provider registry 明确不做。`llm_turns` 记 `slot` 与 `reasoning_tokens`。

### D26：thinking = 语义四档，冻结进 RunSpec

枚举 `off | low | medium | high`（语义档而非参数名——三家一年内换过控制方式，
参数名会腐烂）。映射与回退规则进 adapter 能力矩阵，显式记录实际下发参数：
超四档值收编（max→high、minimal→low，不是 off）、medium 缺失降 low（宁可
少想）、off 不可用降最低可用档并写 run_events。默认 answer=medium、
utility=low。`reasoning_content` 不进模型上下文，只入记账（reasoning_tokens
按输出价计费是各家现实）。依据：When Thinking Fails（NeurIPS 2025 Spotlight：
开思考使格式遵循 13/14、15/15 回退——直接威胁 citation 格式）+ ThinkPrune
（65% 可剪无损）+ overthinking 的 agentic 反转证据。

### D27：模型元数据 = 内置 curated preset + 未登记放行 + unknown

仓库内 ≤10 条 preset（数据文件非代码）：context_window、价格四元组（含
cache_read/cache_write per-million）、thinking 能力矩阵、default_utility 推荐；
价格快照带 `pricing_version` 戳与来源。**未登记模型直接放行**：警告 + 保守
兜底继续运行（aider/codex 模式），不报错拒跑。价格永远 advisory、unknown ≠ 0。
0.x 不做在线拉取（litellm 2026-03 投毒先例）；1.x 可选后置、默认关、不进运行
时关键路径。`momotaro doctor --probe <slot>` 探测存在性与 thinking 参数，失败
= 结构化错误（`model_probe`）。token 估算系数（D20 待校准项）按 preset 分档：
CJK 高占比模型 1.4-1.5。

### D28：key 分层读取链（修订 §7 规则 1）

显式参数（测试）→ 环境变量（CI/headless 首选）→ OS keychain（桌面默认，
Rust `keyring` 4.2 三后端；per-slot service = `momotaro.<slot>`）→ 配置引用
+ 明文警告（最后一级；git/gh/Electron/VS Code 四先例——无工具在无钥匙串
环境拒绝工作）。tauri-plugin-stronghold 已官方弃用，不可用。工程纪律：
keyring「不可用/被锁」与「无凭据」显式区分并提示；Windows 单凭据访问串行化。

### D29：SQLite 引擎纪律（PRAGMA 定稿 + 版本钉扎 + 红线=治理触发）

缺省 PRAGMA 八条定稿与容量触发表见 [规模与生命周期](scale-lifecycle.md)。
**引擎版本钉扎**：捆绑引擎必须落在 WAL-reset 窗口之外。上游口径（sqlite.org/wal.html
§11）：缺陷存在于 **3.7.0（2010-07-21）至 3.51.2（2026-01-09）**，**自 3.51.3
（2026-03-13）起修复**；窗口内另有 3.44.6 / 3.50.7 两个回补版本。3.52.0 已撤回
（误报 `integrity_check` 损坏），**永不采用**——它与窗口是两个不同的拒绝理由。
`doctor` 输出 `sqlite_version()` / `sqlite_source_id()` / `PRAGMA compile_options`，
并**在窗口内硬失败**（`status = invalid`、退出码非 0）——引擎版本不是可选信息。
容量红线不是「库不能超过 X」而是指标越线触发治理动作（数字待 pilot）；
写连接唯一、读连接不限；mmap 与 auto_vacuum 显式不开（Windows VACUUM 静默失败）。

### D30：run_events 生命周期——归档 ≠ 删除

`run_events` / `history_items` 是审计正史：治理只有搬移（月度 `VACUUM INTO`
归档库，归档文件仍属 canonical，trace 跨库透明读）与派生重建
（usage_projections）两种合法形态，**永不 DELETE**——先例的 prune-on-insert
对象全是可丢遥测，不可照搬。默认单表 + `created_at` 时间索引 + 聚合走派生表，
是否归档由 pilot 数字决定；分区表出局（存量 migration + 跨分区唯一约束两关，
收益不抵摩擦）。快照滚动预算 日7/周4/月12。Tantivy：commit 去抖 5-10s
（Windows #2847 正确性防御）、不主动 purge_deletes（#710 教训）。

### D31：同步边界——C1 逐字否决，C2 快照单向流，CRDT 排除

论文字节与快照范围见 [ADR 0019](adr/paper-bytes-live-beside-the-database.md) / [ADR 0023](adr/snapshot-excludes-paper-bytes.md)。

整库 `.db/.wal/.shm` 与 Tantivy 索引目录进任何同步盘 = 官方禁令，进 §21。
1.0 通路 = 桌面定时 `VACUUM INTO` 快照 → 用户同步目录 → 移动端只读打开
（快照时间戳命名防网盘冲突副本）；C5 源 Markdown 镜像同步并行留缝（移动端
从源重建索引的降级实现，不承诺）。CRDT / 双向同步 / 多写者复制正式排除
（local-first 研究：单编辑者文件同步即工作得很好；单人场景历史膨胀负资产）。
运行时防御：打开库探测网络挂载并告警；数据目录三分离——源 Markdown（可
同步）/ 运行时库+索引（禁同步）/ 快照导出（可同步）。移动端口径「小时级
滞后」。

### D32：备份四步法与腐化自检四层

0.x 快照默认不含 papers 目录（[ADR 0023](adr/snapshot-excludes-paper-bytes.md)）。

备份 = 临时名 `VACUUM INTO` → 产物 `quick_check` 验证 → 记快照清单 → 原子
改名（中断只废产物不伤原库）。检测四层：打开时 `quick_check` +
`foreign_key_check`（integrity_check 不查外键）；备份产物验证；**审计对账**
（runs 终态计数 vs terminal 事件计数——抓 integrity_check 查不出的静默丢写）；
路径告警。恢复 L0 隔离 → L1 `.recover` → L2 快照重放 → L3 源 Markdown 全量
重建兜底。修复阶梯上限：每进程每路径一次 + 持久台账 + 跨进程锁（hermes
105 次/89GB 修复风暴的制度化防御）。验收含 kill -9 ×1000 故障注入。细则见
[规模与生命周期](scale-lifecycle.md)。

### D33：同库互检是差异化定位——「首个组合」，不称「首创」

v0.3 兑现为混池 BM25，不是 1-hop 附赠（[ADR 0004](adr/same-library-retrieval-is-mixed-bm25.md)）。

（2026-09 产品竞品调查）定位表述：本地优先、跨平台、LLM 时代、带 chunk 级
引用绑定与信任档的同库互检——组合（双向互检 × 五元组 × 信任档 × 本地优先）
无在位者，单项均有先例（DEVONthink 25 年 / Omnisearch 187.9 万下载），叙事
诚实。付费意愿未验证，留用户访谈 pilot，不为未验证需求写代码。叙事纪律：
不引用无主数据（三条流传声明已证伪）；差异化锚定可验证性而非「不训练数据」
（已 commodity 化）。

### D34：信任靠离散档位与验证终点呈现

信任级显示用**离散档位徽标**内联每条证据/引用处（Dietvorst 不对称惩罚：
一次低分即弃用，数字分数禁用）；proposed 内容显示灰色「待 pin」态不隐藏。
Provenance 面板两级漏斗：hover 五元组摘要 → 点击跳原文高亮，前置系统验证
结论（用户很少真的点开引用）。Trace 回放以**验证结果为默认终点**（首屏显示
通过/失败/漂移）。总原则：透明≠信任（成功标准是改变核验行为）；顺序即
干预（证据先于断言）；引用是结构化契约。细则见
[产品信任面与分发工程](distribution-trust.md)。

### D35：无 key 首启即首次价值时刻

无 key 本地能力（index + lexical search）是首启信任基座而非降级态：首个动作
零成本，AI 功能表述「未启用」给出最短解锁路径（免费额度 provider 优先），
empty state 内置示例。本地模型形态 = 小模型只支撑非对话功能 + 指向免费额度
provider；**不把「装 Ollama + pull 模型」作为默认首启路径**（五个非开发者
断点）；内置聊天 LLM（1.6GB+）是可选增强。key 配置页五要素（预设列表 /
测试连接 / 申请指引 / 免费额度指向 / keyring 不回显）。

### D36：成本展示——unknown 红线四态与 BYOK 措辞

per-run 小字 + 统计页聚合（缓存 token 单列，provider 折扣差 12 倍）；
**unknown 红线四态**：确证免费 = `¥0`、已知定价 = `≈¥x`（附快照日期）、
未知定价 = `¥?`（token 照常）、usage 缺失 = 全 unknown——绝不把未知渲染
成 0。BYOK 措辞：「token 数是事实，金额是估算」视觉分离；「不经手你的 API
请求，不加价、不抽成」明说；禁用「额度快用完」类措辞。成本叙事：「agent
贵在循环，Momotaro 贵在克制」（单 turn 形态重度月成本 ¥5–60）。

### D37：解析层信任分级——默认层即同步子进程（纯 Rust）

与 [ADR 0006](adr/pdf-parse-is-subprocess.md) 同口径。

paper add 的 PDF 解析**从第一版就走子进程**（Tauri sidecar、D18 接缝同一
协议）：canonical 库不可重建，而 abort 类风险（栈溢出/OOM）在进程内纯 Rust
也防不住——结构性崩溃隔离的价值超过一次性限额代码成本。默认层限纯 Rust
crate（禁 FFI：mupdf/pdfium 的 AGPL + CVE 流 + SQLite 踩踏向量）；OS 资源帽
（Job Objects / RLIMIT / watchdog 外杀）+ 入口硬 cap + 来源记账（自建 MOTW）
+ 解析在写事务外（进程死 = 丢本篇，库完好）。解析入口收敛单一 trait 边界，
(b)→(c) 常驻子进程是接缝后替换。细则见
[摄取安全与 Vault 互操作](ingest-interop.md)。

### D38：Feed fetch/parse 五件套与四态 reason-code

quick-xml ≥0.41、scheme allowlist（http/https）、响应字节硬上限 10–15 MiB
（流式截断，无视 content-length）、总超时 20–30s、OPML 同管线校验；私网/环回
字面量拒绝。四态映射落地（§15）：限长截断记 `response_too_large` 不误归
parse_error；304 = success(not_modified)；0 items = no_results。不做
https-only 强制 / per-host 限流 / DNS pinning（单用户桌面威胁模型）。

### D39：cargo 供给链审计基线

零成本立即项：全 crate `publish = false`、CI 全部 `--locked`、workflow
`uses:` pin 完整 40 位 SHA（tj-actions 实证 tag 可重写）。`deny.toml` 为唯一
策略文件（防 audit/deny 双 ignore 清单漂移）；CI 双时点（PR 拦新增 + main
每日拦存量后披露）。license 白名单（GPL/LGPL/AGPL 默认不放行）；新增依赖
验收八条（默认答案是不新增）。发布私钥只在发布 CI 受信 environment + 离线
冷备。认知基线：审计是「已知问题报警」不是「未知攻击防御」。

### D40：文件监控——事件是提示，对账是真相

笔记改名 = 新 Source，不改写已落库键（[ADR 0014](adr/note-rename-is-a-new-source.md)）。

watcher = `notify` 8.2.0 稳定线 + `notify-debouncer-full`（Windows 溢出缺口
由对账兜底，不冒险 RC）；事件永不 trust，全部归并为「重扫该路径」：stat
双验（racy 强制内容校验，git 同构）→ 读全文双 stat（撕裂重试，绝不落库）→
sha256 与库比对才重建 chunk。对账四触发（启动全 walk / 每日 / 手动 / watcher
异常恢复），10k 文件亚秒级。单文件更新 = 恰好一个 SQLite 事务，chunk 内容
寻址 upsert，canonical 永不污染。网络盘维持探测告警（D31），不做轮询降级
实现。细则见 [摄取安全与 Vault 互操作](ingest-interop.md)。

### D41：vault 语法边界与建边矩阵

白名单 `*.md` + `*.canvas`；附件登记不进 FTS；一切点开头目录/文件黑名单。
wikilink 全形态建边（pulldown-cmark `ENABLE_WIKILINKS`）；dangling 链接是
一等公民（target NULL 保留 raw，日后回填）；同名歧义三级规则（含路径精确
匹配 → 裸名唯一 → 多候选不建边 + 事件，比 Obsidian 保守）。降级事件十类：
每条「没索引/没建边」的决定都留可审计事件。不改写用户 vault 文件、不以
私有语法（Dendron 式）建边。

### D42：source_key = NFC 身份键，键层永不折叠

`source_key = note:<vault-relative NFC path> | arxiv:<id>`；NFC/不折叠/碰撞规则作用于 scheme 之后的 tail。verbatim 前缀与盘符大小写由解析时的 `canonicalize` 决定（既不是键的一部分，也不是我们做的改写）——用户可见处一律渲染存储的 `local_path`，不渲染解析后的路径（[ADR 0002](adr/source-key-is-scheme-prefixed.md) / [ADR 0024](adr/local-path-is-workspace-relative.md)）。大小写原样保留、`/` 分隔；**禁用 NFKC 与 casefold**（Linux 大小写双写合法并存，折叠 = 数据丢失级事故）；`raw_name` 原始字节另存（事件匹配/显示）；查询层大小写折叠另建 fold 索引列。NFC 碰撞（NFC/NFD 双写）：两条目都保留 + 消歧后缀 + 事件，绝不静默合并。同步盘预警：OneDrive placeholder 只建元数据不读内容；Windows 非法名在非 Windows 端照常索引但发预警（会经同步盘毒化 Windows 端）。平台测试矩阵 11 项进 CI。

### D43：更新管线——动态薄层 + 通道 endpoint 隔离 + 灰度

endpoint 双源：主源 = 轻量动态层（中国可达性、灰度放量、紧急停更三件职责，
Serverless 级成本近零）；备源 = GitHub Releases 静态 latest.json。stable/beta
两条 endpoint 隔离（版本号不设防）；灰度 = install-id hash 取模，1.0 起步
5%；坏版本应急 = 立即发更高版本号修复版。Windows `on_before_exit` 是更新
安装前唯一安全窗口（WAL checkpoint + 会话落盘）。明确不做：delta 差分、
静默安装（平台必弹框）、「启动即崩自动回退二进制」（数据防线替代）。细则见
[产品信任面与分发工程](distribution-trust.md)。

### D44：签名分发——macOS 公证必选，Windows OV 档

macOS：$99 Apple Developer Program + 公证，1.0 必选。Windows：**OV 档**
（Certum 开源开发者 €69/年或 SignPath OSS 免费），同一证书签每一版（信誉
跨版本继承）——目标人群是非开发者研究者，SmartScreen 绕过说明书的转化损耗
远大于开发者产品；**EV 证书明确不做**（2024 年起无 SmartScreen 加成）。
Linux：AppImage + GitHub + GPG 内嵌签名起步；Flathub 后置。1.0 预算
~$200/年量级。

### D45：零遥测 + 本地崩溃 journal + 主动导出

零遥测是立场不是开关：无启动 ping、无使用统计、无自动崩溃上报；唯一网络
行为 = 更新检查（可关）；纯本地不外发在 GDPR 下不构成「处理」，无需同意
界面。崩溃链路：panic hook → 本地 JSONL journal（写入时 home 脱敏，轮转
20 条）→ doctor 查看与诊断 bundle（先枚举内容再给导出按钮）。**永不写
minidump**（栈内存夹带用户内容）。任何未来上报必须 opt-in 且首次显式
二选一（Logseq 默认开是反面教材）。

### D46：迁移单事务 + 迁移前强制备份 + 通道分库

每次 schema migration 全部 SQL 一个事务（`user_version` 是最后一条）——
中途崩溃自动回滚，天然消解迁移崩溃循环。**迁移前强制备份**（D32 四步法的
第一个强制调用场景）：备份失败则迁移拒绝开始，无跳过选项；产物命名带前后
版本号。版本锁文案六要素（两个方向：旧开新无逃生门按钮；新开旧确认明示
「升级是永久的」）。beta 与 stable **分库目录**（首个 beta 从 stable 复制
副本初始化，从此各自演化；schema 不承诺跨通道兼容窗口）。

## 23. Definition of Done

### 引擎 Beta（约 v0.3）

```text
1. momotaro index 可索引 fixture vault。
2. momotaro search 无 API key，零 LLM。
3. momotaro ask 返回带引用答案。
4. 每个引用可定位到 source revision 和 chunk。
5. 每个回答有 run_id。
6. momotaro trace 能回放 query、context、LLM turn 和验证结果。
7. 引用失败显式标记，不伪装成功。
8. 索引重建不修改历史事实。
9. fake LLM integration test 全部通过。
```

### v1.0.0 Formal Release

```text
1. 跨平台桌面 GUI，不要求用户使用终端。
2. Windows / macOS / Linux 安装包可启动并完成核心流程。
3. GUI 覆盖 vault、索引、search、ask、papers、project、annotation、trace。
4. GUI 与 CLI 复用同一套核心服务和契约。
5. 引用可跳转到来源原文、revision 和 locator。
6. Trace Viewer 可展示 retrieval、context、LLM turn 和验证状态。
7. 后台任务显示进度、允许取消、暴露结构化错误。
8. 无 API key 时本地索引和 search 仍可用。
9. CLI 与 GUI 的相同操作产生兼容的 run trace。
10. Spark 若已实现：未 pin 不进检索；未实现则不阻塞 1.0，但契约已留表。
```
