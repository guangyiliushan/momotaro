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

打开数据库时做一次确定性清扫：

1. 扫 `runs` 中 `status IN (accepted, running)` 且无 terminal 事件、且属于已死
   进程的行；
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
   要么没有（与 Architecture §10.3「失败不得留下半初始化 source」一致）；
4. UI 只发取消命令；只有 run 引擎改状态。

## 4. 超时与重试

| 对象 | 超时 | 结果 |
|---|---|---|
| provider turn | policy 配置，默认 120s | `llm.finished` 带 error，run failed，category=provider |
| 工具执行 | policy 配置，默认 30s | `tool.finished` 带 error |

重试规则（借 opencode/pi 的纪律）：**只在可观察输出产生之前自动 retry**，至多
一次；一旦有 token 或结果已落库，失败就如实标 failed。ask 的失败 retry 是用户
显式新 run，不是引擎静默重放。

## 5. 并发模型

MVP 刻意简单：

1. SQLite WAL：并发读不受限；**写是单写者**——进程内一个写队列，run 串行
   提交事务；
2. 一个 session 同时至多一个活跃 run；跨 session 的 `search`（只读）可并行；
3. 0.x 不做跨进程写协调：CLI 短生命周期、桌面单进程是前提。若未来出现第二个
   常驻写者，再引入 lock 文件或升级方案，不提前抽象；
4. feed 轮询、索引重建等后台任务在桌面端走同一写队列，不另开旁路写。

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
