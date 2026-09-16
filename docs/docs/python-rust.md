---
sidebar_position: 10
---

# Python 与 Rust 协作边界

## 结论

**产品运行时只有 Rust。** Python 允许出现在两个非运行时位置：开发期工具与外部
agent 技能。两者都不进 `Cargo.toml`、不进发布二进制、不常驻进程。

这条边界解决一个真实张力：科研生态（SymPy、数据处理、论文工具）大量是 Python，
而 Momotaro 的交付物是单二进制、可审计、跨平台的 Rust 应用。答案不是把 Python
塞进核心，而是：

1. 把「借 Python 生态」限制在开发期和显式子进程；
2. 让 Rust contracts 成为两种语言共享的唯一事实源。

## 四条通道

| 通道 | 用途 | 边界 |
|---|---|---|
| CLI `--json` | 评测、统计、批量分析 | 只消费输出；不写 canonical |
| 只读 SQLite | golden set 标注、回归取数 | 只读连接；schema 以 Rust contracts 为准 |
| 类型投影 | contracts → TS（`packages/ipc`）/ Python（评测） | 单一事实源在 Rust，生成而非手写 |
| 版本化子进程 | 默认 PDF 解析与高配 PDF 解析（GROBID/Marker）；后置 math/CAS 验证 | JSON stdin/stdout；高配默认关；不在 ask 热路径 |

前三条现在就可以用；第四条是版本化子进程协议，默认 PDF 解析从第一版启用，高配后端可换。

## 高配解析器子进程（v0.4 接缝，D18）

PDF 双层解析走同一子进程通道（决策见 [Architecture D18](architecture.md#decisions) 与
2026-09 学术 RAG 调查）：

1. 默认层是同步子进程（纯 Rust crate、版本化 stdin/stdout），CLI 与桌面 spawn 同一二进制；高配层
   （GROBID 的引用对齐、Marker/MinerU 类版面解析）按需显式运行；
2. 协议与 CAS 接缝同构：版本化 JSON stdin/stdout，结果带
   `tool + tool_version` 回写 `metadata_json`，解析质量可审计；
3. 子进程无网络（除显式 `paper` 网络策略）、有超时、崩溃不影响默认层已入库
   的内容——高配结果是**增强**（替换 chunk 需新 revision），不是前置依赖；
4. 解析失败与低置信页显式降级记录（ingest 覆盖诚实性，见
   [上下文与检索工程](context-retrieval.md)），不静默劣化。

## 开发期工具放在哪

```text
momotaro/
  tools/
    evals/          # golden set + 检索回归（可 Python）
    migrate/        # 一次性迁移脚本
```

规则：

1. `tools/` 不进 pnpm workspace、不进 Cargo members、不被 turbo 管理；
2. 评测入口固定：先 `momotaro search --json` 或只读 SELECT，本地算指标，
   零 LLM、可重复；
3. Python 脚本绝不直接写 canonical SQLite；要导入数据，走显式
   `momotaro import`（校验 schema 与 hash 后进事务）；
4. CI 里 `cargo test --workspace` 先行，Python 评测作为可选后置 job。

## contracts 的语言投影

`momotaro-contracts` 是唯一事实源。类型预留 schema 导出接缝（实现期选型），
生成 `packages/ipc` 的 TS 类型与评测用 Python 类型。方向永远
Rust → 其它语言；禁止在 TS / Python 里维护第二份契约再「保持同步」。

## math/CAS 接缝（后置，不是承诺）

`verifications.kind=math` 已留枚举（见 [Reference](reference.md)）。若 1.0 后确实需要：

1. 先找 Rust 原生 crate；没有可用的，才考虑子进程验证器；
2. 子进程协议是版本化 JSON：

   ```json
   {"expression": "...", "expected": "..."}
   {"status": "passed", "value": "...", "tool": "...", "tool_version": "..."}
   ```

3. 由 policy 显式开关（默认关）；`status` 枚举沿用
   `passed | failed | unsupported | skipped`，`passed` 只来自确定性计算，
   LLM 不得宣布数学正确；
4. 子进程无网络、无文件写权限、有超时。

## 外部 agent 技能

Light-skills 等技能包放 `.agents/skills`，只服务于开发期的调研与代码代理，
不进入产品任何路径。见 [Light-skills 融合边界](light-skills-integration.md)。

## 明确拒绝

1. PyO3 / 嵌入 CPython 进核心；
2. Python 常驻 sidecar（HTTP 或其它）作为产品运行时；
3. Python 写 canonical 数据库；
4. FastAPI / Flask 中间层冒充 `serve`；
5. 为评测方便把 provider 调用塞进 Python——fake LLM 在 Rust 测试里做。

理由：单二进制交付、交叉编译、崩溃面隔离、审计边界（能写库的只有 Rust 事务）。
