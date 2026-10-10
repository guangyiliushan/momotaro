---
sidebar_position: 8
---

# Harness 工程：Run 生命周期的健壮性规则

[Architecture](architecture.md) §6.4/§10 定义了 Run 的契约；本文补齐它的工程纪律。
这些规则对 CLI 和桌面同样成立——桌面只是把同一批服务函数放进了长驻进程。

一句话：**run_events 是事实，runs.status 是投影；终态必达；副作用幂等。**

## 1. 状态机与终态必达

```text
accepted -> running -> finished | failed | cancelled
```

1. `runs.status` 永远由 terminal 事件驱动：`run.finished` / `run.failed` /
   `run.cancelled` 与 status 更新在**同一个事务**里原子落库；
2. 不存在「status=finished 但没有 terminal 事件」或反过来的状态；
3. `run_id` 贯穿全部事件；`client_request_id` 只负责幂等去重（重放同一请求返回
   同一 run，不新建）；
4. 进程在任何一步崩溃后，库里的状态都能如实反映「中断」，而不是悬挂的 running。

## 2. 崩溃恢复

由 `ask` 与 `trace` 在打开库时做一次确定性清扫（`index` / `search` / `doctor` 不扫；
`--mode expand` 会建 run 却不扫，它留下的悬挂 run 由下一次 `ask` / `trace` 收尾）：

1. 扫 `runs` 中 `status IN (accepted, running)` 且无 terminal 事件的行。
   「属于已死进程」**不是查询里的第三个子句**——谓词不判 liveness，`momotaro-store` 的
   `sweep_interrupted_runs` 在 doc 里记着同一件事。它本该由 0.x 的单写者纪律
   （§5 第 3 项）成立，但 §5 自己记着这条纪律在 0.x 的**已知破口**：两个跨 session 的
   并发 CLI 会互扫对方**活着**的 run。那一半是已知面，**不由本查询保证**，也不在本条的
   承诺里；将来出现第二个常驻写者时，这条要从「前提」变成「查询」，不是改措辞；
2. 追加 `run.failed`，`data.category = interrupted`，attribution=system，
   retryable=true；
3. 不自动重跑。用户显式重试时新建 run（`parent_run_id` 指向旧 run）。

配套规则：

- 副作用幂等：`paper add` 以 `(arxiv_id, version)` 幂等，ingest 以
  `(source_key, revision_hash)` 幂等——崩溃恢复后重试不产生重复事实；
- `replay=never` 的工具（写操作）崩溃后**只允许人工重试**；
- SQLite WAL；写事务短小；torn write 不可能跨事务出现，无需 JSONL 式
  torn-tail 修复（见 [存储边界](storage-events.md)）。

## 3. 取消

1. 取消是协作式的：provider 调用与工具执行检查取消信号后尽快停止；
2. 取消终态是 `run.cancelled`，已完成的写入（事件、hit、turn）保留，不回滚
   canonical 表；
3. 取消不删除半成品 source：`paper add` 中断的下载留在临时区，正式行要么完整
   要么没有（与 Architecture §15 的 `paper add`「失败不得留下半初始化 source」一致）；
4. UI 只发取消命令；只有 run 引擎改状态。

## 4. 超时与重试

| 对象 | 超时 | 结果 |
|---|---|---|
| provider turn | `[policy].provider_turn_timeout_s`，默认 **120s** | 超时按一次失败 turn 落账（`llm.finished` 失败载荷）；**turn 不重开**，但发送阶段的超时发生在可观察输出产生之前，仍归下面那条「至多一次自动 retry」管——那不是第二个 turn |
| 工具执行 | `[policy].tool_timeout_s`，默认 **30s**（读入，尚未接线 —— 见下） | — |

> **状态（2026-10-09）。** provider turn 的截止是**挂死逃生阀，不是模型质量旋钮**：
> 它的存在是让「接受了连接然后停住」的 endpoint 不能把 `ask` 挂到无限期；而一个
> 通用的总截止又可能砍掉慢而有效的长回答，所以数字归 `[policy]`，不归 `http.rs`。
> 落地是：`PolicySection`（`crates/momotaro-run/src/lib.rs`，七键）读
> `provider_turn_timeout_s`，`http_transport_with` 用它构造
> `HttpTransport::with_turn_timeout`，落到 ureq 的 `timeout_global`（覆盖 connect /
> recv_response / recv_body；连接另有的 10s 常量仍在）。`http.rs` 里的
> `RESPONSE_TIMEOUT = 300s` 从此只是裸 `HttpTransport::new` 的回退值，**不再是事实上的
> 契约数字** —— 契约数字是上面那个默认 120s。
>
> `tool_timeout_s` **读入未接线**：本构建还没有工具执行（`tool.started` /
> `tool.finished` 没有生产者），键先落配置，等工具执行落地那天它是配置而不是迁移。
>
> 「用户取消」这条退路仍无生产者（`run.cancelled` 已在 `reference.md` 的 reserved
> 名单里），不在本次两键的范围。

重试规则（借 opencode/pi 的纪律）：**只在可观察输出产生之前自动 retry**，至多
一次；一旦有 token 或结果已落库，失败就如实标 failed。ask 的失败 retry 是用户
显式新 run，不是引擎静默重放。

## 5. 并发模型

MVP 刻意简单：

1. SQLite WAL：并发读不受限；**写是单写者**——同一时刻只有一个写者，锁由
   每条写路径自己取（`commit_*` 单元开 `BEGIN IMMEDIATE`），`busy_timeout`
   5s，run 串行提交事务；
2. 一个 session 同时至多一个活跃 run；跨 session 的 `search` 可并行——
   lexical 不写 run / retrieval / turn 行（`--mode expand` 会写，见
   [Architecture](architecture.md) §10.2 写入行），但两者都不是零写：打开库要走一次
   `init_schema` 的元数据 UPSERT、拿一次写锁。改写期间不进事务，所以两个 `expand`
   的改写调用会重叠；
3. 0.x 不做跨进程写协调：CLI 短生命周期、桌面单进程是前提。若未来出现第二个
   常驻写者，再引入 lock 文件或升级方案，不提前抽象；
4. feed 轮询、索引重建等后台任务在桌面端走条目 1 那个单写者，不另开旁路写。

一个 session **跨进程存活**（`ask --session` 的每一轮都是新进程）；同 session 同时至多
一个活跃 run —— 这条由**本条第 3 项的单写者纪律 + §2 每次打开时的清扫**共同成立，
**没有**一道独立的「会话忙」拒绝门。0.x 判不出一条 `accepted` / `running` 的行背后是
活进程还是已死进程：判据就是清扫自己用的那条谓词（pre-terminal 状态 + 无 terminal
事件），里面没有任何 liveness 信息，所以那样一道门只有两种结局——要么拦在一个马上就要
被清扫掉的孤儿上，把 §8.1 的恢复路径从 `--session` 这个**唯一会卡住的入口**堵死；要么
因为单写者纪律下根本没有第二个进程而永不触发。因此 `ask` **先清扫、后判 session**——顺序实现在 `ask_workspace`
（§2 只规定清扫本身），悬挂 run 在下一轮开始前就地收尾。0.x 仍不做跨进程写协调（本条第 3 项
不变）：**跨 session** 的两个并发写者仍会互相清扫对方的活跃 run，这是已知面，待第 3 项
升级时一并处理。

## 6. 事件顺序与重放

1. `seq` 每 run 内单调递增，`(run_id, seq)` 唯一约束兜底；
2. 事件只追加；`run_events` + canonical 表足以重建完整时间线（`trace` 的实现
   就是这个性质的验收）；
3. 过程性进度（token delta、UI 百分比）不落库，丢了不影响事实。

## 7. Schema 版本

1. `momotaro-store` 记录 `schema_version`；启动时校验；
2. 旧版本二进制打开新库：拒绝并提示升级，不做尽力而为读取；
3. migration 只前进；canonical 表的 migration 必须能处理「事件流已存在」的库，
   不假设空库。

## 8. 验收

1. 在任一步 `kill -9` 后重开库：run 终态为 `failed(interrupted)`，`trace` 可用；
2. 同一 `client_request_id` 重放不产生第二个 run；
3. 取消后库中无悬挂 running；
4. 崩溃后重试 `paper add` 不产生重复 source revision；
5. 两个并发 `search` 不互相阻塞。
