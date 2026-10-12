---
title: 配置
description: momotaro.toml——笔记库位置、模型端点的接线与网络策略；秘密只留在环境里。
type: reference
sidebar_position: 2
---

# 配置 {/* #configuration */}

一个 Momotaro 工作区，就是一个包含 `momotaro.toml`、独占一个数据库（`.momotaro/`）的目录。配置文件刻意写得短：它只命名位置与环境变量，从不写秘密。

```toml title="momotaro.toml"
[vault]
path = "notes"

[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"

[policy]
allow_network = false
```

## `[vault]`——笔记在哪里 {/* #vault */}

- `path`——要被索引的笔记树，相对工作区（也可写绝对路径，但必须解析到工作区**里面**；`笔记库 == 工作区根` 合法）。记录在案的都是工作区相对路径，这就是笔记库不能住到别处的原因。

## `[model]`——provider 端点 {/* #model */}

- `model_id`——提问时使用的模型。
- `base_url_env`——存放 API 根地址的环境变量的**名字**（例如 `MOMOTARO_BASE_URL`）。值本身留在你的环境里，不进文件。
- `api_key_env`——存放密钥的环境变量名；不需要密钥的端点可以省略。

只有真正要和模型说话的时刻才需要端点——`ask` 与 `search --mode expand`。在那之前，工具链的其余部分按构造就是离线的。

## `[policy]`——默认允许什么 {/* #policy */}

- `allow_network`——Beta 默认保守：网络访问**关闭**，除非你放行——按命令（`--allow-network`）或在这里一次性开启。关闭期间，`ask` 与 `search --mode expand` 在建立任何连接之前就被拒绝。

## 让工作区保持健康的两条习惯 {/* #habits */}

- **秘密永不进文件。** 文件里只有环境变量名；值待在秘密该待的地方。
- **同步笔记，不同步工作区。** `.momotaro/` 里装着数据库；同步服务会弄坏数据库。只同步笔记库文件夹。

## 去哪里继续 {/* #next */}

- [命令行参考](cli.md)——每条命令与它的选项。
- [快速开始](../guides/quickstart.md)——以上一切的实际操作。
