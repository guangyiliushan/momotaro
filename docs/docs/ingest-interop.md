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

1. **选型**：`notify` 8.2.0 + `notify-debouncer-full` 0.7（稳定线；8.2.0 的
   Windows 后端不为 RDCW 溢出发 rescan 信号，缺口一律由对账兜底，不冒险 RC）；
   backend 一律 RecommendedWatcher。**降级只在后端起不来时发生**：构造失败、
   `MaxFilesWatch`、backend I/O 错误 ⇒ 换 PollWatcher 并播报「已降级为轮询，
   不再承诺实时」。**网络挂载不走这条**——见第 8 条。不自研 watcher；
2. **重扫流程**：stat 与库比对（一致且**非 racy** 则丢弃假事件）→ 读全文（读
   前后双 stat，不一致 = 撕裂进重试队列）→ sha256 与库中 revision_hash 比对
   → 变了才重建 chunk。**rename 降维为 delete+create 两条重扫**：delete 半程
   只确认路径已不存在，**不写数据**（键在历史里永不改写，[ADR 0014](adr/note-rename-is-a-new-source.md)）；
   create 半程提交**之后**，若防抖器把这对 From/To 配上了，追加一行
   `source_moved(old_key, new_key)` 提示（单独一个 rename batch），且绝不
   UPDATE 已存 `source_key`。**「某键从 walk 里消失了」是每一批都会重算的收敛**，
   不分 rename 与真删除：该键落一行 `skipped` + `source_removed`；同一批里
   `graph_links` 的剪除与派生索引中该源的文档**一并删掉**，不等下一次对账——
   等对账的是「事件流根本没看见」，不是「看见了却不改」；
3. **racy 强制内容校验（git racy-git 同构）**：stat 一致但 `mtime ≥ 上次写入
   store 的时刻` → 强制读内容比对。该时刻取**全库口径**（`store_events` 最新行的
   `recorded_at`），与 git 的 `is_racy_stat()` 用 index 文件自身 mtime 同构；
   **不是**该行的 `ingested_at`（那是每行的入索引时刻，会让同秒入索引的文件
   永久粘滞）。**racy 且比对相等 ⇒ 落一行 `skipped` 覆盖行**——等价
   `git update-index --refresh`，让 `recorded_at` 越过 mtime 自行解除 racy；
   非 racy 且 stat 一致仍按第 2 条静默丢弃。FAT/exFAT 卷（两秒刻度）最近有
   事件则同样放宽为内容比对——`gate` 只在事件到达时运行，所以「最近有事件」
   恒成立；卷粒度判定只在 Windows 上做，其余平台答「未知」、本条不适用。**不模仿
   git 的 size-smudge**：`metadata_json.size_bytes` 有下游读者，截断会污染
   locator 校验；
4. **对账四触发**：启动全 vault walk+stat；每日定时（24h，进程内）；手动
   `momotaro reconcile`；watcher 异常恢复——`Flag::Rescan`（覆盖 Linux
   `IN_Q_OVERFLOW` 与 FSEvents `KernelDropped`，notify 8.2 在两者上发出的是
   同一个 flag）**或**单个防抖批去重后路径数 > 256 的事件风暴。四者全部写
   `vault.reconcile.*`。对账成本（一次全量 walk+stat 的 no-op、release 构建、
   参考机实测）：1000 文件 ≈ 0.35 s、10000 文件 ≈ 2.9 s——这是**量级**锚点，
   不是「亚秒」。**对账与事件流会合用
   Watchman cookie 协议（sentinel 文件）**：同进程内有 watcher 时，reconcile
   先写 `.momotaro-reconcile-sentinel`、等 watcher 回报该路径再 walk、结束后
   删除；**无 watcher 则跳过**；**等待超时不使对账失败**——降级为「不带
   cookie 继续 walk」并**播报一行**（`cookie_skipped`，`reason`），让读者知道这一遍
   可能落后一个事件；
5. **防抖与撕裂重试**：归并窗口 ~500ms（`tick_rate` 默认为其 1/4）；撕裂 =
   指数退避 + mtime 稳定窗口（连续两次 stat 间隔 ≥50–100ms 相同），5 次后挂
   进程内延迟队列（30s），仍失败留给每日对账——**绝不把撕裂内容落库**；
   队列不落盘，进程退出即由四触发承接；
6. **黑名单**（事件层直接丢弃，先于一切重扫）：`.~lock.*#`、`~$*`、`*.swp`、
   `*~`、`*.tmp`、**一切点开头目录与文件**（与 walker 共用同一判定，索引范围
   逐字一致，否则同一个 vault 两套真相），**唯 `.icloud` 驱逐存根除外**——walker
   正是为了能报「那条笔记怎么了」才留下它（D42），watcher 不留它就是两套真相。
   sentinel 也是点开头文件，因而同样被本条丢掉；它作为**协议消息**是在本条
   **之前**、在原始事件流上被认出来的（第 4 条），不是被豁免；
7. **索引事务边界**：单文件一次一致性更新 = 恰好一个 SQLite 事务（BEGIN IMMEDIATE，
   `store::commit_source_file`：revision + chunks + 边同事务，事件由调用方选配；
   **coverage 行一律走批尾 sweep**（2026-09-29 起 watch 与批量同形——`stale_anchor`
   只能对落定状态裁决，多路径防抖批中途裁决会随行走序翻转），丢窗 = 批尾、对账承接；口径
   与代价见 [ADR 0026](adr/store-events-are-the-ingest-audit-log.md)）。**watcher（W2.5）用
   同一单元**：guard 闭包做事务首步 stat 复核、不一致拒绝 ⇒ ROLLBACK 重入队；chunk 以
   `chunk_id`（= `f(source_key, revision_hash, ordinal)` 的内容寻址主键）幂等替换、
   按 `(source_key, revision_hash)` 整体换版（`chunk_hash` 只是内容指纹）；提交前不再
   读文件；极端落旧 revision 由每日对账收敛——最终一致且 canonical 永不污染。
   **watcher 的事件是 `ingest.coverage`，批壳是 `ingest.started` / `ingest.finished`，
   域 = `ingest`**（每个防抖批一个 batch）——摄取就是摄取，读命令不必学会第二个域；
8. **网络盘立场**：维持 D31 的探测告警——**启动 watcher 与 `doctor` 时探测数据
   目录与 vault 根所在的卷**，Windows 上判 `GetDriveTypeW == DRIVE_REMOTE`，
   其余平台报「未知」。探测到网络挂载 ⇒ **不降级 PollWatcher**，只告警并明示
   「网络盘不承诺实时」；不承诺支持。文案同时说明 SQLite 在网络文件系统上
   WAL 不工作、锁不正确时可能损坏库。

## 5. vault 语法边界与建边矩阵（D41）

1. **索引范围**：**索引白名单** = `*.md`（正文 + frontmatter 结构化剥离）、`*.canvas`
   （JSON Canvas）；其余普通文件按**附件层**登记、不进 FTS（图片/音视频/PDF 等，`.base`
   同样只记清单——成员口径见第 10 条）。
   黑名单 = **一切点开头目录与文件**，watcher 对黑名单静默；
2. **建边规则**：wikilink 全形态建边（含 heading/block/别名/markdown 标准链接
   等价）；`[[#heading]]` 自环**不建边、不发事件**——自环是显式设计的非边，不是
   「没建成的边」；embed 建边但 `edge_kind='embed'` 单列——**附件 embed 同样
   建边**，`to_source_key` 是附件自己的源键（附件是 Source，embed 不因目标是
   附件而豁免）；frontmatter 内链建边但 `link_source='frontmatter'` 单列（扫描
   规则见第 5 条）；tags 是分类语义不建边；`obsidian://`、`mailto:` 与 `http(s)://` 外链
   不建边——外链判据是 **URI 形**（`scheme://…` 任意方案名与 `mailto:`；裸
   `arxiv:1234` 形的名字不是 URI，照常按名字解析成 dangling）；**dangling 链接是一等公民**（`to_source_key` NULL + `raw_target`
   保留原文，目标日后创建时随下一次 `index` 的重投影回填，见下）。**抽取与
   投影**：每条链接的 `raw_target`/`anchor_raw` 原文在解析期一次抽取，落该
   revision 的 `metadata_json` 的 **`links`** 键（记录形如
   `{"raw_target":…, "edge_kind":…, "link_source":…, "anchor_kind":…, "anchor_raw":…}`
   （`anchor_kind` 取 `"heading"` / `"block"` / `null`，`anchor_raw` 随之可空），
   记录序 = 文档序、同一身份只留首现）；
   每次 `index` 对每个走到的文件按**当前命名空间**解析抽取、整替换该文件的
   `graph_links` 行（行身份、替换粒度与 `created_at` 语义见
   [ADR 0027](adr/graph-links-rows-are-a-rebuilt-projection.md)）——删表重跑
   逐行一致；rename 后旧键的边在下次 `index` 如实变回 dangling
   （[ADR 0014](adr/note-rename-is-a-new-source.md)）；解析规则升级后可从存量
   抽取离线重放、不需重读 vault。canvas 建边（file 节点 / 连线 / text 节点
   内链）**本批不出生产者**：`canvas_card`/`canvas_edge` 两值随枚举闭合
   （[architecture](architecture.md) §8.2），生产者待 canvas 建边件；引用网络边（论文参考文献）留 v0.4 的
   paper add。**现状（2026-09-26）**：建边、dangling、投影与回填**已落**；
   canvas 建边与引用网络边**未做**。**迁移注记**：先于 `links` 键写入的行里
   没有抽取记录，指纹未变的文件不重写 blob（`skipped` 也不会重抽取）——那些
   文件的边要等内容变化或清库重建才建，`rm graph_links` 重跑对它们同样
   一致（重建出的行与存量行都不含它们）。
3. **同名歧义三级规则**：**含 `/` 的目标** → vault 相对路径精确匹配（命中 =
   源键 tail，即 vault 相对 NFC 路径，等于该串或该串 + `.md`——注意不是
   `local_path`，那是 workspace 相对的另一个坐标）；**裸名目标** → basename
   等于该串、或去扩展名后等于该串（笔记与附件同一规则）；多候选 → 同目录
   优先，仍并列则**不建边 + `ambiguous_wikilink` 事件**（比 Obsidian 更保守；
   该文件 coverage 行的 `data_json` 带 `ambiguous` 清单——每个未建成的链接记
   `raw_target` 与全部候选源键）。**markdown 标准链接同规则**：URL 解码后进
   同一解析，前导 `./` 去掉；含 `..` 的目标不解析成键（键层同口径），一律
   dangling。匹配比较按 NFC 规范化后进行、不做大小写折叠（D42 同口径）。
   **现状（2026-09-26）**：三级规则、同目录优先与候选清单载荷**已落**。
4. **locator 三型**：`heading` / `block(^id)` / `span` 是「定位」的共享词汇，
   分工不同——`heading`/`block` 是**链接锚点**两型（`graph_links.anchor_kind`
   的全部取值；无锚点时两列 NULL），`span`（行区间）是 **chunk 自身的定位**，
   即 `Chunk::locator_json` 已有的那一型，**不当锚点值使**。`anchor_raw` 全量
   保留原始锚点串（ingest 规则升级后可离线重放重建边）。`block` 的 `^id`
   字符集 `[A-Za-z0-9-]+`，三种定义形态（段末 / 独立行 / bullet 后）在解析期
   扫描建映射。**heading 是文本锚点非 ID**：`#H1#H2` 多级锚按 heading_path
   逐级文本定位、`anchor_raw` 保留原文；目标笔记内同名标题**取第一个出现**
   （本仓显式规则，不发事件）。锚点校验失败记 `stale_anchor` 不静默丢边
   （行保留）。**附件目标不做锚点解析**（PDF 页参一类片段不是锚点）：
   `anchor_raw` 原文保留、`anchor_kind` NULL、不校验、不发 `stale_anchor`。
   **现状（2026-09-26）**：两型锚点、校验与 `stale_anchor` **已落**；`span`
   先于本批已在 chunk 侧（`Chunk::locator_json`）。
5. **frontmatter**：全量结构化解析（未知字段是 Obsidian 一等数据）；正文 FTS
   排除 frontmatter 块。块的身份只认**第一行**、且必须有**闭合**：只有首行恰为
   `---`（行尾空白除外）且其后存在列 0 的 `---` 或 `...` 时才成块；首行之前有
   任何东西（BOM、缩进、空行、正文）或找不到闭合时，整篇即正文——那里的 `---`
   只是水平线，一个字都不吞（Obsidian 同此要求："Type `---` at the very
   beginning of a file."）。**只读第一个文档、不向前看**：块内第一个列 0 的
   `---` 之后一律是正文，后面的 `---` 不会改写块边界——YAML 允许多文档流
   （spec §9.1），我们取第一个；`...` 作闭合是我们的**扩展**（Obsidian 未定义）。
   失败降到**字段级**：块里某个字段的形状没被建模（`|`/`>` 块标量、嵌套 map、
   混着续行的列表）→ **该字段**按源行保留（`Raw`），其余字段照常解析；整块
   失败只留给块本身读不出来的情况（既不是 `key: value`、也不是任何字段的续行；
   **无字段可属**的列表项；空字段名）——那时整块原文保留 + `frontmatter_parse_error`
   事件，正文照常索引；**未建模 ≠ 猜**：带引号的键头（`: ` 在闭引号之后，§7.4.2）、
   除简单一行 `[a, b]` 以外的花括号/嵌套/引号内含逗号的值、`|`/`>` 标量头下的
   任何行（含 `- ` 开头的，它们是内容，§8.1.2）——一律落到**该字段**的 `Raw`，
   原文逐字保留，下游按 `Value` 形状分支即可，不要去猜。**JSON 属性块是第三种形态**：
   Obsidian 官方文档明确支持（"you can also define properties using JSON … read,
   interpreted, and saved as YAML"），所以首行为 `{` 的块按 JSON 建模 —— ingest 只认
   形状、原文交出去（本 crate 不引 JSON 依赖），`run` 用已有解析器读成与 YAML 相同的
   字段形状（`state: "parsed"` + `source: "json"`）；读不出来的 JSON 块与读不出来的
   YAML 块走**同一条失败路径**（同码、同状态），下游不必学第二套词汇；两道 `---` 夹住的散文正是这种形状：整块 `Failed`、
   **不进 FTS**（也不进 title/heading_path），原块连行号与原因留在
   `metadata_json.frontmatter` 的 `raw`/`line`/`reason`，重解析即恢复（`reason`
   是**稳定码** `not_a_field` / `orphan_list_item` / `empty_field_name`——下游要分支
   就读码，不要去匹配句子。那句话随码落盘一同删除；原文块仍在 `raw` 里）。**迁移注记**：早于该码写入
   的行里 `reason` 仍是句子；指纹未变的文件不会重写 blob（`skipped`），
   `rebuild-index` 也只重建 tantivy，所以那些行会一直带着旧词表——重解析即恢复，
   今天没有读者，故不做迁移。
   **frontmatter 内链一律从 `raw` 扫描**（字段形状未建模也不会漏链）：`raw` 在
   `Parsed` 与 `Failed` 两条路径都保留，扫描两路都生效；只扫**非注释行**
   （列 0 `#` 开头是 YAML 注释，其中的 `[[x]]` 不建边）；块标量（`|`/`>`）
   内容按 YAML 语义是**内容**，其中的 `[[x]]` **建边**。
   **`metadata_json` 的键形状**（下游按形状分支，勿按散文匹配）：
   `frontmatter` 四态——`{"state":"absent"}`（无块）；`{"state":"parsed", …}`
   带 `"raw"`（块原文）与 `"fields":[{"key":…,"value":…},…]`——`value` 是
   `{"scalar":…}` / `{"list":[…]}` / `{"raw":…}` 三型之一（YAML；JSON 属性块
   同形、另带 `"source":"json"`）；`{"state":"failed", …}` 带 `"raw"` /
   `"line"` / `"reason"`（读不出来的 JSON 块同态、`line` 恒 1）。`fields` 序 =
   源序（JSON 属性块按键序）、重复键全
   保留，`value` 三型与 `Value` 枚举同构。`links` 键见第 2 条（存量记录里本 build 不认识的拼写整条跳过并按
   `unknown_link_syntax` 记损失——降级窗口里也不许无码消失）；`link_losses`
   键 = 抽取期损失的**去重码表**（声明序，`markdown_link_decode_error` /
   `unknown_link_syntax` 两类事实随行重投影，`skipped` 行不丢码）；`outline`
   键 = `{"headings":[[<级别文本>…],…],"block_ids":[…]}`——锚点校验与离线
   重放的依据（每个标题一条路径 = 它入栈后的栈快照；`block_ids` 为行末
   `^id` 定义）。指纹刷新整块重写 `metadata_json`，各键都由调用方带过
   （刷新语义 = 内容未变）。
   `chunks.text` 仍是原文切片，locator 必须把切掉的字节/行加回（见
   [ADR 0026](adr/store-events-are-the-ingest-audit-log.md) 同层的口径）——
   注意解析基座不这么看（CommonMark 把 `---`+散文+`---` 读成 hr + 第二道 `---`
   作散文的 setext H2 下划线），所以**剥离必须先于解析**：先切块、再 desugar、
   建 index；
   **现状（2026-09-26）**：解析、接线与排除**已落地**（字段进
   `source_revisions.metadata_json` 的 `frontmatter` 键、正文进 chunker 且
   locator 按原 note 计、失败发 `frontmatter_parse_error` 且 `outcome` 仍为
   `indexed`）；wikilink 五类降级**已随 W2.4 建边落地**（第 2–4 条）；同步盘
   三条（OneDrive/iCloud/Windows 非法名）**已落**，是 `Warning` 而非十类之一
   （口径见 §6 第 5 条与第 8 条 ⑤）；十类**全部有生产者**：
   `frontmatter_parse_error`（本条）、五类 wikilink 码（第 2–4 条）、
   `excalidraw_detected`/`canvas_parse_error`（第 9 条）、
   `symlink_encountered`/`attachment_unindexed`（第 10 条）；
6. **解析基座**：pulldown-cmark ≥ 0.13.3 + `ENABLE_WIKILINKS`（官方内置、
   Obsidian 兼容目标）；私有语法（Dendron 式）当 `unknown_link_syntax` 降级——
   **触发面 = 正文里「长得像链接但解析器没成链」的残迹**（空锚 `[[#^]]`、
   只有别名的 `[[|x]]` 这类），按纯文本保留并发码；锚在了链上但形态非法
   （`^` 后不是 `[A-Za-z0-9-]+`）同样归它；**code span / 代码围栏里的
   `[[x]]` 不算**：那是有意的代码文本，不是疑似链接，不发码。
   **现状（2026-09-26）**：全部解析点已开 `ENABLE_WIKILINKS`（chunker、title
   与 links 的抽取/轮廓两处，共四处；黄金集复测逐位未动——夹具内 `[[` 为 0
   次，属夹具巧合 + 按构造不变）；
   `unknown_link_syntax` 生产者**已落**。
7. **降级事件十类**：frontmatter_parse_error / unresolved_wikilink /
   ambiguous_wikilink / stale_anchor / markdown_link_decode_error /
   unknown_link_syntax / excalidraw_detected / canvas_parse_error /
   symlink_encountered / attachment_unindexed——每条「没索引/没建边」的决定
   都留可审计事件，ingest 汇总报告回答「覆盖了多少、漏了什么、为什么」。
   **五类 wikilink 码的行为空间**（生产者随 W2.4 落地）：`unresolved_wikilink`
   = dangling 行照建 + 码；`ambiguous_wikilink` = **不建行** + 码（候选清单
   进 `data_json`）；`stale_anchor` = 行保留 + 码；`markdown_link_decode_error`
   = **不建行** + 码（该链接按纯文本入 FTS）；`unknown_link_syntax` =
   **不建行** + 码（原文本保留）。**现状（2026-09-26）**：十类全部有生产者，
   落点见第 2–6 条与第 9/10 条。
8. **一个文件可以同时丢好几样**（降级槽的形状，2026-09-20 定）：W2.4 起，
   「三条悬挂 + 一条歧义」是常态，所以**不许**用单槽表示。取值形状如下——
   ① `FileOutcome::Indexed` 携带**一个列表**（`degradations: Vec<Degradation>`），
   顺序 = 枚举声明顺序（确定性，重跑同结果）；② coverage 行的
   `reason_code` 是这份列表的**第一个**（主码，既有读者与计数语义都不动）；
   ③ **完整列表进同一行的 `data_json`**（`{"degradations":[…]}`）——
   `data_json` 列已存在，**不需要 schema 迁移**；④ `CoverageReport` 按码**计数**
   暴露（`doctor --coverage` 因此不再把带码的行只算作 `indexed` 而把码吞掉）。
   ⑤ **预警与降级是两个词表、同一行**（2026-09-24）：同步盘三条（`onedrive_placeholder` /
   `icloud_placeholder` / `windows_incompatible_name`）是 `Warning` 而不是
   `Degradation`——十类仍是十类（预警不是「丢了什么」，是「换一台平台活不下去」）；词表里
   还有第四个 `Warning` 即 `source_removed`，但它与同步盘无关，是 D40 §4.2 的 delete 半边。
   承载与降级同形：`reason_code` = 降级的首个，**无降级时才轮到**预警的首个（两份列表都按各自枚举的声明顺序——确定性，重跑同结果，首个即行主码）；
   `data_json` 增 `"warnings"` 键、非法名行另有 `"windows_incompatible_names"` 键
   （各自非空才写）。读者（`doctor --coverage` 与任何读行的人）必须**先查两个词表再判未知**——
   已知预警**不是** failure；两个词表都不认识的码照旧计入 failures。
   `CoverageReport.warnings` 按码 × 行状态计数（与 `degradations` 同形），
   `doctor --coverage` 两个清单都渲染。
   两条被否的替代：**每码一行**（破坏「每个走到的文件恰好一行」的计数不变量）
   与 **`reason_code` 里拼分隔串**（列是稳定码，编码串会诱使下游做字符串匹配）。
   **现状（2026-09-26）**：槽形状先于本批已落；五类 wikilink 码走同一槽、无新
   形状（一文件多码共现按本条既有规则：`reason_code` 取首个、`data_json` 全表）。
9. **`.canvas` 的文本口径，与 Excalidraw 场景段（2026-09-21 定，随 W2.3 落地）**：
   `.canvas` 是 JSON Canvas 1.0（顶层只有 `nodes`/`edges` 两个数组，规范里**两个都是
   optional**），进索引的只有规范**赋予了文本语义**的字段——`text` 节点取 `text`
   （逐字；规范说它是 Markdown）、`file` 节点取 `file` 且 `subpath` 存在时**直接拼接**
   （`readme.md#Heading`，规范保证 subpath 以 `#` 开头）、`link` 节点取 `url`、`group`
   节点**只在 `label` 存在时**输出该段（缺省即无段，**不产占位串**）。**不进文本**：
   `id`（随机 hex，且只在单个画布内唯一）、`type`、`x`/`y`/`width`/`height`、
   `color`/`background`/`backgroundStyle`、`fromSide`/`toSide`/`fromEnd`/`toEnd`——
   规范对颜色语义**故意不定义**、位置尺寸只是像素，任何由它们派生的 token 按构造就是
   噪声。顺序 = `nodes` 数组序（规范里数组序即升序 z-index）后接 `edges` 数组序；段间
   以空行分隔（`chunks.text` 里见到的多余换行来自 chunker 打包的既有行为，不是自造
   串），**不插自造分隔串**（`---` 会被 frontmatter/标题语义吃掉）。**边**输出一行
   `<from 节点的文本> -> <to 节点的文本>`，`label` 存在时追加 `: <label>`；
   `fromNode`/`toNode` 指向的节点没产出文本时**保留该 id 字面**，不因一条悬空边判死整
   文件。**空画布**（`nodes: []`、无 `edges`、或两键都缺）产出**零段且不是失败**。
   **失败只有一种**：这份文本**读不成一个 JSON Canvas 对象**——不是 JSON、顶层不是
   对象，或 `nodes`/`edges` 存在却不是数组（规范对非法 JSON / 缺字段 / 未知 `type`
   **零规定**：全文对 invalid/unknown/reject/fail 0 匹配，也没有 schema）——那时整文件
   原文照常进 `chunks`、`outcome` 仍 `indexed`、行上带 `canvas_parse_error`；单节点缺
   `type` 或类型未知只跳过**该节点**，几何字段缺失不影响取文本；`{}`（空对象）与
   `{"nodes": []}` 不是失败，是零段。**读 JSON 的层**：
   ingest 只认**形状**（扩展名与形状识别），JSON 解析在 `run`——本 crate 不引 JSON
   依赖，与 JSON 属性块同一条口径；源键仍是 `note:`（不新增 scheme），`SourceKind`
   仍是 `Note`（不扩枚举）。
   **Excalidraw 场景段不进索引**：`.excalidraw.md`（插件默认也写裸 `.md`）把整幅图存成
   一个围栏代码块（压缩态 info string 为 `compressed-json`、非压缩态为 `json`，整段被
   `%%` 包住——`%%` 只是渲染层隐藏，**字节全在**），今天它会被当正文切块并进 FTS。
   判据**照 Excalidraw 插件（上游 excalidraw/excalidraw 仓库，不在本仓）自己的正则**：
   插件源码里 `shared/excalidrawMarkdownParsing.ts` 的 `DRAWING_COMPRESSED_REG` /
   `DRAWING_REG`。该文件属上游项目、逐字文本在仓库外，本仓未引，故这里写的是**口径**
   而不是逐字照抄：段头 `##? Drawing` 必须在**列 0 且其后无别的字符**
   （正则锚在 `\n##? Drawing\n`），段头与围栏之间允许任意**非反引号**字符（正则里的
   `[^`]*`）——空行、散文、`%%` 都不会让判据失效；**第一个**带反引号的行决定其余：它的
   info string 为 `compressed-json` 或 `json` 才算命中（别的语言在这里就不是它的布局，
   句子中间的行内反引号同样如此）。排除区间 = 该围栏的整段（含起止两行），粒度是**块**：
   与该区间**相交**的块整块丢弃（插件布局下那个块就是围栏本身），其余内容一个字节都不动、
   locator 不做位移（Markdown 路径下 `chunks.text` 仍是原文切片）。`## Text Elements` 段
   与其余正文**照常索引**（真正的可索引文本在那儿；不解压 LZString）。我们知道的一处
   **放宽**：不要求笔记带 `%%` / `# Excalidraw Data` 前言。**已知边界**：手写笔记里若真有
   一个列 0 的 `## Drawing` 标题紧跟 `json` 围栏，我们与插件的真实布局**分不出来**；别的
   标题下的 `json` 块不受影响。命中时该文件带 `excalidraw_detected`。
   **迁移注记**：本次落地之前索引过的 Excalidraw 笔记，场景字节仍在 canonical `chunks` 里，
   而**没有任何路径会替旧库重切一遍**：指纹未变的文件报 `skipped`（不读文件、也就没有码）；
   `touch` 后重跑 `index` 走的是 `metadata_refreshed`——**只刷指纹，不重切、不替换 chunks、
   也不碰 tantivy**，所以场景同时留在 canonical **与搜索结果里**；`rebuild-index` 更会把它
   从 canonical 重新灌进索引（那一行只说本次运行做了什么）。要让场景真正出库，只能让文件的
   **内容**变化（改一下再存回）或清库重建。**仪器**：这一面目前**没有带外仪器**（黄金集夹具里没有 `.canvas` 与
   `.excalidraw` 笔记）——只有 `cargo test` 守着它。
   **`.canvas` 的 locator**：渲染文本不是文件的字节切片，所以它的 chunk 指向**整份画布
   文件**（`start_byte = 0`、坐标同样是**归一化文本**的坐标，与全仓一致）——引用仍能落到
   文档本身；节点级精度是后续任务。

10. **附件登记与符号链接（2026-09-22 定，随 W2.3 单位 B 落地）**：白名单的**第三类**是
    附件。清单的参照是 **Obsidian 官方《Accepted file formats》**（Markdown / Bases `.base` /
    JSON Canvas / Images `.avif .bmp .gif .jpeg .jpg .png .svg .webp` / Audio `.flac .m4a
    .mp3 .ogg .wav .webm .3gp` / Video `.mkv .mov .mp4 .ogv .webm` / PDF `.pdf`）——它是唯一
    给出这些扩展名的官方来源（IANA 只登记媒体类型，不认 `.md`/`.canvas`/`.base`）。**实现口径比
    那份清单宽一格**：walker 把**每一个不是 `.md`/`.canvas` 的普通文件**归为附件
    （`VaultFileKind::Attachment`）——读不了的文件仍是 vault 里的事实，D41 的「每条『没索引』的
    决定都留可审计事件」对**walker 走到的每个条目**成立（列不出来的目录里有什么，今天仍答不出来，
    见剩余面）；官方清单在这里是**描述**（附件层覆盖什么），不是封闭集合。
    **登记写法**：复用 `source_revisions`（`Source → Revision → Chunk` 的第三段为空，不另立表）——
    `kind = SourceKind::Attachment`（`kind` 列是 TEXT、没有 CHECK ⇒ **零迁移**）、
    `origin_class = owner`、`metadata_json` = 指纹（`mtime_ms`/`size_bytes`/`tool`）、**0 chunk**
    （`chunks` 与 Tantivy 都不写）、`local_path`/`raw_name` 与笔记同形。`revision_hash` 走
    `revision_hash_reader`：**流式**哈希原始字节、同一个 `REVISION_DOMAIN`（二进制不过
    `normalize_markdown`、不整读进内存）——同一文件的两个字节版本因此仍是两个 revision。指纹
    不变的重复 `index` 与笔记一样报 `skipped`；指纹变了而字节没变（例如 `touch`）报
    `metadata_refreshed` 并把指纹刷进库；字节变了则是一条**新 revision**。**coverage 行**：**码随行状态走**——首次登记（写下了 revision）是
    `indexed`，只刷指纹的是 `metadata_refreshed`，未变的重复 `index` 是 `skipped`，三种行都带
    `reason_code = attachment_unindexed`（十类之一）：「不进 FTS」由码说明，「这次读了没有」
    由行状态说明。**符号链接（含 Windows junction）不跟随**：`file_type().is_symlink()` 在**发现层**
    分类（`VaultFileKind::Symlink`），walk 不下降进链接目录、**不读目标内容、不写 revision**；
    它的行是 `outcome = skipped` + `reason_code = symlink_encountered`，而 **`skipped` 行同样
    计数降级码**（只数 `indexed` 行的读者会把链接的码丢掉）。目标在 vault 内时按它自己的真实
    路径索引一次——链接是路径，不是第二份内容。**`.base` 与裸 `.excalidraw`** 同样落附件层
    （前者是官方 Bases 行，后者是插件私有格式）；将来有自己的 reader 再分出去。**`vault` 等于
    工作区根时**，`momotaro.toml` 自己也在 vault 里 ⇒ 同样被登记为附件（诚实描述 vault 的内容；
    若将来要把附件列给用户看，再单独决定是否排除工作区自己的配置文件）。**仪器**：这一面只有
    `cargo test`（黄金集夹具里没有附件）。

**不做**：MFT/USN Journal 方案（10k 规模复杂度收益比为负）；键层折叠或 NFKC；
改写用户 vault 文件（Foam 教训）；读 placeholder 内容；跟随符号链接（首版）。

## 6. source_key 规范与同步盘边界（D42）

1. **键构造**：`source_key = note:<vault-relative NFC path> | arxiv:<id>`。NFC/不折叠/碰撞规则作用于 scheme 之后的 tail。笔记 tail：大小写原样保留；分隔符统一 `/`——**这是指 walker 拼装组件时用 `/` 连接，不是对名字做字符替换**：`/` 之外的字节一律原样保留（名字里的 `\` 就是 `\`，`docs\ml\a.md` 与 `docs/ml/a.md` 是两个身份）；`..` 组件一律拒绝（键只接受已在 vault 内的相对组件序列，不做「剥离后重校验」）。**禁用 NFKC**（兼容分解会把不同文件折成同一键）。`local_path` 一律相对 workspace root；解析时归一化后必须仍落在工作区内，拒绝 symlink 逃逸（[ADR 0002](adr/source-key-is-scheme-prefixed.md) / [ADR 0024](adr/local-path-is-workspace-relative.md)）。verbatim 前缀与盘符大小写由解析时的 `canonicalize` 决定——既不是键的一部分，也不是我们做的改写；用户可见处渲染存储的 `local_path`。
2. **原始字节另存** `raw_name`（事件匹配、改名回写、显示用）；键只做身份；
3. **查询层折叠另建 fold 索引列**（ASCII-only 起步，unicase 完整折叠）——
   键层不做任何 casefold（Linux 大小写双写合法并存，折叠 = 数据丢失级事故）；
4. **碰撞策略**（D42 目标口径；**现状（2026-09-29）**：消歧后缀未落地——第二个请求者被拒为
   `outcome=error` + `reason_code=key_collision`，是 [ADR 0002](adr/source-key-is-scheme-prefixed.md) /
   [ADR 0026](adr/store-events-are-the-ingest-audit-log.md) 如实标注的临时护栏）：NFC 键相同而原始字节不同 →
   两条目都保留 + 消歧后缀 + `key_collision` 事件；**「绝不静默合并」只作用于身份键**（内容寻址的
   revision 集、同策略幂等替换的 chunk 集（可含被引用保留的历史行）与 `graph_links` 行集允许静默并——
   [ADR 0027](adr/graph-links-rows-are-a-rebuilt-projection.md)）；
5. **同步盘预警（D42 三条，词表 `Warning`）**：
   - **OneDrive placeholder**：判据**只认** `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`——
     官方要求所有 Cloud Files 占位必须置位；`OFFLINE`（普通文件上也能置位，实测）、
     `PINNED`/`UNPINNED`（用户意图）、`REPARSE_POINT`（链接）一律**不作判据**；
     `RECALL_ON_OPEN` 只在列目录类查询里出现、句柄读看不到，故不进判据。检查点在
     `fs::metadata`（不读内容）之后、一切读取之前：命中即**不读内容、不建 revision**
     （内容不可寻址时没有诚实的散列），行 = `skipped` + `onedrive_placeholder`；本地化后
     下一次 `index` 正常读入——每次运行重查属性位，状态翻转立刻跟上（行本身就是那条
     元数据条目）。
   - **iCloud 驱逐占位**：判据**两端锚定**（`starts_with('.') && ends_with(".icloud")`）；
     `x.icloud.md`、`.x.icloud.md` 这类名字里含 `.icloud` 的真笔记不受影响。点文件本就
     不入 vault，这条只把占位**报出来**：行 = `skipped` + `icloud_placeholder`，其内容
     永不进 FTS。
   - **Windows 非法名**：判据**逐组件**——第一个 `.` 之前的第一段与 28 个保留名
     （CON/PRN/AUX/NUL、COM1–9/LPT1–9、COM¹²³/LPT¹²³；ASCII 不敏感）**全等**
     （`constitution.md`/`com10.md`/`aux2.md` 合法）、出现保留字符 `<` `>` `:` `"` `/`
     `\` `|` `?` `*` 或 NUL/1–31 控制字符、末字符是点或空格。命中即发
     `windows_incompatible_name`（`outcome` 不变、照常索引，命中组件列表随行）；判据
     **与平台无关**（文档规则正是 Explorer 与同步端的实际约束——`\\?\` 路径能建这些名字不代表同步过去还能活；任何平台都如实预警）。
   - 长路径 manifest 声明 longPathAware + 文档明示 LongPathsEnabled；
6. **平台测试矩阵（11 项；随 W2.6 的 `path_matrix.rs` 进 CI）**：大小写双写/仅大小写改名/NFD/NFC+NFD
   双写/260 长路径/保留名与尾随字符/OneDrive placeholder（**真样本只能手工一次**——该属性位
   只有内核/云引擎能置，CI 造不出；仓里是 `#[ignore]` 的手工测试）/iCloud placeholder/
   三种路径表述同一键/折叠陷阱字符（İ、Σ、Maße、者/者）/exFAT 移动盘。

source_key 是主键，**P0 级正确**——现在定错以后改不动。
