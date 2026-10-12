---
title: 命令行参考
description: Momotaro 的每条命令、参数与退出行为——0.x 工具的稳定面。
type: reference
sidebar_position: 1
---

# 命令行参考 {/* #cli */}

所有命令共用同一个二进制与同样的三个全局选项。这一页列出存在什么、吃什么参数、怎么退出——有意不列出未来版本可能添加的每一个 flag。

## 命令 {/* #commands */}

| 命令 | 做什么 |
|---|---|
| `momotaro init` | 在当前目录初始化本地库并校验配置。 |
| `momotaro index [PATH]` | 索引 Markdown 笔记（`PATH` 覆盖配置里的笔记库）。确定性、离线。 |
| `momotaro search [QUERY]…` | 对索引做词法检索。默认零模型 token。 |
| `momotaro ask [QUERY]…` | 从笔记回答问题、带引用；至多一次模型调用。 |
| `momotaro trace <RUN_ID>` | 回放一次运行：检索、上下文、引用、结论。 |
| `momotaro doctor` | 检查工作区健康，什么都不改动。 |
| `momotaro stats` | 打印库计数：状态、数据库路径、来源数、块数。 |
| `momotaro watch` | 笔记库变化时保持库同步（长驻）。 |
| `momotaro reconcile` | 一趟把库与整个笔记库对平。 |
| `momotaro rebuild-index` | 从规范存储重建检索索引。 |

值得知道的命令专属选项：

- `search`：`-n/--top <N>` 限制命中数（默认 10；`0` 合法，返回空）。`--mode lexical|expand` 选择纯 BM25（默认）或检索前先做一次 LLM 改写。
- `ask`：`--session <id>` 接续已有会话；`--client-request-id <id>` 让重试的请求防重放。
- `doctor`：`--coverage` 报告最近一次索引对每个文件做了什么，而不是检查健康。

## 全局选项 {/* #global-options */}

| 选项 | 含义 |
|---|---|
| `--json` | 在 stdout 输出机器可读的 JSON。 |
| `--no-llm` | 禁止一切模型调用；`ask` 与 `search --mode expand` 会拒绝。词法 `search` 不受影响。 |
| `--allow-network` | 允许需要它的命令访问网络。Beta 默认关闭。 |
| `-h`, `--help` / `-V`, `--version` | 帮助与版本。 |

## 退出码 {/* #exit-codes */}

- **`0`** 成功、**`1`** 失败——只有一处刻意的例外：`doctor` 以*健康*为准入门槛，未初始化或 invalid 的工作区即便报告打印正常也退 `1`。（`doctor --coverage` 是报告不是检查：只要日志读得到就退 `0`。）
- 用法错误（未知参数或子命令）退 **`2`**——而且连 `--json` 也拿不到 JSON 体，因为解析在程序自己的错误出口运行之前就失败了。
- `index` 的逐文件错误不会让整批失败：摘要里写着 `N errors`（N > 0）也照样退 `0`。

这些规则背后的道理——以及它们为之存在的恢复路径——在[重建与恢复](../guides/rebuild-and-recover.md)。

## 例：`stats` {/* #example-stats */}

```text
status: ready
schema version: 9
database: …/.momotaro/momotaro.db
initialized: true
sources: 2
chunks: 2
```

`--json` 携带同样的字段。尚不可知的计数在两个出口都打印 `unknown`——绝不伪造一个 `0`。

## 去哪里继续 {/* #next */}

- [配置](configuration.md)——`momotaro.toml` 文件。
- [快速开始](../guides/quickstart.md)——命令的实际顺序。
