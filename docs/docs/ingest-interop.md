---
sidebar_position: 13
---

# 摄取安全与 Vault 互操作

本文承载三组决策的实施细则：**解析与供给链安全**（D37–D39，2026-09 解析层
与供给链安全调查）与**文件监控 / vault 互操作 / source_key 规范**
（D40–D42，2026-09 文件监控与 vault 互操作调查）。决策记录见
[Architecture](architecture.md) §22。

一句话：**事件是提示，对账是真相；解析走隔离子进程；source_key 是 NFC
身份键，键层永不折叠。**

## 1. 解析层信任分级（D37）

### 默认层 = 同步子进程（(b) 档，Tauri sidecar 打包）

paper add 的 PDF 解析**从第一版就走子进程**，不做进程内起步。理由：canonical
库是不可重建的审计正史，而 abort 类风险（栈溢出 / OOM）在进程内**纯 Rust
也防不住**——下载未知来源 PDF 的供给面叠加这条风险，结构性崩溃隔离的价值
超过一次性限额代码成本。子进程即 [Python 与 Rust 协作边界](python-rust.md)
的 D18 接缝（版本化协议），不新增复杂度类。

1. **子进程内 OS 级资源帽**：Windows Job Objects（ProcessMemoryLimit +
   KILL_ON_JOB_CLOSE）；Linux RLIMIT_AS/CPU/FSIZE；macOS RLIMIT_CPU/FSIZE +
   父进程 watchdog 外杀（macOS 无 RLIMIT_AS、sandbox-exec 已废弃，进程级
   崩溃隔离仍等价成立）；
2. **默认层限纯 Rust crate**，禁 C/C++ FFI（mupdf/pdfium-render：AGPL +
   CVE 流 + SQLite 踩踏向量）；lopdf ≥ 0.42.0 且显式 `max_decompressed_size`
   （默认 None = 裸奔）；pdf-extract 按「panic 遍地」假设设防；
3. **入口硬 cap**：文件字节上限（≤100 MB）、页数上限、快扫对象数；
4. **来源记账（自建 MOTW）**：下载门记录 URL/域名/时间入库，为分级信任与
   崩溃归因留数据；
5. **解析严格在 SQLite 写事务之外**，结果一次性短事务落库——进程死 = 丢
   本篇 + 自动回滚，库完好；
6. **解析入口收敛为单一 trait 边界**（文件路径进、结构化结果出），升级
   （b)→(c)（常驻子进程，批量为常态或需并行时）是接缝后替换不是管线重写。

### 升级触发器

引入任何 FFI 后端 / 首次解析致 abort / 批量导入路径出现（单次 >10 篇）/
解析崩溃率 >0 → (b)；批量为常态或需并行 → (c) 常驻协议（版本化沿用 D18）。

## 2. Feed fetch/parse 五件套（D38）

1. **quick-xml ≥ 0.41.0**（RUSTSEC-2026-0194/0195 修复），CI audit/deny 兜底；
2. **scheme allowlist**：入口提前校验仅 http/https；reqwest 内建重定向每跳
   重校验；
3. **响应字节硬上限 10–15 MiB**：流式逐字节计数截断，无视 content-length，
   绝不把流直接交给解析器；
4. **总时间预算 20–30s**（`ClientBuilder::timeout()` 全程覆盖）；
5. **OPML 同管线**：每个 `xmlUrl` 过与手工添加完全相同的校验。

轻量必做：私网/环回字面量拒绝（含 169.254.169.254；DNS rebinding 残余风险
文档化）。选对库白得的防护（换库即失效，进评审清单）：XXE/billion laughs 在
quick-xml 生态结构性不成立；serde_json 默认 128 层递归上限挡深嵌套，不调用
`disable_recursion_limit`；避开 xml-rs 系（unmaintained）。

**不做**：https-only 强制、per-host 限流、DNS pinning（单用户桌面威胁模型）。

### 四态 adapter 的 reason-code 映射（§15 落地细则）

| 情形 | 四态 | reason code |
|---|---|---|
| scheme/私网拒绝、DNS/连接/TLS/超时/重定向/4xx/5xx/字节超限 | `upstream_error` | `invalid_source_url` / `dns_error` / `connect_error` / `tls_error` / `timeout` / `redirect_error` / `http_4xx` / `http_5xx` / `response_too_large` |
| 304 Not Modified | `success`（沿用上次） | `not_modified` |
| 200 但 XML/JSON 语法坏、根缺失、类型不识别、版本不支持 | `parse_error` | `malformed_xml` / `malformed_json` / `unknown_feed_type` / `unsupported_version` |
| 200 解析成功但 0 items | `no_results` | `empty_feed` |

限长 reader 产生的 IoError 必须拦截转记 `response_too_large`，不误归
parse_error。

## 3. cargo 供给链审计基线（D39）

1. **立即（零成本）**：workspace 全部 crate `publish = false`；Cargo.lock 入库；
   CI 全部 `--locked`；`.github/workflows` 的 `uses:` 全部 pin 完整 40 位 SHA
   （tj-actions CVE-2025-30066 实证 tag 可被重写）；
2. **CI 审计双时点**：`cargo deny --locked check`（licenses/bans/advisories/
   sources，**deny.toml 为唯一策略文件**，防 audit/deny 双 ignore 清单漂移）
   每个 PR 拦新增；main 每日 schedule 拦存量后被披露（arrayref 式时滞）；
3. **license 白名单**：MIT/Apache-2.0/BSD/ISC/0BSD/Unicode/CC0 起步；MPL-2.0
   桌面分发可放行；GPL/LGPL/AGPL 默认不放行；-sys crate 触发单条 allow 并记录
   理由。**白名单以 `deny.toml` 为准**；2026-09 按实际依赖图核对：图里出现的
   `Unlicense OR MIT` 与 `zlib-acknowledgement OR MIT` 都由 **MIT 分支**满足，
   不必为它们各开一条 allow。真要为后者开的话，ID 要写小写
   `zlib-acknowledgement`——cargo-deny 的 license 词表区分大小写，
   `Zlib-acknowledgement` 会直接报 unknown term；
4. **新增依赖验收八条**：维护活跃度（无维护热门 crate 是接管首选靶位）/
   下载量×stars 交叉 / 传递依赖数 / unsafe 与 -sys 辨识 / build.rs 行为 /
   可替代性（默认答案是不新增）/ 许可证 / 发布者身份；
5. **供给链资产纪律**：发布私钥只在发布 CI 受信 environment（日常 CI 永不
   接触）+ 离线冷备；发布产物附 build provenance 证明。

认知基线：crates.io 公告系统性少报——审计是「已知问题报警」不是「未知攻击
防御」；科研工具社区已被定向冒充（finch 事件）。

## 4. 文件监控一致性协议（D40）

**事件是提示，对账是真相**——事件永不 trust，全部事件归并为「重扫该路径」。

1. **选型**：`notify` 8.2.0 + `notify-debouncer-full`（稳定线；Windows RDCW
   溢出无信号的缺口由对账兜底，不冒险 RC）；backend 一律 RecommendedWatcher，
   异常整体降级 PollWatcher。不自研 watcher；
2. **重扫流程**：stat 与库比对（一致且非 racy 则丢弃假事件）→ 读全文（读
   前后双 stat，不一致 = 撕裂进重试队列）→ sha256 与库中 revision_hash 比对
   → 变了才重建 chunk。rename 降维为 delete+create 两条重扫；
3. **racy 强制内容校验（git racy-git 同构）**：stat 一致但 mtime ≥ 上次索引
   写入时刻 → 强制读内容比对；FAT/exFAT 卷最近 N 秒内有事件则放宽为内容
   比对；
4. **对账四触发**：启动全 vault walk+stat；每日定时；手动命令；watcher 异常
   恢复（IN_Q_OVERFLOW / FSEvents KernelDropped / 事件风暴）。对账成本锚点：
   10k 文件亚秒级；对账与事件流会合用 Watchman cookie 协议（sentinel 文件）；
5. **防抖与撕裂重试**：归并窗口 ~500ms；撕裂 = 指数退避 + mtime 稳定窗口
   （连续两次 stat 间隔 ≥50–100ms 相同），5 次后挂延迟队列（30s），仍失败
   留给每日对账——**绝不把撕裂内容落库**；
6. **黑名单**（事件层直接丢弃）：`.~lock.*#`、`~$*`、`*.swp`、`*~`、`*.tmp`、
   点开头目录内一切事件；
7. **索引事务边界**：单文件一次一致性更新 = 恰好一个 SQLite 事务（BEGIN
   IMMEDIATE）；事务首步复核 stat、不一致 ROLLBACK 重入队；chunk 以
   chunk_hash 内容寻址 upsert；提交前不再读文件；极端落旧 revision 由每日
   对账收敛——最终一致且 canonical 永不污染；
8. **网络盘立场**：维持 D31 的探测告警，不做 PollWatcher 降级实现、不承诺
   支持；文案明示网络盘不承诺实时。

## 5. vault 语法边界与建边矩阵（D41）

1. **索引范围**：白名单 = `*.md`（正文 + frontmatter 结构化剥离）、`*.canvas`
   （JSON Canvas）；附件层（图片/音视频/PDF）登记不进 FTS；`.base` 只记清单。
   黑名单 = **一切点开头目录与文件**，watcher 对黑名单静默；
2. **建边规则**：wikilink 全形态建边（含 heading/block/别名/markdown 标准链接
   等价）；`[[#heading]]` 自环不建边；embed 建边但 `edge_kind='embed'` 单列；
   frontmatter 内链建边但 `link_source='frontmatter'` 单列；tags 是分类语义
   不建边；**dangling 链接是一等公民**（target NULL + 保留 raw 串，目标日后
   创建时回填）；
3. **同名歧义三级规则**：含路径→精确匹配；裸名唯一→唯一匹配；多候选→同目录
   优先，仍并列则**不建边 + `ambiguous_wikilink` 事件**（比 Obsidian 更保守）；
4. **locator_json 三型**：`heading` / `block(^id)` / `span`；`raw` 字段全量
   保留原始锚点串（ingest 规则升级后可离线重放重建边）；锚点校验失败记
   `stale_anchor` 不静默丢边；
5. **frontmatter**：全量结构化解析（未知字段是 Obsidian 一等数据）；正文 FTS
   排除 frontmatter 块；解析失败 → 整块原文保留 + `frontmatter_parse_error`
   事件，正文照常索引；
6. **解析基座**：pulldown-cmark ≥ 0.13.3 + `ENABLE_WIKILINKS`（官方内置、
   Obsidian 兼容目标）；私有语法（Dendron 式）当 `unknown_link_syntax` 降级；
7. **降级事件十类**：frontmatter_parse_error / unresolved_wikilink /
   ambiguous_wikilink / stale_anchor / markdown_link_decode_error /
   unknown_link_syntax / excalidraw_detected / canvas_parse_error /
   symlink_encountered / attachment_unindexed——每条「没索引/没建边」的决定
   都留可审计事件，ingest 汇总报告回答「覆盖了多少、漏了什么、为什么」。

**不做**：MFT/USN Journal 方案（10k 规模复杂度收益比为负）；键层折叠或 NFKC；
改写用户 vault 文件（Foam 教训）；读 placeholder 内容；跟随符号链接（首版）。

## 6. source_key 规范与同步盘边界（D42）

1. **键构造**：`source_key = note:<vault-relative NFC path> | arxiv:<id>`。NFC/不折叠/碰撞规则作用于 scheme 之后的 tail。笔记 tail：大小写原样保留；分隔符统一 `/`——**这是指 walker 拼装组件时用 `/` 连接，不是对名字做字符替换**：`/` 之外的字节一律原样保留（名字里的 `\` 就是 `\`，`docs\ml\a.md` 与 `docs/ml/a.md` 是两个身份）；`..` 组件一律拒绝（键只接受已在 vault 内的相对组件序列，不做「剥离后重校验」）。**禁用 NFKC**（兼容分解会把不同文件折成同一键）。`local_path` 一律相对 workspace root；解析时归一化后必须仍落在工作区内，拒绝 symlink 逃逸（[ADR 0002](adr/source-key-is-scheme-prefixed.md) / [ADR 0024](adr/local-path-is-workspace-relative.md)）。verbatim 前缀与盘符大小写由解析时的 `canonicalize` 决定——既不是键的一部分，也不是我们做的改写；用户可见处渲染存储的 `local_path`。
2. **原始字节另存** `raw_name`（事件匹配、改名回写、显示用）；键只做身份；
3. **查询层折叠另建 fold 索引列**（ASCII-only 起步，unicase 完整折叠）——
   键层不做任何 casefold（Linux 大小写双写合法并存，折叠 = 数据丢失级事故）；
4. **碰撞策略**：NFC 键相同而原始字节不同 → 两条目都保留 + 消歧后缀 +
   `key_collision` 警告事件；绝不静默合并；
5. **同步盘预警**：OneDrive placeholder 检查属性位（RECALL_*）→ 只建元数据
   条目 + 事件，绝不默认读内容；iCloud `*.icloud` 跳过 + 事件；Windows 非法名
   （保留名/尾随空格点）在非 Windows 端照常索引但发 `windows_incompatible_name`
   预警（会经同步盘毒化 Windows 端）；长路径 manifest 声明 longPathAware +
   文档明示 LongPathsEnabled；
6. **平台测试矩阵（11 项进 CI）**：大小写双写/仅大小写改名/NFD/NFC+NFD
   双写/260 长路径/保留名与尾随字符/OneDrive placeholder/iCloud placeholder/
   三种路径表述同一键/折叠陷阱字符（İ、Σ、Maße、者/者）/exFAT 移动盘。

source_key 是主键，**P0 级正确**——现在定错以后改不动。
