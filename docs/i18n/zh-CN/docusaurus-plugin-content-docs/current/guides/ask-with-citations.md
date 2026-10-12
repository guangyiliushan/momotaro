---
title: 带引用的提问
description: 把一次提问变成有记录、可验证的运行——会话、防重放的幂等重试，以及引用保证到底意味着什么。
type: task
sidebar_position: 2
---

# 带引用的提问 {/* #ask-with-citations */}

`ask` 在代码里检索、在代码里构建上下文、至多发一次模型调用，并把答案绑定到字节。这一页讲它交回什么，以及它周围的三个旋钮。

## 交回什么 {/* #what-comes-back */}

- 答案，带编号的引用；
- [运行](../glossary.md#run) id，以及一条可直接粘贴的 `momotaro trace <run_id>`；
- 每条[引用](../glossary.md#citation)的校验结论——失败的引用显示为失败，绝不被悄悄丢掉。

运行失败时，命令退 `1`——同时照常打印答案与显式的失败标记，脚本和人看到的是同一件事。

## 会话 {/* #sessions */}

不带 `--session` 时，每次提问新开一个会话。要接着上一轮，把它的 id 带上（上一条答案的两个出口都会打印 id）：

```bash
momotaro ask --session <session_id> "那噪声项呢？"
```

id 必须指向存在的会话——写错是报错，而不是悄悄新开一场对话。上一轮若因崩溃中断，下一条命令会先清扫悬挂的轮次再继续；没有需要手工收拾的东西。

## 防重放的幂等重试 {/* #retries */}

会重试的自动化靠 client request id 获得"恰好执行一次"：

```bash
momotaro ask "……" --client-request-id retry-7 --allow-network
```

同一个 id 重放返回**同一条** run——不重复干活、不重复花钱。同一个 id 配不同的请求会被点名拒绝（幂等键冲突），空 id 同样被拒。让 id 命名"逻辑请求"，而不是"第几次尝试"。

## 网络门 {/* #network */}

提问要和模型 provider 说话，因此在网络被显式允许之前一律拒绝——命令行上 `--allow-network`，或在配置里一次性允许：

```toml title="momotaro.toml"
[policy]
allow_network = true
```

拒绝发生在**建立连接之前**：什么都不记录，退出码 `1`。`--no-llm` 则直接拒绝 `ask`——不存在"不靠模型也答一句"的中间态。

## 引用校验失败时 {/* #failure */}

引用失败，运行就失败。这是设计：答案不会被"清理"到看不出失败，回放里精确指出哪条引文在记录下来的修订中找不到。把失败当作关于材料的信号，而不是关于命令的信号——检查尽到了它的职责。

## 去哪里继续 {/* #next */}

- [回放一次运行](trace-a-run.md)——看模型当时看到的一切。
- [引用循环](../concepts/retrieval-and-citations.md)——那些标记背后的概念。
- [术语表](../glossary.md#verification)——"校验"的精确含义。
