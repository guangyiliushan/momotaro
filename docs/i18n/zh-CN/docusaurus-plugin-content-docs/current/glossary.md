---
title: 术语表
description: 一个概念一个词，定义一次——所有页面共同使用的词汇。
type: reference
sidebar_position: 6
---

# 术语表 {/* #glossary */}

一个概念一个词，定一次。术语在页面上第一次出现时链接到这里。

{/* generated from terminology.yaml — do not edit by hand */}

## 工作区 {/* #workspace */}

包含 `momotaro.toml` 的目录；独占一个数据库，论文原文就放在数据库旁边。

_避免使用_: vault, project, profile, app-data store.

## 笔记库 {/* #vault */}

工作区指向的那一棵笔记目录树。

_避免使用_: library, knowledge base, corpus.

## 来源 {/* #source */}

库可以引用的一个笔记、论文或网页文档；它的身份是来源键，不是行号，也不是随时会变的文件路径。

_避免使用_: file, document.

## 来源键 {/* #source-key */}

来源的带方案前缀的身份（`note:` / `arxiv:`），做 NFC 归一、永不做大小写折叠。

_避免使用_: canonical file path, database id, URL as key.

## 来源修订 {/* #source-revision */}

来源的一份不可变字节快照；更新是追加一个新修订，而不是覆盖旧的。

_避免使用_: version, file, document.

## 块 {/* #chunk */}

某个来源修订的确定性投影；检索命中和引文定位都以它为单位。

_避免使用_: passage, embedding, canonical fact.

## 来源类别 {/* #origin-class */}

字节的作者是谁：`owner` | `paper` | `web` | `agent` | `system`。

_避免使用_: trust score, provenance.

## 项目 {/* #project */}

有名有姓的一组来源（以后还包括批注与已收下的启发卡）；删掉项目永远不删来源。

_避免使用_: workspace, folder, tenant.

## 会话 {/* #session */}

一条对话线索，创建时绑定零个或一个项目；这个绑定决定默认检索范围。

_避免使用_: chat, thread, project.

## 批注 {/* #annotation */}

钉在某个来源修订与定位点上的手写笔记；来源类别永远是 `owner`。

_避免使用_: comment, highlight, memory.

## 启发卡 {/* #spark */}

由 agent 提出、带引用的卡片（`proposed` | `pinned` | `dismissed`）；只有被收下且带引用的启发卡才是检索来源。

_避免使用_: memory, idea, wiki page.

## 配置档 {/* #profile */}

一组有名有姓的运行默认值（模型槽位、策略、工具集）；不是记忆，也不是笔记。

_避免使用_: persona, memory, user profile.

## 运行 {/* #run */}

一次被受理、可审计的工作单元；终态由与它同时记录下来的终止事件决定。

_避免使用_: job, request, agent loop.

## 提问运行 {/* #ask */}

一次先在代码里做检索与上下文构建、然后至多向模型发起一次调用的运行。

_避免使用_: agent, tool loop, chat completion.

## 同库检索 {/* #same-library-retrieval */}

一份同时覆盖手写笔记与论文的词法索引；一次查询返回混合来源类别，按来源加权排序。

_避免使用_: hybrid search, knowledge graph.

## 上下文范围 {/* #context-scope */}

P1（已绑定项目的来源，未绑定时为整个笔记库）与 P2（其余部分）之分；它决定什么进入模型上下文，不影响检索记录了什么。

_避免使用_: search filter, permission, tenant.

## 引用 {/* #citation */}

把一条论断锚定到具体字节的五元组：来源键、修订、定位、引文、哈希。

_避免使用_: reference, footnote, link.

## 引用校验 {/* #verification */}

把引用与记录下来的字节核对；失败是显式的一等状态，绝不藏着不说。

_避免使用_: fact check, spell check.

## 回放 {/* #trace */}

对一次运行的完整复原记录——查询、证据、提示、模型回答与校验结论——事后可回放。

_避免使用_: log, history.

## 订阅流 {/* #feed */}

由工作区轮询的 RSS / Atom / JSON Feed 来源，可用 OPML 批量导入；订阅流里的论文要过确认门才进库。

_避免使用_: reader, newsletter.
