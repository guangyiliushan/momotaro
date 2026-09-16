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

## Policy flags

```bash
--no-llm
--allow-network
--allow-write
--max-context-tokens <n>
--mode lexical|expand
```

Beta 默认保守：未显式允许时，网络和写文件关闭。移动端默认使用更小的 context budget。
