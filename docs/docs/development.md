---
sidebar_position: 4
---

# Development

## Setup

仓库同时有两套工作区，职责分开：

```bash
pnpm install
pnpm turbo run test --filter=@momotaro/cli
cargo test --workspace
```

| 层 | 清单 | 职责 |
|---|---|---|
| pnpm + Turborepo | `package.json` `pnpm-workspace.yaml` `turbo.json` | JS 应用、文档、用 scripts 拉起 Rust/Tauri |
| Cargo workspace | 根 `Cargo.toml` `Cargo.lock` | 全部 Rust crate 与二进制 |

不要用 Turbo 的实验性 Cargo 原生工作区当地基（`experimentalCargoWorkspaces` 随时会变）。Rust 依赖图以 Cargo 为准；每个**可运行面**带一个薄 `package.json`，scripts 里调用 `cargo` / `tauri`。这是 [Turborepo 多语言指南](https://turborepo.dev/docs/guides/multi-language) 对「尚无稳定原生支持的语言」的官方做法。

## 依赖面

刻意保持小：

1. Rust（`rust-toolchain.toml` 钉版本）；
2. SQLite（关系事实、WAL、事务）；
3. Tantivy（BM25 倒排，派生索引）；
4. clap + 终端渲染（CLI）；
5. 一个 PDF 解析库（crate 级选型，不在 UI 里解析）；
6. OpenAI-compatible HTTP（唯一 LLM 协议）；
7. Tauri 2 + 现有 Vite/React（桌面）；
8. 引用校验在核心；CAS / SymPy 级数学验证后置。

避免 LangChain、LlamaIndex、通用 plugin loader、LangGraph、把 Python 运行时塞进核心。

## 目录边界

```text
momotaro/
  apps/
    cli/                 # @momotaro/cli：package.json shim + Cargo 二进制
    desktop/             # @momotaro/desktop：产品 UI；src-tauri 无 package.json
    mobile/              # 后置，与 desktop 同构
    www/                 # 后置，GSAP 独立站，不跑引擎
  packages/
    ui/                  # 设计系统
    app-ui/              # 桌面/移动共用屏幕（库，不是应用）
    ipc/                 # TS 对 Rust contracts 的投影
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
    momotaro-run/        # search / ask / trace 的唯一服务面
  docs/                  # Docusaurus 工程；站点内容根 = docs/docs/（只有这里被路由）
    docs/adr/            # 决策记录（命名规范见「文档规范」）
  CONTEXT.md             # 域名词表；不上站
  {workspace}/.momotaro/papers/   # paper add 的 canonical 字节
```

`pnpm-workspace.yaml` 只包含 `apps/*`、`packages/*`、`docs`。不要把 `crates/` 或 `apps/desktop/src-tauri` 登记成 pnpm package。Turbo **不支持嵌套 package**：`apps/desktop` 和 `apps/desktop/src-tauri` 不能都有 `package.json`。

根 `Cargo.toml` 的 members：`crates/*`、`apps/cli`、`apps/desktop/src-tauri`。

## 硬性规则

1. `momotaro-contracts` 无 IO，不 import rusqlite / reqwest / tauri / clap。
2. `apps/cli` 只解析参数和渲染输出。
3. `momotaro-llm` 不知道 SQLite、vault、citation。
4. `momotaro-retrieve` 不渲染 prompt。
5. `momotaro-feeds` 只产生候选，不写 library。
6. 桌面 / 移动 / 未来 HTTP 面只能调用 `momotaro-run` 暴露的服务函数。
7. UI 不直连 SQLite，不拼 prompt，不执行 policy。
8. 跨 crate 依赖必须在各自 `Cargo.toml` 显式声明。

## Canonical versus derived

分类以 [Architecture](architecture.md) §8.1/§8.2 为准，本文件不复制清单：

- canonical：sessions、projects、source_revisions、annotations、sparks、runs、run_events、history_items、retrievals、retrieval_hits、run_context_items、tool_calls、llm_turns、answers、answer_citations、verifications、feed_sources、feed_items；
- 第三态（保留型投影）：chunks —— 见 [ADR 0022](adr/chunks-are-a-retained-projection.md)，既不是 canonical 也不是可随手丢弃的 derived；
- derived：Tantivy 段、embeddings / BM42 / SPLADE、graph_links、graph_layout、usage_projections。

## 启动

```bash
pnpm turbo run dev --filter=@momotaro/cli
pnpm turbo run dev --filter=@momotaro/desktop
pnpm --filter docs start
cargo test --workspace
```

CLI shim 示例（业务不写在 package.json 里）：

```json
{
  "name": "@momotaro/cli",
  "private": true,
  "scripts": {
    "dev": "cargo run -p momotaro-cli --",
    "build": "cargo build -p momotaro-cli --release",
    "test": "cargo test -p momotaro-cli --locked"
  }
}
```

## CI

`.github/workflows/rust.yml` 三个 job（`uses:` 全部 pin 完整 40 位 SHA，D39）：

| job | 触发 | 内容 |
|---|---|---|
| `test` | push / PR（Rust 路径、`tools/evals/**` 或 workflow 自身改动）+ 每日 | ubuntu 与 windows 两腿：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`。**windows 腿串行**（`RUST_TEST_THREADS=1`）：并行建索引在该平台会瞬时拒绝访问（实测并行 5/40 与 2/12 红、串行 0/11）——这是**换稳定，不是修因**，flaky 本体仍登记在册 |
| `deny` | 同上 | `cargo deny --locked check`（`deny.toml` 是唯一策略文件：licenses / bans / advisories / sources；特性覆盖来自 `[graph] all-features`，不是 CLI flag） |
| `instruments` | 同上 | `perf_smoke`（1000 文件性能冒烟，`#[ignore]` 那条）与 `bash tools/evals/run_golden.sh`（golden set 回归绊线）。**配方与门槛只有这一份**：README 教的就是它、CI 调的就是它，门槛写在脚本里（贴基线，仪器确定性 ⇒ 不是容差；golden set 扩容时显式重设） |

`test` 之外的两位是「`cargo test` 看不见的东西」：冒烟与 golden set 原先只在本地跑，
检索排序改动可以一路绿到合入。**冒烟不设时间阈值**——它只断言跑通与计数，时间门在
共享 runner 上会抖；要卡性能得用同机基准。

文档站由 `docs-test.yml`（push→dev / PR→main 构建，断链 throw）与
`docs-deploy.yml`（push main 发布）负责。

## Beta definition of done

更宽测试前，至少：

1. `momotaro index` 能索引 fixture vault；
2. `momotaro search` 无 API key 可用，且不消耗 token；
3. `momotaro ask` 返回带引用答案；
4. `momotaro trace` 能回放 run；
5. 无效引用被显式标记失败；
6. 索引重建不改 canonical run 历史；
7. fake-LLM 集成测试通过。

Project / Annotation / Spark 的契约在 v0.5–v0.7 补齐，不阻塞上述引擎闭环。

## Desktop / mobile

`1.0.0` 默认桌面是 Tauri 2，前端复用 `apps/desktop` 的 Vite/React，Rust 核心通过 Tauri command 调用 `momotaro-run`。

GUI 必须：

1. 与 CLI 调用同一服务层；
2. 不直连 SQLite；
3. 不在 UI 里组装 prompt；
4. 覆盖库浏览、search、ask、citation 跳转、trace、项目与批注；
5. 长任务在后台跑，可取消、可看进度。

移动端是后置伴侣：读库、提问、批注、收 spark。不做全库 ingest、不做 feed 轮询、不做 LaTeX。

## 文档规范（命名 · 排序 · 路由）

站点内容根是 `docs/docs/`。只有这里的 `.md` 会被路由；仓库根 `CONTEXT.md` 是域名词表，不走站点路由。

1. 文件名：全小写 kebab-case、ASCII、`.md`。禁止数字前缀、日期前缀、`_` 前缀（`_` 会被 Docusaurus 当 partial 忽略）、驼峰与空格。一文件一主题。
2. 排序：`sidebar_position`（文档）或 `_category_.yml: position`（目录）。文件系统顺序不承载任何语义；数字只存在于 front matter，不存在于文件名。
3. URL：默认等于文件名。只有「文件名与期望 URL 不一致」时才写 `slug`（当前仅 `index.md` 的 `slug: /`）。
4. 改名：改名即改 URL。真需要改名时，先加 `slug` 锁住旧 URL，再补 `@docusaurus/plugin-client-redirects` 规则。
5. 交叉引用：文档间一律相对 Markdown 链接。被外部引用的标题加显式 id（本站 Markdown 走 MDX，用 `{/* #id */}`）。
6. ADR：文件名 = 决策标题的 kebab-case（无数字）；编号（`D42` / `ADR 0015`）留在正文 H1 与 front matter（`sidebar_label`），不进文件名。一个决策一个文件；被取代的决策**就地改写并标注 Superseded**，不新开文件。
7. 一个决策只有一个身份：`D<n>` 是站点内的引用键，ADR 文件是它的详述。[Architecture 决策记录](architecture.md#decisions) 每条被修订的 D 链到对应 ADR；ADR 写明它承接/修订的 D 编号。
8. 校验：`onBrokenMarkdownLinks` 为 `throw`。
