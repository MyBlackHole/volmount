# Journal P1 差距修复 — 原子保留状态 + 多 Buffer 流水线

## Goal

将 volmount Journal 从当前单 buffer 互斥模型重构为 bcachefs 对齐的原子保留状态 + 多 buffer 流水线模型，消除 Journal 写路径的全局互斥瓶颈。

## Confirmed Facts

### 当前 Journal 实现（`crates/volmount-core/src/journal/types.rs`）
- `pending: Vec<Jset>` 单队列，`flush()` 串行写入
- `reserve_seq()` 简单的 `self.last_seq += 1`
- 所有操作要求 `&mut self`（全局互斥）
- 四索引回收不变式已对齐 bcachefs

### bcachefs 目标
- `union journal_res_state`（atomic64_t）：cur_entry_offset(22b) + idx(2b) + 4×10b refcounts
- `JOURNAL_STATE_BUF_NR = 4`：四个 journal_buf 独立生命周期管理
- `journal_res_get_fast()`：CAS 循环实现无锁保留
- `seq: atomic64_t`：无锁递增
- `ring[seq & mask]`：保留路径直接访问 buf.data

### 依赖关系
- BtreeTransaction 已有 `journal: Vec<...>` 积累 journal 条目
- 当前 Journal 与 BtreeTransaction 的集成由调用者（Volume 层）管理
- Journal bucket 管理和四索引回收不变式保持不动（已对齐）

### 已有参考
- `docs/journal-design.md` — 完整 bcachefs Journal 设计文档（420 行）
- `.trellis/tasks/archive/2026-06/06-22-concurrency-research/research/journal_concurrency.md` — 596 行对比分析
- bcachefs-tools: `fs/journal/types.h`, `fs/journal/journal.h`, `fs/journal/journal.c`

## Requirements

- [ ] 替换 `Journal` 核心：`pending: Vec<Jset>` + `last_seq: u64` → atomic reservation state + multi-buffer
- [ ] 实现 `JournalReservation` 类型（bcachefs `struct journal_res`）
- [ ] 实现 `journal_res_get_fast()` + `journal_res_put()` fastpath
- [ ] 实现 `JOURNAL_STATE_BUF_NR` buffer 生命周期管理（open → accepting → close → write → free）
- [ ] 实现 `seq: AtomicU64` 无锁 seq 分配
- [ ] 实现新 Journal API：`reserve(req_u64s)` + `commit(res, data)` + `wait_all_done()`
- [ ] 按新设计重构 Journal 公开接口（不保留旧接口，需要改就改）
- [ ] 所有现有测试通过
- [ ] Journal bucket 管理和四索引回收不变式不修改

## Acceptance Criteria

- [ ] Journal 保留不再需要 `&mut self`（仅慢路径需要）
- [ ] `reserve()` 通过 fastpath 原子保留，不阻塞调用者
- [ ] 多 buffer 流水线支持 4 个未完成写入同时存在（`JOURNAL_STATE_BUF_NR=4`）
- [ ] buffer refcount 归零自动触发写入（不再依赖显式 flush）
- [ ] BtreeTransaction 集成新 reservation API
- [ ] `cargo test` 通过，Journal 相关测试全部通过
- [ ] bcachefs `journal.h` 引用标注精确行号

## Out of Scope

- Watermark 系统（后续 P1）
- Pin FIFO per-entry 管理（后续 P1）
- journal_keys overlay（后续 P1）
- 后台回收线程（P2）
- must_flush/noflush 区分（P2）
- 多设备 journal 复制（P3）
