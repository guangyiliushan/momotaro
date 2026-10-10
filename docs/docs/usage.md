---
sidebar_position: 3
---

# Usage

## Install

`0.x.y-beta` 的界面是 Rust CLI。在 monorepo 根目录：

```bash
pnpm install
pnpm turbo run dev --filter=@momotaro/cli -- --help
```

或直接：

```bash
cargo run -p momotaro-cli -- --help
```

`1.0.0` 起，日常使用走桌面应用；CLI 留给自动化、调试和批处理。

## Initialize a workspace

```bash
momotaro init
```

创建本地数据库并校验 `momotaro.toml`。

## Index local notes

```bash
momotaro index ./notes
```

索引是确定性的，不需要 API key。

`index --json` 还会给出本批次的标识：

```json
{"batch_id": "index-1737000000000-2d87c684", "files_scanned": 5, "files_indexed": 5, …}
```

`batch_id` 也是 `store_events` 里那一批审计行的 `batch_id`（[ADR 0026](adr/store-events-are-the-ingest-audit-log.md)）：
想知道「这次索引对每个文件做了什么」，按它查即可——

```sql
select seq, type, outcome, reason_code, source_key from store_events
 where batch_id = ? order by seq;
```

人类可读的摘要行不带批次号（只有 `--json` 给）。

## Check the workspace

```bash
momotaro doctor
momotaro doctor --json      # the same report for scripts
```

`doctor` 不写任何东西，未初始化的 workspace 也会退非零（见下方 "Exit codes"）——**`--coverage` 是例外**：它是报告不是检查。

想知道**上一次 `index` 对每个文件做了什么**（包括被拒的那些），读审计日志：

```bash
momotaro doctor --coverage
# ingest batch "index-1737000000000-2d87c684": 5 indexed, 0 skipped, 0 refreshed, 0 failed
momotaro doctor --coverage --json   # CoverageReport，字段同上有事件计数与 finished
```

还没索引过时它会说 `no ingest batch recorded yet` 并**成功退出**（"没有批次"是事实，不是故障；
`--json` 下 stdout 给 `null`）。未结束的批次会标成 `no ingest.finished yet: still running, or the
process died`——日志分不出这两者，所以它不假装能分。`ingest.coverage` 的三种结果分别计数；
降级（D41）与同步盘预警（D42）各按码计数（`degradations` / `warnings` 两个清单，人类行逐行渲染）；
**预警不是失败**——只有两个词表都不认识的码才计入 failures。任何看不懂的 outcome 都会**归入 failures**
而不是被丢掉。库不可用时（版本过新、外来库）退 1，
报的是与 `doctor` **同一句拒绝文本**（含引擎版本）。

## Search

```bash
momotaro search "Fourier transform"
momotaro search "谱定理" --json
momotaro search "transformer reasoning" --mode expand
```

默认 `--mode lexical`：纯 Tantivy BM25，**不得消耗 LLM token**。

返回条数默认 10；`-n/--top <N>` 改上限，**N > 1000 被拒绝**（`--top 0` 合法——返回空结果，不是错误）。

`--mode expand` 显式开启一次短 LLM turn（可观察输出之前的重发仍算这一次），把输入扩成多个查询再检索聚合。这是搜索模式，不是 Agent 循环。

`--mode expand` 会建立一条 `mode = search` 的 run：`--json` 输出里带 `run_id`
（人类可读输出也会先打一行 `run <run_id>`），可以用 `momotaro trace <run_id>` 回放——
重写的那一次 LLM turn 与它报的 token 用量都在里面。四道门（`--no-llm`、未授权网络、
索引与库不可用、spec 构造与校验——端点未配、空 query）都在建 run **之前**，所以被拒时
不会留下任何行；四道之间的先后是实现细节（清单与口径见
[architecture](architecture.md) §10.2 的「写入 / 门禁」两行）。**层要分清**：这四道是
`search --mode expand` 的，它进门就问、四道全在建 run 之前；**`ask` 只共享其中三道**（`--no-llm`、未授权网络、spec 构造与校验）。第四道
「索引与库不可用」对 `ask` 要拆开看：**库**在建 run 之前问（`open_run_store`），
**索引**在 [Architecture](architecture.md) §10.1 第 5 步、建 run **之后** —— 所以索引
不可用对 `ask` 落的是一条 `failed` run（见
`an_unusable_index_fails_the_run_as_index_unavailable`）而不是零行。**「端点未配」是 CLI 在组 transport 时问的**（`apps/cli`，也早于引擎被调用），
`ask_workspace` 自己只校验 `[model].model_id`。

`--no-llm` 下 `--mode expand` 直接拒绝并点名两者
（`policy refused the request: --mode expand rewrites the query with an LLM, which
--no-llm forbids`）；lexical 搜索不受影响——它本来就零 token，`--no-llm` 对它是
同义反复。

## Ask with citations

```bash
momotaro ask "解释谱定理和 PCA 的关系"
```

每个回答绑定 `run_id`，并记录：

1. 原始 query；
2. 检索命中；
3. 实际送给模型的 context fragments；
4. 这一次 LLM turn；
5. 引用校验结果。

自动化里重试同一个提问时，带上 `--client-request-id <id>`：同一个 id 重放返回**同一条 run**
（不重复建 run、不重复计费）；同一个 id 带不同的 query/spec 会被**点名拒绝**（幂等键冲突），
空 id 也会被拒绝。

## 多轮会话

`ask` 默认每次新开一个会话。要接着上一轮问，带上会话 id：

```bash
momotaro ask --session <session_id> "再问一个"
```

id 从上一条 `ask` 的输出里取（人类行与 `--json` 都给，并打印一条可直接粘贴的下一条命令）。
`--session` 的值**必填**；给一个不存在的 id 会**直接报错**，不会替你新建一个会话。
该会话上一轮若因崩溃/中断悬挂着，**这一步会先清扫它再继续**（[Harness](harness.md)
§2 规定清扫本身、§5 规定「先清扫、后判 session」这个顺序，`ask` / `trace` 打开库时清扫，
`--session` 也不例外），所以中断的会话不需要先去别处跑一条命令才能续上；**没有**单独的「该会话正忙」拒绝（判不出活跃 run 背后是活进程
还是死进程，见 Harness §5）。
`AskReport` 因此多一个 `session_id` 字段（`--json` 与人类行各一处）。

续接时旧轮按 [上下文与检索工程](context-retrieval.md) §3 选择：近 2 轮（且 ≥8K token）
保底，取宽者；已注入过的证据按 `chunk_hash` 去重，所以重复问同一批笔记不会把同样的正文
再占一遍预算。更早的轮次**在有 `summary.v1` 摘要记录时**才降级为该摘要——**0.x 不产生
摘要记录**（[ADR 0010](adr/session-ask-retrieves-fresh.md) 的 status 段：生成等 `kb_read`），
因此今天每一轮都是逐字重放。

## Provider endpoint

`ask` 与 `search --mode expand` 各要一次 provider turn（§16：可观察输出之前至多
重发一次，仍是一次 turn）；本版其余命令零网络。这两个命令还受网络策略约束：
默认拒绝，需 `--allow-network` 或 `[policy] allow_network = true`——与下面
Policy flags 的「Beta 默认保守」是同一条。
端点与密钥都**不写进 `momotaro.toml`**：配置只写变量名，值留在环境里
（architecture §7）。

```toml
[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"  # 变量名；值 = API 根地址，如 https://api.example.com/v1
api_key_env  = "MOMOTARO_API_KEY"   # 可省略：本地端点无需密钥
```

```bash
export MOMOTARO_BASE_URL="https://api.example.com/v1"
export MOMOTARO_API_KEY="sk-…"
```

未设置 `[model].base_url_env`、或它指向的变量没有导出时，命令在**建立连接之前**
以 `provider unavailable: …` 拒绝（退出码 1），并且不产生 run 行。
`api_key_env` 省略 = 不带密钥；写了而变量没导出，同样在拨号前拒绝。

## Stats

```bash
momotaro stats
```

Counts for the local store: workspace status, schema version, database path,
initialized, sources, chunks. `--json` prints the same fields; a count that is not
known prints `unknown` on both outlets (never a fabricated `0`).

## Inspect a run

```bash
momotaro trace <run_id>
```

用来回答：

1. 检索找到了什么？
2. 模型当时看到了什么？
3. 用了哪些引用？
4. 引用是否通过校验？

## Projects, annotations, sparks

> **后置**：`project`、`annotate`、`spark` 尚无子命令——下面这些示例今天跑起来是
> clap 的 `unrecognized subcommand`（后置清单见 [architecture](architecture.md) §17；§20 排期）。

```bash
momotaro project new "谱方法选题"
momotaro project add <project_id> --source arxiv:2401.12345
momotaro annotate <source_key> --locator ... --text "这里的假设过强"
momotaro spark --project <project_id>
```

- **Project**：人的选题抽屉。
- **Annotation**：钉在某个 source revision + locator 上的人手写层。
- **Spark**：AI 给出的启发卡，默认 `proposed`。0.x 只有 pin / dismiss；未 pin 或不带五元组的卡不进入检索，也不变成 owner 事实。

## Papers and feeds

> **后置**：`paper` 与 `feed` 的全部子命令（含 `feed poll`）尚无实现；`[feeds]` 配置段
> 同样未实现（[architecture](architecture.md) §7；后置清单见 §17）。

```bash
momotaro paper search "graph neural network reasoning"
momotaro paper add arxiv:2401.12345
momotaro feed add https://export.arxiv.org/rss/cs.ai
momotaro feed import ./subscriptions.opml
momotaro feed poll
```

发现 ≠ 入库。下载和写入库都要确认。OPML 只用于导入/导出订阅列表，不是实时协议。

## Workspace and vault

`momotaro.toml` and `.momotaro/` make a **workspace**; `[vault].path` names a
directory that must resolve **inside** it (`vault == workspace root` is legal —
[ADR 0025](adr/vault-must-live-inside-the-workspace.md)). Stored paths are
workspace-relative — a vault elsewhere would break the records.

That is also why **the sync scope must be the vault, never the whole
workspace**: SQLite's WAL does not work on network filesystems
(OneDrive / Dropbox / iCloud / NFS), and syncing the database is the fastest way
to corrupt it. Sync the notes; leave `.momotaro/` alone.

`index` reports what the run did: `indexed N of M files (X new revisions,
Y skipped), C chunks, 0 errors (0 key collisions) in T ms`. `skipped` counts files nothing was read for:
a fingerprint, stored path **and** raw name that all still match, a link the walk does
not follow, an iCloud eviction stub, a cloud placeholder, or a source whose file is
gone (its row carries `source_removed`) — a deletion is a row like any other, so
`doctor --coverage` and this line count it the same way. A pure directory
move refreshes the stored path and shows up as `indexed` with `0 new
revisions`, because no new content revision was created.

## Watch the vault

```bash
momotaro watch
```

A long-running process. It opens the file watcher first and then runs one full
vault walk+stat reconciliation — the watcher is already listening by then,
which is what makes the cookie that walk waits for mean anything — and after
that it serves events. **Events are hints, never truth** (D40): every event,
whatever it says, is merged into "rescan that path", and the rescan decides
from the file's own bytes whether anything actually changed. It reconciles
again every 24 hours, immediately when the platform reports that it dropped
events, and again when a single batch of events names more than 256 paths (an
event storm — at that point a full walk is cheaper than N rescans and is the
only thing that can be sure).

`Ctrl-C` stops it. There is no graceful-shutdown handler: the process is torn
down by the operating system. That is safe because every file's rows commit
with its data — a batch interrupted part-way simply has not happened yet, and
the next startup reconciliation picks it up.

Everything it says goes to **stdout**, in both modes. With `--json` each line
is one object tagged `kind`, so a script can tell them apart: `batch`
(`{"kind", "batch_id", "rescanned", "rebuilt", "refreshed", "unchanged",
"removed", "dropped"}`), `reconciled` (`trigger`, `batch_id`), `degraded`
(`reason`), `network_volume` (`message`), `cookie_skipped` (`reason`), and
`retries_exhausted` (`path`). A watch on
a network volume is reported and **not** promised to be real-time; a backend
that cannot start falls back to polling and says so in a `degraded` line.

## Reconcile the vault

```bash
momotaro reconcile
```

One full vault walk+stat, writing `vault.reconcile.started`,
`vault.reconcile.coverage` (one row per file) and `vault.reconcile.finished`.
This is the command the other three triggers share: the watcher runs it at
startup and every 24 hours, and again after the platform reports dropped
events. Run it by hand whenever you suspect the store and the vault have
drifted — a deleted file, a rename while the watcher was down, an interrupted
index.

With `--json` it prints `{"batch_id", "scanned", "changed", "removed",
"events"}`.

## Exit codes

`0` means the command succeeded; `1` means it failed. **`doctor` is the one
command that gates on health, not just on failure:** it exits `1` for an
`invalid` **and** for an `uninitialized` workspace, so `if momotaro doctor; then
momotaro index; fi` never indexes a workspace that is not ready (run `init` on a
fresh directory first). `--json` still prints the full report on stdout first,
so gating on the exit code loses nothing.

**Exception — `doctor --coverage` is a report, not a check.** It exits `0`
whenever the log could be read, including "no ingest batch recorded yet" (a
workspace that was never indexed): nothing having happened yet is a fact, not a
failure. With `--json` it prints `null` on stdout in that case, so a script still
gets an answer where it looks. A store this build cannot use is an error and
exits `1`, with the same refusal text `doctor` reports (the same line, engine
version included). A workspace whose database file does not
exist yet is still "nothing has happened yet": the sentence tells its owner to
run `init` — and if the directory is not a workspace at all (no `momotaro.toml`),
`--coverage` reports that instead, because `init` would fail there too.

Other commands exit `1` only when they fail, with two measured branches
spelled out here: **a usage error is clap's, exit `2`** (unknown subcommand,
missing argument) — stdout is empty, and **even `--json` gets no JSON error
body** (clap exits during parsing, before the program's own error outlet); and
**`index` per-file errors do not fail the batch** — a summary reporting
`N errors` (N > 0) still exits `0`, because each failure is recorded and the
batch completed.

`ask` exits `1` when its run failed — a failed citation fails the run
(`architecture.md` §10.1) — and still prints the answer and its explicit failure
marks on both outlets.

`reconcile` follows the ordinary rule: `0` when the walk ran, `1` when the vault
or the store was unusable. `watch` runs until it is stopped, so its exit code
only describes how it *ended* — `0` for an orderly stop, `1` when it could not
start (missing vault, unusable store, or a filesystem backend that refuses to
watch). A reconcile whose cookie wait timed out still runs the walk and still
exits `0`: waiting is an optimisation, not a precondition.

An engine inside the WAL-reset window — or a withdrawn 3.52.x release — is
reported as `invalid` too, with the bundled `sqlite_version()` /
`sqlite_source_id()` in the report: the version of the engine that writes the
database is part of the workspace's health, not a detail.

## The index is derived

`rebuild-index` reconstructs the Tantivy index from canonical storage, so an
index this build cannot open — written by an older schema, truncated mid-write,
missing a segment, or a directory left behind with no index in it — is thrown
away and built again rather than blocking the very command that exists to fix it.
The human line and the JSON report (`replaced_stale`) both say when that
happened: losing a derived artifact is fine, losing it silently is how "my
results changed" becomes a mystery.

An index directory that exists but cannot be opened is not `ready` either: the
SQLite store may be perfectly healthy, but a workspace whose retrieval cannot run
is not ready, so `doctor` reports `invalid`, gives the reason, and names
`rebuild-index`. `search` and `index` report that state the same way — `index`
only adds files the store has not seen, so it cannot repair a missing or
unreadable index, and saying so beats indexing zero files while the workspace
stays unsearchable.

Never indexed is a different case: `doctor` stays `ready` (nothing is wrong yet)
and `search` tells you to run `index`.

## Policy flags

```bash
--no-llm
--allow-network
--allow-write
--max-context-tokens <n>
--mode lexical|expand
```

Beta 默认保守：未显式允许时，网络和写文件关闭。移动端默认使用更小的 context budget。
