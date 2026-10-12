---
title: 快速开始
description: 从空目录到可搜索的工作区——初始化、索引笔记、检索，以及第一次带引用的回答。
type: task
sidebar_position: 1
---

# 快速开始 {/* #quickstart */}

这一页从零开始，走到一个可搜索的工作区和第一次带引用的回答。0.x 版是命令行工具，从项目源码构建；预编译安装包随第一个正式版本到来。

## 1. 构建命令行 {/* #build */}

需要 Rust 工具链。在项目检出目录里：

```bash
git clone https://github.com/guangyiliushan/momotaro
cd momotaro
cargo build --release -p momotaro-cli
```

二进制落在 `target/release/`——`momotaro-cli`（Windows 上是 `momotaro-cli.exe`）。本页余下部分把它写作 `momotaro`。

## 2. 让工作区指向你的笔记 {/* #workspace */}

[工作区](../glossary.md#workspace)是一个包含 `momotaro.toml`、独占一个数据库的目录。创建它，并在里面放一个笔记文件夹：

```bash
mkdir study && cd study
mkdir notes
```

给出指向笔记文件夹的配置：

```toml title="momotaro.toml"
[vault]
path = "notes"
```

然后初始化：

```bash
momotaro init
```

```text
workspace initialized
```

两条规矩保工作区平安：笔记库必须住在工作区**里**；同步时只同步**笔记文件夹**——绝不要同步 `.momotaro/` 目录。数据库过不了同步服务这一关。

已经有笔记树了——一个 Obsidian 文件夹、一个研究目录？把 `[vault] path` 指向它，而不是新建一个：工作台读的是纯 Markdown，原样保留你的 front matter，绝不改写你的文件。

## 3. 索引笔记 {/* #index */}

往 `notes/` 里放两篇 Markdown，然后：

```bash
momotaro index
```

```text
indexed 2 of 2 files (2 new revisions, 0 skipped), 2 chunks, 0 errors (0 key collisions) in 90 ms
```

索引是确定性的——不需要账号、密钥或网络——并且如实报告它做了什么。重跑只处理变化：字节与位置都没变的文件不会产生新修订，大库的重复索引因此很省。随时检查工作区只需一条命令：

```bash
momotaro doctor
```

```text
workspace is ready (SQLite 3.53.2)
```

## 4. 检索 {/* #search */}

```bash
momotaro search "spectral theorem PCA"
```

```text
1. "note:pca.md"#0  "PCA and the spectral theorem"  score=11.700
    Principal component analysis diagonalizes the covariance matrix. The spectral theorem is why the eig...
2. "note:fourier.md"#0  "Fourier transform"  score=0.369
    The Fourier transform decomposes a signal into frequencies. In the discrete setting, the sampling th...
```

词法检索零模型 token、可离线。只有 `ask` 与展开检索会碰到 provider；其余全部——索引、`doctor`、回放、重建——都不需要它。脚本要读结果时加 `--json`。

## 5. 带引用地提问 {/* #ask */}

提问要花一次模型调用，因此需要 provider。配置里只写环境变量名；值留在你的环境里：

```toml title="momotaro.toml"
[model]
model_id     = "qwen3"
base_url_env = "MOMOTARO_BASE_URL"
api_key_env  = "MOMOTARO_API_KEY"
```

```bash
export MOMOTARO_BASE_URL="https://api.example.com/v1"
export MOMOTARO_API_KEY="sk-…"
momotaro ask "谱定理和 PCA 是什么关系？" --allow-network
```

回答会带着引用、[运行](../glossary.md#run) id，以及一条可直接粘贴的下一条命令回来。每条引用都已在你看之前与记录下来的字节核对过——校验失败会怎样，见[检索与引用](../concepts/retrieval-and-citations.md)。

## 去哪里继续 {/* #next */}

- [带引用的提问](ask-with-citations.md)——会话、重试与网络策略。
- [保持库与笔记同步](keep-the-store-current.md)——监视笔记库并对账。
- [回放一次运行](trace-a-run.md)——看模型当时看到了什么。
- [重建与恢复](rebuild-and-recover.md)——索引需要被替换的时候。
