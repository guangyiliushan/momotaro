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

## Search

```bash
momotaro search "Fourier transform"
momotaro search "谱定理" --json
momotaro search "transformer reasoning" --mode expand
```

默认 `--mode lexical`：纯 Tantivy BM25，**不得消耗 LLM token**。

`--mode expand` 显式开启一次短 LLM 调用，把输入扩成多个查询再检索聚合。这是搜索模式，不是 Agent 循环。

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
Y unchanged), C chunks, 0 errors`. `unchanged` counts files whose fingerprint
**and** stored path both match; a pure directory move refreshes the stored path
and shows up as `indexed` with `0 new revisions`, because no new content
revision was created.

## Exit codes

`0` means the command succeeded; `1` means it failed. **`doctor` is the one
command that gates on health, not just on failure:** it exits `1` for an
`invalid` **and** for an `uninitialized` workspace, so `if momotaro doctor; then
momotaro index; fi` never indexes a workspace that is not ready (run `init` on a
fresh directory first). `--json` still prints the full report on stdout first,
so gating on the exit code loses nothing. Other commands exit `1` only when they
fail.

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
