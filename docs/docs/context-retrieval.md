---
sidebar_position: 9
---

# 上下文工程与检索工程

本文补齐 [Architecture](architecture.md) §11/§12 未展开的工程规则。Context pipeline
与 retrieval 是两个可独立演进的子系统，契约面是 `retrieval_hits`：检索可以整体升级
（BM25 → hybrid → dense），只要它仍然产出带 `source_key + revision_hash + chunk_id +
chunk_hash` 的命中，context 与 citation 层就不用返工。

一句话：**检索可以换引擎，上下文规则不换；上下文可以换策略，fragment 契约不换。**

## 上下文工程

### 1. 请求确定性与缓存稳定

同一个 session 状态 + 同一个 query 必须产出同一个 provider 请求（`request_hash`
稳定）：

1. fragment 顺序固定：`policy.v1` 与 tool 定义在最前，history 居中，evidence
   按第 1.1 节的三层排序规则，`question.v1` 最后；
2. 稳定前缀不含易变字段：system 级内容禁止出现时间戳、run_id、session 统计；
3. tools 定义 canonical 序列化：列表排序 + 字段序固定（provider 的缓存键就是
   tools JSON 的序列化顺序——百炼端点一手规则），并纳入 `request_hash`；
4. 去重按 `chunk_hash`，不按文本相似度。

这是从 Codex 的上下文纪律借来的：不重写历史、避免缓存失效、单项有界。

配套工程动作（2026-09-15 上下文调查）：

- `llm_turns` 增加 `cached_tokens` / `billable_tokens` 记账字段；`run_events`
  带 cache 摘要——缓存收益可观测，不做只凭感觉的优化；
- 相邻 run 公共前缀长度回归测试：前缀字节级稳定是契约不是愿望；
- 只做本地**精确**缓存（`request_hash` → response）；语义缓存被三线证伪
  （vCache "不可部署"、NDSS 投毒、ICML 碰撞劫持），明确不做。

### 1.1 Evidence 三层排序（替代「按 retrieval rank 排」）

「evidence 按 retrieval rank 排」已被 CacheWeaver（2026-06）点名为缓存失效
模式：集合同、序不同 → 前缀断裂。修订为三层统一，排序函数仍是 session 状态
的纯函数：

```text
第 1 层（缓存）：session 内 chunk 位置守恒——已出现过的 chunk 固定其首次
                出现位置，新 chunk 追加在后；
第 2 层（连贯）：同源 chunk 按原文顺序连续；
第 3 层（效果）：跨源首次排序按重要性——最高分源置首、次高置尾、其余居中。
```

三层叠加后首轮确定、后续轮次字节稳定——效果（首轮）、连贯（源内）、缓存
（后续轮）各得其所。位置效应证据本身脆弱（2026 复现：幅度强依赖条件），
故「首尾摆放」排第 3 优先级而非第 1。

实现落点：`momotaro-context` 的 rank → render_provider_messages 两阶段之间加
「位置策略」子规则；session 位置守恒表是 0.x 采纳项（v0.3 起）。

### 2. 预算分配

`max_context_tokens` 按固定优先级分配，先到先得不回填：

| 顺序 | 类别 | 约束 |
|---|---|---|
| 1 | policy + tool definitions + **constraints 常驻层** | 固定小预算，内容稳定；压缩不可触达 |
| 2 | question | 全量保留 |
| 3 | history | 近轮全量（见 §3），旧轮见 §3 |
| 4 | evidence | 按意图分档的 top-k（见 §2），三层排序（§1.1） |
| 5 | instructions | 固定尾部 |

### 2.1 两个可校准参数（2026-09-15 调查）

以下数值标注为「假设 + 观测闭环」，不是定论：

1. **token 估算系数 ×1.25**：处于工程谱系低端的合理值（漂移 5-15%、事故
   编目 2.0×、常见 1.5×）。校准闭环：从 provider usage 回读 P95 低估率；
   中文占比高的库应提到 1.4-1.5（CJK 估算偏保守）。
2. **8192 默认预算**（32k 窗口的 25%）：恰好落在 RAG 性能峰值区（有效填充
   8k-32k / 标称 25-50%；RULER 有效上下文仅 50-65%，NoLiMa 在 32k 处腰斩，
   context rot 实测含 Qwen3）——「预算是性能优化不是妥协」有可引用依据。
   后置自建评测：qwen3@32k 填充率扫描（2k/4k/8k/12k/16k 档位）。

evidence 兜底：预算再紧也保留 P1 的证据，条数按**预算比例联动**——evidence 段
不超过总预算 50% 内的最大条数，下界 3、上界 10，按意图分档（fact 3-5 /
comparison 每侧 4-6 / survey 8-10；2026-09 调查：精排后 5-10 条是经验最优区
间）。不足时显式记 `omitted_reason`，不静默丢。`adjusted_total = ceil(raw_total * 1.25)`
的估算规则不变，但系数标注为**待校准参数**（见 §2.1）。

### 3. 多轮选择（select_history）

`ask --session` 的后续轮次：

1. **recency 保护带**：近 2 轮且 ≥8K token 保底，取宽者（一轮可能是 50
   token 也可能是 20K，固定轮数不安全；OpenCode 40K 保护带先例）；
2. 保护带之外的更早轮次降级为摘要（`summary.v1`，见 §3.1），不重放原始
   evidence 全文；
3. 旧轮被引用过的 chunk 以标题 + 指针出现，需要全文时由模型走 `kb_read`
   （ReadAgent gist+lookup 同构——指针化记忆有直接学术先例与理论豁免权：
   rate-distortion 下限证明任何有损压缩存在事实幻觉下限，原文永不压缩）；
4. 已注入过的 evidence 按 `chunk_hash` 去重，不因多轮重复占预算。

### 3.1 summary.v1 分字段结构

摘要不是一段自由文本，是分字段 schema（显式声明**不承载约束**——约束类信息
物理上放 system 常驻层，永不依赖压缩存活；Governance Decay：周期性 compaction
会静默擦除安全约束且不可观测）：

```json
{
  "version": "v1",
  "covered_turns": ["history_id..."],
  "first_kept_item_id": "history_id...",
  "tokens_before": 6543,
  "user_decisions": [{ "decision": "...", "reason": "..." }],
  "open_questions": ["..."],
  "evidence_refs": [{ "chunk_hash": "...", "title": "...", "claim": "..." }],
  "next_action_hint": "..."
}
```

结构规则（2026-09-15 会话管理调查，与 opencode/Pi 两套独立实现对照）：

1. 所有分字段**必填**，空则写 `"(none)"`——保结构可断言；
2. 压缩提示词明确：**精确保存 chunk_hash / 标题 / 命令 / 错误串 / 路径，
   不许转述**；
3. `user_decisions` 每条带一行理由（Pi Key Decisions 格式）；
4. 摘要生成的 usage 写 run_events 并计入会话成本；
5. 再压缩合并规则：上一份 summary 未提及的目标 / 约束 / 用户决策 / 开放问题
   必须携带；冲突时新对话胜并显式改写。

断言是 **fail-closed**（见 §4 第 3 条）：必需分字段存在、`evidence_refs` 的
chunk_hash 逐字存在于摘要存储文本；失败重试 N 次后放弃压缩、保留原史。

### 4. 压缩触发（两轮调查的合并裁决）

两轮 2026-09-15 调查在此冲突：上下文工程轮认为「早压保质量方向有据、数值无
外部证据」，保留 40% 为起步值；会话管理轮指出 **40% 是全场孤例**（无任何
系统公开低于 50%），且缓存经济学惩罚频繁压缩——每次压缩开新 epoch = 旧前缀
整体失效一次（缓存未命中约 10 倍价差），Momotaro 的 8192 档有效上下文本来
就已经在压制 context rot。**裁决：默认对齐业界，早压降为实验档**——

```toml
[policy]
compact_trigger_ratio = 0.85      # 默认；0.40 保留为「质量实验档」，须配观测数据使用
compact_reserve_abs = 12000       # 吸收工具输出突发；按预算缩放 min(12000, 25% × 预算)
# trigger = min(max_context_tokens - compact_reserve_abs, ratio × max_context_tokens)
```

配套规则（采纳自会话管理轮）：

1. **保留尾**：压缩后保留最近原轮（默认预算的 40-50%，大预算会话 8-20K 档；
   8192 档约 3-4K）；**切点禁止落在工具调用与其结果之间**；摘要后重注入
   「最近读过的文件」工作集（至多 5 个，超 5000 token 退化为指针）；
2. **观测三指标**不变：每千轮压缩次数、摘要 token 均值、压缩后首轮引用验证
   通过率——若 0.40 实验档在自家数据上质量增益覆盖缓存+摘要成本，再下调默认；
3. **fail-closed 断言**（OpenClaw safeguard 式，替代旧「指针可解析」软断言）：
   必需分字段存在、`evidence_refs` 的 chunk_hash 逐字存在于摘要存储文本；
   失败重试 N 次后放弃压缩、保留原史，失败写 run 记录——防「丢约束/丢决策」
   （交接头号失败模式）的最后闸门；
4. 边界不变：只追加 `summary.v1`（§3.1 分字段结构），不删不改 `history_items`；
   压缩生效 = 新 epoch，旧前缀整体失效一次好过逐轮断裂（既有 epoch 规则的
   缓存论证）。

## 检索工程

### 1. 分块策略

1. heading-aware：优先沿标题层级切；
2. 表格与代码块不跨 chunk 切断；超长块整体保留并显式标 `truncated`；
3. `chunk_hash` 确定性生成：`f(source_key, revision_hash, ordinal, text)`；
4. 中文走 CJK tokenizer，不做空格假设；
5. `max_chunk_tokens` / `chunk_overlap_tokens` 是 index policy 参数，改动视为
   策略版本变化，触发重索引，不影响 canonical 表；
6. **标题链前缀进索引**：检索文本 = `《标题》 > §heading_path：` + chunk 正文。
   2026-09 调查（arXiv:2608.00824）证实零 LLM 成本可拿到 contextual
   retrieval 方案的大部分收益；Markdown / LaTeX 源的标题结构是天然供给。
   注意区分：**索引文本**带前缀（帮助命中），**canonical `chunks.text` 保持
   原文切片**（引用验证永远对原文），前缀在 retrieve 层拼接，不写回存储。

### 2. 增量索引

mtime + size 快速路径：未变文件跳过 rehash。`revision_hash` 不变则 chunk 与
Tantivy 文档不重建。重索引 = 按 `source_key` 删旧 revision 文档再写新，canonical
表不动。

### 3. 评测先行

BM25 基线没有量化之前，不上 dense / hybrid：

1. fixture golden set：每个 query 标注期望命中的 source / chunk；
2. 离线指标：`recall@k`、`MRR`，零 LLM、可重复；
3. 任何检索改动（分词、分块、权重、融合）必须先过回归再合入；
4. 这是 Architecture §20 中「BM25 基线有了」这一触发条件的验收方式。

**golden set 口径升级**（2026-09-15 调查：选择的目标函数是「充分性」而非
「相关性」——Google Sufficient Context）：

1. 从「标相关」升级为「标充分集」：每题标注构成可回答最小集的几条证据；
2. **hard negatives 是负例类别**：半相关、无信息量的段落显式标注——它们是
   top-k 的主要加害者且随数量单调加害；rank 阶段对 BM25 分数断崖的同文档
   段落降权；
3. **注入回归子集**（20-50 条）：prompt 内嵌「忽略上述指令」类文本，utility +
   robustness 双报告——检索库本身是攻击面（PoisonedRAG：5 条投毒文本即可
   达 90% 攻击成功率），投毒检查在入库时做。

**v0.3 起加生成层指标**（调查结论：LLM 自由引用的论文选择准确率仅 4-18%，
引用质量必须量化，不能只看检索层）：

1. **citation precision**：答案引用中指向真实本地 chunk 的比例（机械可算，
   零 LLM——就是 `answer_citations.verified` 的聚合）；
2. **citation coverage**（ALCE 式）：答案中的事实性断言被引用支撑的比例；
   起步用人工抽检，本地 NLI 自动化是后置项——标准配方已有（SummaC 句切分
   + 聚合、DeBERTa 级检测器、RAGTruth 校准语料），AttributionBench 证明
   NLI 微调系显著优于零-shot 与 LLM 自评；
3. **faithfulness**：同 NLI 通道，后置；
4. **context relevance 机械近似**：进入 context 的 golden 未标注充分集比例
   （零 LLM，context 层的查准对照）。

golden set 随真实使用增长（目标 50-200 条），对比类问题按「侧」标注——一侧
证据缺失时模型是否诚实说「证据不足」是可信度底线指标。后置：RAGChecker 式
claim 级 relevant-correctness / irrelevant-misleading 拆分（后者直接度量
「被坏上下文带偏」，依赖 NLI 就绪）。

评测 harness 可以用开发期 Python 写（见 [Python 与 Rust 协作边界](python-rust.md)），
通过 `momotaro search --json` 或只读 SQLite 取数。

### 4. 演进路径与接缝

```text
BM25（默认） -> expand(RRF) -> dense(LanceDB, derived) -> hybrid fusion -> rerank（可选）
```

1. rerank 是纯排序位：重排不产生新事实，只改 `included` 顺序与 rank，命中仍带
   完整四键；
2. dense 向量记录必须带 SQLite 稳定键（见
   [存储边界](storage-events.md)）；
3. 每一级的引入都以第 3 节的回归指标为门槛，不以「更先进」为理由；
4. 融合后进入 context 的排序遵循 §1.1 三层规则（session 位置守恒 > 同源原文
   顺序 > 首次重要性首尾）——检索分数决定「谁进来」，三层规则决定「怎么摆」；
5. rerank 候选池 ≤50（Drowning in Documents：池越大 rerank 越差）；
6. 近似重复软去重：同文档相邻 chunk 覆盖度起步，后置 MMR / token Jaccard
   （不引入 embedding 依赖）。

### 4.1 查询意图路由（v0.3 ask 的接缝，v1.x 再实现）

调查（Adaptive-RAG, 656 引）表明按查询复杂度路由优于全局技术替换。Momotaro
的路由位放在 `momotaro-context` 的 `select_evidence` 之前，是纯函数接缝：

```text
query_intent: fact | comparison | survey
```

- **fact**（默认）：现行单步检索即可；
- **comparison**：分侧取证 → pointwise 抽取 → 维度对齐表格输出（调查结论：
  不要让模型在混合上下文里 pairwise 直觉比较，BlackboxNLP 2025 偏倚证据）；
- **survey**：后置，与任务图（[multi-agent](multi-agent.md)）合流。

路由错误代价低（fact 路径就是现状），0.x 只埋枚举不实现分支。多跳查询走
引用网络扩展（D19），不先做迭代 agent 循环——多步 token 成本可达 5 倍
（arXiv:2601.19827），必须有预算护栏与用户可见轨迹才值得开。

### 5. Ingest 覆盖诚实性

抽取失败、扫描页、公式丢失必须写成 ingest 事件，不允许把部分提取当成完整索引
（借自 Light-skills 的 `light-file-reading` 纪律，见
[融合边界](light-skills-integration.md)）。`paper add` 是 Run（`mode=paper_add`）。`index` 写 ingest 事件、不建 Run；覆盖与失败进那些事件，不进 `trace`。
