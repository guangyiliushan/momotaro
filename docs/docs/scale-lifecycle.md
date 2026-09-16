---
sidebar_position: 11
---

# 规模与数据生命周期

本文承载 D29–D32（2026-09 规模与数据生命周期调查）的实施细则；决策记录见
[Architecture](architecture.md) §22。前提事实：**SQLite 引擎不构成 Momotaro 的
规模上限**（社区 1GB/10M 行容量恐惧已被官方 + PVLDB 2022 双背书证伪——勿再
引用）；红线以「治理触发条件」形式存在，数字待 pilot 回填。

一句话：**容量红线是治理触发条件不是存储上限；归档 ≠ 删除；同步走快照单向流。**

## 1. SQLite 引擎纪律（D29）

### 缺省 PRAGMA（八条定稿）

```sql
-- 既有三条：journal_mode=WAL; foreign_keys=ON; busy_timeout=5000;
-- 新增缺省：
synchronous=NORMAL;
cache_size=-65536;            -- 64MiB，四方趋同值
journal_size_limit=67108864;  -- 64MiB WAL 上限
trusted_schema=OFF;
-- 保持默认：wal_autocheckpoint=1000（≈4MiB，官方默认即合理）、page_size=4096
-- 显式不开：mmap（Windows VACUUM 截断静默失败）、auto_vacuum（存量库改造需整库 VACUUM）
```

### 版本钉扎（唯一立即代码项）

升级 `rusqlite` ≥ 0.40.x（bundled SQLite 3.53.2，出 WAL-reset 窗口
3.7.0–3.51.2）。本仓原捆绑 3.50.2 落在窗口内（WAL 重置可致库损坏；
Tailscale 19 起实害同因；修复亦回补 3.44.6/3.50.7）。`momotaro doctor`
输出捆绑 SQLite 版本，窗口内即告警。

### 连接层单写

写连接唯一（连接池 max=1 的写通道），读连接不限——crush `SQLITE_NOTADB(26)`
事故与五方单写先例的连接层教训，补强 [Harness](harness.md) §5。

### 容量红线 = 治理触发表（数字待 pilot）

| 触发信号 | 阈值（待 pilot） | 动作 |
|---|---|---|
| WAL 峰值 | > 64MiB 持续 | 检查长事务 / checkpoint 策略 |
| 负载 A p95 | > 200ms | 启用 run_events 治理（§2 档位） |
| 单库体积 | > 阈值（实测） | 归档 / 分库评估 |
| quick_check | > 2s | 自检降频或分区告警 |

大 blob 约束：进 WAL 后 page_size 永久冻结（首库即定 4096）；512-token chunk
设计天然避开 blob 穿破 WAL 上限问题（记录为约束而非动作）。

## 2. run_events 生命周期（D30）

`run_events` / `history_items` 是审计正史（§6.5 只追加）。治理只有两种合法
形态：**搬移**（归档文件仍属 canonical，`trace` 跨库透明读）与**派生重建**
（`usage_projections` 类聚合表可随意重建）。先例的 prune-on-insert（openclaw）
作用对象是可丢遥测，不可照搬到 canonical 表。

1. **默认方案（摩擦最低）**：单表 + `created_at` 既有列 + 时间索引；统计/
   聚合一律走 `usage_projections` 派生表；是否需要归档由 pilot 数字决定，
   不预先建设；
2. **归档变体（pilot 后按需）**：月度 `VACUUM INTO 'archive-YYYYMM.db'`
   （归档库不可变、属 canonical、ATTACH 只读挂载）；触发 = 主库聚合 p95 越线；
3. **分区表方案出局**：需过存量事件流 migration + 跨分区 `(run_id, seq)`
   唯一约束两关，收益不抵摩擦；
4. **快照滚动预算**：日 7 / 周 4 / 月 12；磁盘预算待 200k 档实测；
5. **Tantivy 独立条款**：commit 去抖 T=5-10s（同时是 Windows #2847 的正确性
   防御）；不主动 `purge_deletes`（默认策略删除率阈值形同虚设 + 首次 purge
   可达小时级——#710 教训）；重建时机由 §1 触发表管理。

## 3. 同步边界（D31）

### 否决与采纳

1. **C1 逐字否决**：整库 `.db/.wal/.shm` 进任何同步目录——官方禁令（根因
   共享内存）+ Zotero/Obsidian/OneDrive 生态级佐证。**Tantivy 索引目录同样
   禁止**（派生数据，同步 = 带宽浪费 + mmap 跨平台差异）。进 §21。
2. **C2 采纳（1.0 默认通路）**：桌面定时 `VACUUM INTO` 单文件快照（§4 四步法）
   → 用户指定同步目录（网盘 / Syncthing receive-only）→ 移动端只读打开。
   快照时间戳命名并在 UI 显示（防网盘冲突副本误开旧版）。
3. **C2+C5 混合变体（留缝）**：源 Markdown 镜像走同步（文本天然可同步可
   git）与快照并行；移动端「从源重建索引」是 C5 降级实现，留接缝不承诺。
4. **C3/C4 出局（附因）**：CloudKit（≤1MB 记录、Rust 生态弱、平台锁定）、
   PowerSync（FSL-1.1 非 OSI、需服务端）、ElectricSQL（已转型）、libSQL
   （绑云）、cr-sqlite（停更）、rqlite（HTTP 服务器违反 D12）。重评触发 =
   真实多写者需求出现。
5. **CRDT 正式排除**：local-first 研究「单编辑者文件同步即工作得很好」+
   CRDT 全面面向多写者并发 + 单人场景历史膨胀负资产。

### 运行时防御与目录三分离

- 打开库时探测路径是否网络挂载（mountinfo），是则告警或回退 rollback journal；
- 数据目录三分离：**源 Markdown（可同步）/ 运行时 `.db/.wal/.shm/tantivy`
  （禁同步）/ 快照导出（可同步）**，路径规范按 platformdirs 三平台口径；
- 移动端预期管理：iOS 后台同步现实是小时级（厂商 FAQ 口径）——文档写
  「小时级滞后」，不承诺准实时；
- 用户教育：「网盘会特殊对待数据库文件」是官方事实；快照 + 源镜像双通道
  是正当答案。

## 4. 备份与腐化自检（D32）

### 备份四步法

临时名生成（`VACUUM INTO 'snapshots/.tmp-…'`）→ 打开产物跑 `quick_check`
验证 → 记录快照清单（时间戳/大小/库版本）→ 原子改名发布。中断只废产物不伤
原库（官方语义）。Backup API（rusqlite `backup` feature）为备用通道；
sqlite3_rsync 不采用（Windows SSHD 官方未跑通）。

### 检测四层

1. **L-open**：每次打开 `quick_check` + `foreign_key_check`
   （integrity_check 不查外键——官方原文）；
2. **L-backup**：备份产物验证（四步法第 2 步）；
3. **L-reconcile（审计对账）**：canonical 计数（runs 终态数）vs 事件流计数
   （terminal 事件数）不一致即抓「静默丢写」——SQLite 设计无腐化冗余，
   integrity_check 查不出从未落盘的提交；
4. **L-path**：同步目录 / 网络挂载路径告警（§3）。

全量 `integrity_check` 仅用户手动 / 备份前，且必须可中断（deadline + 部分
结果保留 + doctor 1 秒时限降级 Warning）。

### 恢复 L0–L3

L0 自动隔离（quarantine 重命名 + 持久台账）→ L1 `.recover` 页面级重组
（3.29.0+）→ L2 快照重放（点时间戳回退）→ L3 源 Markdown 全量重建兜底
（耗时上限由 pilot 重建基准定义，产品文案引用该数字）。

### 修复阶梯上限

每进程每路径一次 + 持久 `repair-attempts` 台账 + 跨进程修复锁——hermes
105 次/89GB 修复风暴事故的制度化防御。

## 5. 验收规格（阈值 = 待 pilot 确认的目标值）

| 指标 | 阈值 | 契约 |
|---|---|---|
| 200k chunk 负载 A 四类 p95 | ≤ 50ms | D29 |
| 冷启动首查 | ≤ 500ms | D29 |
| WAL 峰值 | ≤ 64MiB | D29 |
| 200k 全量重建 / 增量 1k chunk | ≤ 15min / ≤ 30s（commit 去抖 ≤10s） | D29/D30 |
| CJK 查询 p99（200k） | ≤ 100ms | D29 |
| quick_check / 归档后聚合 p95 | ≤ 2s / ≤ 100ms | D32/D30 |
| kill -9 ×1000 后 | 无 SQLITE_CORRUPT 悬挂、终态必达不变 | D32 |
| 快照生成（200k 档） | 耗时与体积实测记录（不设阈值，供 §2.4 预算） | D32 |

故障注入验收：kill -9 ×1000 损坏率统计 + Nemetz 2018 语料过检测层出检出率；
「真实断电损坏率」官方明确不可行，不做。

## 6. 明确不做（本轮新增，已并入 Architecture §21）

1. `.db/.wal/.shm` 与 Tantivy 索引目录进任何同步目录；
2. CRDT / 双向同步 / 多写者复制（重评触发见 §3.4）；
3. 对 canonical 审计表的任何 DELETE；run_events 按月分区表；
4. Litestream 常驻进程；sqlite3_rsync（Windows）；
5. auto_vacuum 预启；mmap 默认开；
6. 高频 commit（去抖间隔小于 5s）——性能与 Windows 正确性双重约束；
7. 引用未经验证的外部性能数字做容量依据。
