# Recovery API 差异分析：volmount-core vs bcachefs 内核

> 分析日期：2026-06-24
> 分析范围：`volmount-core/src/recovery/` ↔ `fs/init/{recovery.c, passes.c, passes_format.h, passes_types.h}` + `btree/journal_overlay_types.h`

---

## 1. Recovery Pass 枚举体系

### 规模与覆盖

| 维度 | bcachefs | volmount-core | 差异程度 |
|------|----------|---------------|----------|
| pass 总数 | **~50+**（`BCH_RECOVERY_PASS_NR`） | **5**（`RecoveryPass`） | 🔴 **严重不足** |
| stable ID 机制 | 双重枚举：运行时序号 + 持久化 stable ID | 无 stable ID，enum discriminant 即序号 | 🟡 需要引入 |
| pass 元数据 | `struct recovery_pass { fn, name, when, depends }` | `PassDescriptor { pass, flags, deps, name }` | 🟢 结构对齐良好 |

### bcachefs 完整 pass 列表（vs volmount）

bcachefs pass（按运行顺序）| volmount 映射 | 差异
---|---|---
`recovery_pass_empty` (占位) | ❌ 缺失 | 🟢 合理略过
`scan_for_btree_nodes` | ❌ 缺失 | 🟡 需要——拓扑修复依赖
`check_topology` | ❌ 缺失 | 🟡 需要——验证 btree 根/父子/边界
`accounting_read` | ❌ 缺失 | 🟡 需要——内存记账初始化
`alloc_read` | ✅ `AllocRead` | 🟢 对齐（但 volmount 当前是 no-op）
`stripes_read` | ❌ 缺失 | 🟢 合理略过（无纠删码）
`initialize_subvolumes` | ❌ 缺失 | 🟡 需要——新 FS 子卷初始化
`snapshots_read` | ❌ 缺失 | 🟡 需要——快照表加载
`check_allocations` (GC) | ❌ 缺失 | 🔴 需要——全 GC 标记-清扫
`trans_mark_dev_sbs` | ❌ 缺失 | 🟢 合理略过
`fs_journal_alloc` | ❌ 缺失 | 🟡 需要——journal bucket 分配
`set_may_go_rw` | ✅ `SetMayGoRw` | 🟢 对齐良好
`journal_replay` | ✅ `JournalReplay` | 🟢 对齐良好
`merge_btree_nodes` | ❌ 缺失 | 🟢 合理略过（优化）
`presplit_shard_boundaries` | ❌ 缺失 | 🟡 需要——shard locality
`check_alloc_info`~`check_dirents` (20+ fsck) | ❌ 全部缺失 | 🟡 可选（fsck 模式）
`resume_logged_ops` | ❌ 缺失 | 🟡 需要——操作日志恢复
`delete_dead_inodes` | ❌ 缺失 | 🟢 合理略过
`lookup_root_inode` | ❌ 缺失 | 🟡 需要——根 inode 可读性验证

**结论：volmount 覆盖率约 10/50（20%），但核心路径（journal read → root loading → alloc → set_may_go_rw → journal_replay）已对齐。**

---

## 2. 函数签名对比

| bcachefs | volmount-core | 差异分析 |
|----------|---------------|----------|
| `bch2_fs_recovery(struct bch_fs *)` | `run_passes(&mut RecoveryState) -> Result<(), StorageError>` | 🟢 架构不同但等价——bcachefs 通过 `struct bch_fs` 传递全局状态，volmount 通过显式 `RecoveryState` |
| `bch2_fs_initialize(struct bch_fs *)` | ❌ 缺失 | 🟡 **需要**——新 FS 初始化有独立流程 |
| `bch2_run_recovery_passes_startup(c, from)` | `run_passes(state)` | 🟡 bcachefs 支持 `from` 断点续跑、failfast、异步 defer |
| `bch2_run_explicit_recovery_pass(c, pass, ...)` | ❌ 缺失 | 🟡 **需要**——修复时按需重跑 pass |
| `bch2_set_may_go_rw(c)` | `set_may_go_rw::run(state)` | 🟢 逻辑对齐，但 bcachefs 有 `go_rw_in_recovery()` 条件判断 |
| `bch2_journal_replay(c)` | `journal_replay::run(state)` | 🟢 对齐良好 |
| `bch2_recovery_cancelled(c)` | ❌ 缺失 | 🟢 合理略过（单线程模型不需要） |
| `bch2_ignore_journal_rewind_errors(c)` | ❌ 缺失 | 🟢 合理略过（无 rewind 机制） |
| `bch2_btree_lost_data(c, msg, btree_id)` | ❌ 缺失 | 🟢 合理略过 |

---

## 3. RecoveryState 结构体对比

| bcachefs `struct bch_fs_recovery` | volmount `RecoveryState` | 差异 |
|------------------------------------|--------------------------|------|
| `current_passes: u64` | 隐含在 `run_passes()` 局部变量 | 🟡 不暴露，无法外部调度 |
| `current_pass: enum` | ❌ 缺失 | 🟡 缺少当前 pass 跟踪 |
| `pass_done: enum` | ✅ `pass_done: usize` | 🟢 对齐 |
| `passes_complete: u64` | ✅ `passes_complete: u64` | 🟢 完全对齐 |
| `passes_failing: u64` | ❌ 缺失 | 🟡 缺少失败 pass 跟踪 |
| `passes_ratelimiting: u64` | ❌ 缺失 | 🟢 合理略过（无 ratelimit） |
| `scheduled_passes_ephemeral: u64` | ❌ 缺失 | 🟡 需要——运行时可调度 |
| `rewound_from / rewound_to` | ❌ 缺失 | 🟢 合理略过（无 rewind） |
| `lock / run_lock / work` | ❌ 缺失 | 🟢 合理略过（单线程） |

### volmount 额外字段

| 字段 | 说明 | 评价 |
|------|------|------|
| `engine: BtreeEngine` | btree 引擎 | 🟢 合理（bcachefs 通过 `c->btree`） |
| `journal: Journal` | journal 实例 | 🟢 合理 |
| `backend: Arc<dyn BlockDevice>` | 块设备抽象 | 🟢 合理 |
| `superblock: BchSb` | superblock | 🟢 合理 |
| `jsets: Vec<Jset>` | 缓存的 journal entries | 🟢 合理——简化设计 |
| `recovered_roots: Vec<(BtreeId, u64)>` | journal 中提取的 root | 🟢 合理 |
| `replayed_seqs: Vec<u64>` | 已回放 seq 列表 | 🟡 bcachefs 用 `journal_replay_seq_start/end` + `blacklist_table` |

---

## 4. Pass 调度器对比

| 特性 | bcachefs | volmount-core | 差异 |
|------|----------|---------------|------|
| 位掩码迭代 | `__ffs64()`（`ctz`）+ `current_passes &= ~BIT_ULL(pass)` | `trailing_zeros()` + `passes_to_run &= !(1 << idx)` | 🟢 完全对齐 |
| pass 依赖解析 | `recovery_passes[i].depends` + `pass_dependents()` 传递闭包 | `PassDescriptor.deps` — 仅直接依赖 | 🟡 缺少传递依赖解析 |
| 条件运行 | `PASS_ALWAYS / PASS_UNCLEAN / PASS_FSCK / PASS_ONLINE / PASS_ALLOC` | `PassFlags::ALWAYS / UNCLEAN / SILENT` | 🟡 缺少 `PASS_FSCK`, `PASS_ONLINE`, `PASS_ALLOC`, `PASS_NODEFER` |
| 失败处理 | `passes_failing` — 同次迭代跳过，成功 pass 后清除 | 无重试/跳过 | 🟡 缺少失败重试逻辑 |
| 重跑pass（rewind） | `rewound_from/to` + `restart_recovery` 错误码 | ❌ 缺失 | 🔴 **关键缺失** |
| 异步 defer | `bch2_run_async_recovery_passes()` + workqueue | ❌ 缺失 | 🟢 合理略过（单线程足够） |
| 完成回调 | `bch2_sb_recovery_pass_complete()` — 更新时间/rate limit | `passes_complete |= / pass_done = max()` | 🟡 缺少 sb 持久化（volmount 后置到 `sync_to_superblock`） |
| 计时跟踪 | per-pass `last_run`/`last_runtime` 写入 superblock | ❌ 缺失 | 🟢 合理略过 |
| failfast 模式 | `bch2_run_recovery_passes(c, passes, **failfast**)` | ❌ 缺失 | 🟡 可以引入 |

---

## 5. Journal Replay / Overlay 对比

| 特性 | bcachefs (`journal_keys`) | volmount (`JournalKeys`) | 差异 |
|------|--------------------------|--------------------------|------|
| 数据结构 | gap buffer (`struct journal_keys`) | `VecDeque<OverlayEntry>` | 🟡 简单但 O(n) 查找 |
| overwritten 标记 | `journal_key.overwritten:1` + `overwrites` array | `OverlayEntry.overwritten: bool` | 🟢 概念对齐 |
| 排序 | `bch2_journal_keys_sort()` — 迁移 gap + 全排序 | 按 journal seq 追加 | 🟡 缺少显式排序 |
| 多级 key | `journal_key.level:8` | ❌ 缺失 | 🟢 合理（volmount 无 btree level） |
| `journal_seq_offset` | 相对于 `journal_entries_base_seq` 的偏移 | 显存 `journal_seq: u64` | 🟢 不同但等价 |
| 条件忽略 | `journal_replay_ignore()` — `ignore_blacklisted`, `ignore_not_dirty` | ❌ 缺失 | 🟡 需要——blacklist 过滤 |
| accounting 合并 | `bch2_journal_replay_accounting_key()` — 累加 delta | ❌ 缺失 | 🟡 需要——记账 delta 合并 |
| 并发安全 | `overwrite_lock` mutex + `ref` atomic | ❌ 无锁（单线程） | 🟢 合理 |
| rewind 支持 | `journal_key.rewind:1` | ❌ 缺失 | 🟢 合理 |
| 批量插入（journal replay） | 排序后 bulk insert → per-key fallback | `insert_entry_raw()` 逐个插入 | 🟡 缺少批量优化 |
| `drain_all()` 后状态 | `active=false`, `draining=false` | ✅ 相同 | 🟢 对齐 |
| `pre_sort` 条目 | 来自离线设备添加 | ❌ 缺失 | 🟢 合理略过 |

---

## 6. 错误处理对比

| 特性 | bcachefs | volmount-core | 差异 |
|------|----------|---------------|------|
| 错误传播 | `int` return + `bch2_err_throw()` + `bch2_fs_emergency_read_only()` | `Result<(), StorageError>` | 🟢 架构不同但等价 |
| 错误码体系 | Linux errno + bcachefs 扩展（`-BCH_ERR_restart_recovery`, `-recovery_cancelled`, `-recovery_pass_will_run`, `-cannot_rewind_recovery`） | `StorageError`（有限枚举） | 🔴 **需要扩展**——缺少 restart/rewind/cancelled 语义 |
| `errors_silent` | per-error bitmask（控制是否静默特定 fsck 错误） | ❌ 缺失 | 🟢 合理略过（无 fsck） |
| `BCH_FS_errors_fixed` | 标记修复是否发生 | ❌ 缺失 | 🟢 合理略过 |
| pass 失败跟踪 | `passes_failing` — 同次迭代跳过 | ❌ 缺失 | 🟡 **需要**——防止失败 pass 死循环 |
| ratelimit | per-pass 时间窗口控制 | ❌ 缺失 | 🟢 合理略过 |
| recovery cancelled | `bch2_recovery_cancelled()` — 检查 `BCH_FS_going_ro` + `kthread_should_stop()` | ❌ 缺失 | 🟢 合理略过（单线程） |

---

## 7. bcachefs `bch2_fs_recovery()` 完整流程 vs volmount

| 阶段 | bcachefs `__bch2_fs_recovery()` | volmount `run_passes()` | 差距 |
|------|--------------------------------|------------------------|------|
| 1. clean shutdown 检测 | ✅ 读取 clean sb，journal_seq | ❌ 缺失（外部调用方处理） | 🟢 接受——volmount 在调用前判断 |
| 2. journal 读取 | ✅ `bch2_journal_read()` | ✅ `joural_read::run()` | 🟢 对齐 |
| 3. 重读 rewind 区间 | ✅ `bch2_journal_reread_for_rewind()` | ❌ 缺失 | 🟢 合理略过 |
| 4. 清理标记 | ✅ 验证 clean-but-journal-not-empty | ❌ 缺失 | 🟡 可考虑 |
| 5. journal_rewind 选项 | ✅ `bch2_journal_add_rewind_range()` | ❌ 缺失 | 🟢 合理略过 |
| 6. blacklist 管理 | ✅ `bch2_journal_seq_blacklist_add()` | ✅ `extract_blacklist_entries()` | 🟢 概念对齐 |
| 7. journal 启动 | ✅ `bch2_fs_journal_start()` | ❌（外部完成） | 🟢 接受 |
| 8. journal_keys 排序 | ✅ `bch2_journal_keys_sort()` | ❌ 无排序 | 🟡 **待改进** |
| 9. btree 根读取 | ✅ `read_btree_roots(c)` | ✅ `btree_roots::run()` | 🟢 对齐 |
| 10. `set_btree_running` | ✅ `set_bit(BCH_FS_btree_running, ...)` | ✅ engine 加载根节点后自动就绪 | 🟢 对齐 |
| 11. 运行 recovery passes | ✅ `bch2_run_recovery_passes_startup(c, 0)` | ✅ `run_passes(state)` | 🟢 对齐 |
| 12. 设置 may_go_rw | ✅ `set_bit(BCH_FS_may_go_rw, ...)` | ✅ `engine.enable_overlay()` | 🟢 对齐 |
| 13. 异步 deferred passes | ✅ `bch2_run_async_recovery_passes()` | ❌ 缺失 | 🟢 合理略过 |
| 14. quota 读取 | ✅ `bch2_fs_quota_read()` | ❌ 缺失 | 🟢 合理略过 |
| 15. sb 最终化 | ✅ write_super + clear errors | ✅ `sync_to_superblock()` | 🟢 对齐 |

---

## 8. 严重性分级摘要

### 🔴 严重缺失（必须补齐）

| 缺失项 | 说明 | 影响 |
|--------|------|------|
| **Recovery pass 扩展** | 仅有 5/50+ passes，缺少 `check_allocations`（GC）、`snapshots_read`、`accounting_read` 等 | 核心功能缺失 |
| **Pass rewind 机制** | bcachefs 独有的重跑机制：通过 `restart_recovery` 错误码回滚后重跑 pass | 无法在运行时修复已发现的损坏 |
| **失败 pass 跟踪** | `passes_failing` 位掩码防止同一 pass 反复失败 | 死循环风险 |
| **错误码体系** | 缺少 `restart_recovery`、`recovery_pass_will_run`、`cannot_rewind_recovery` 语义 | 恢复流程无法表达控制流需求 |

### 🟡 中度缺失（推荐补齐）

| 缺失项 | 说明 |
|--------|------|
| `bch2_fs_initialize()` | 新 FS 初始化独立流程 |
| `PASS_FSCK` / `PASS_ONLINE` / `PASS_ALLOC` 标记 | pass 调度条件不够丰富 |
| 传递依赖解析 | 当前只检查直接依赖，不计算传递闭包 |
| 显式 journal_keys 排序 | 批量 replay 时排序保证 |
| `overwrite_lock` | 并发安全（如后续引入多线程） |
| accounting key 合并 | journal replay 中 delta 累加逻辑 |
| stable ID 双枚举 | 持久化 pass 标识避免 reorder 问题 |
| 从指定 pass 断点续跑 | `bch2_run_recovery_passes_startup(c, from)` 语义 |

### 🟢 合理略过或已对齐

| 项 | 说明 |
|----|------|
| `RecoveryState` ↔ `struct bch_fs_recovery` | 字段映射基本完成 |
| 位掩码迭代 | `trailing_zeros()` ↔ `__ffs64()` 完全对齐 |
| `PassDescriptor` ↔ `struct recovery_pass` | 结构体字段对齐良好 |
| `JournalKeys` ↔ `journal_keys` | 核心概念对齐 |
| `SetMayGoRw` pass 语义 | overlay 激活逻辑一致 |
| 单线程安全模型 | 当前无需并发保护 |

---

## 9. 关键设计差异附录

### 9.1 volmount 的简化假设

```rust
// volmount 的 RecoveryState 内嵌了所有依赖
// bcachefs 通过全局 struct bch_fs 访问
pub struct RecoveryState {
    pub engine: BtreeEngine,       // bcachefs: c->btree
    pub journal: Journal,          // bcachefs: c->journal
    pub backend: Arc<dyn BlockDevice>,  // bcachefs: c->ca[dev]->...
    pub superblock: BchSb,         // bcachefs: c->disk_sb
    pub allocator: BlockAllocator, // bcachefs: c->ca[dev]->alloc
    // ...
}
```

bcachefs 将 `struct bch_fs` 作为全局上下文，所有 subsystem 通过 `c->xxx` 访问。volmount 选择显式传递 `RecoveryState`，使依赖关系更清晰但要求调用方组装所有依赖。

### 9.2 bcachefs 的 per-pass 完成回调

```c
// bcachefs 在每个 pass 完成后异步回调：
static void bch2_sb_recovery_pass_complete(struct bch_fs *c,
    enum bch_recovery_pass pass, s64 start_time)
{
    // 1. 清除 recovery_passes_required bit
    // 2. 更新 last_run / last_runtime
    // 3. 检查 errors_silent（首次运行 FS 后清除）
    // 4. 写入 superblock
}
```

volmount 在 `run_passes()` 循环内仅更新内存状态，`sync_to_superblock()` 在全部 passes 完成后一次性写入。差异在于：bcachefs 在 dirty shutdown 时每个 pass 的完成状态都能持久化，volmount 要么全部完成要么全丢失。

### 9.3 journal overlay 激活时机

```
bcachefs:
  bch2_fs_recovery()
    → journal read / keys sort / btree roots
    → bch2_run_recovery_passes_startup(c, 0)
      → ... passes ...
      → set_may_go_rw    ← 按 schedule 运行
        → set_bit(BCH_FS_may_go_rw, &c->flags)
      → journal_replay   ← 依赖 set_may_go_rw
    → set_bit(BCH_FS_may_go_rw, &c->flags)  // 确保最终置位

volmount:
  run_passes(state)
    → passes::set_may_go_rw::run(state)  // engine.enable_overlay()
    → passes::journal_replay::run(state)  // replay + drain overlay
```

注意 bcachefs 在 `__bch2_fs_recovery()` 末尾额外设置了一次 `BCH_FS_may_go_rw`，确保即使 `set_may_go_rw` pass 未运行（例如 norecovery 模式），后续流程也能正常工作。volmount 缺少这层保险。

---

## 10. 推荐行动项（按优先级）

| # | 行动 | 工作量估计 | 依赖 |
|---|------|-----------|------|
| P0 | 引入 `passes_failing` 防死循环 | 小（~20 行） | 无 |
| P0 | 扩展 `StorageError` 含 `RestartRecovery` / `RecoveryPassWillRun` 错误码 | 小（~15 行） | 无 |
| P1 | 添加 `snapshots_read` pass（加载 snapshot btree 到内存表） | 中（~80 行） | snapshot 模块已实现 |
| P1 | 添加 `check_allocations` pass（GC 标记-清扫） | 大（~200 行） | alloc/bucket 模块就绪 |
| P1 | 添加 `accounting_read` pass | 中（~100 行） | accounting 模块 |
| P1 | 添加 `stable_id` 枚举与新 `PASS_FSCK`/`PASS_ONLINE` flags | 中（~60 行） | 无 |
| P1 | 实现传递依赖解析 | 小（~30 行） | 无 |
| P2 | `journal_keys_sort()` 等价实现——replay 前排序 | 中（~50 行） | 无 |
| P2 | `initialize` 子卷：`initialize_subvolumes` pass | 中（~100 行） | subvol 模块 |
| P2 | `bch2_fs_initialize()` 等价路径 | 中（~150 行） | 多个依赖 |
| P3 | `bch2_run_explicit_recovery_pass()` 按需重跑 | 中（~80 行） | rewind 机制 |
| P3 | per-pass sb 持久化（非一次性 sync） | 中（~60 行） | 无 |

---

> **总评估**: volmount-core 的 recovery 模块在**核心路径**（journal read → btree roots → alloc → set_may_go_rw → journal_replay）上对齐良好，PassDescriptor/RecoveryState/位掩码调度等基础架构设计正确。主要差距在 **pass 数量（5 vs 50+）** 和 **错误恢复机制（无 rewind/重试）** 。P0 项（失败跟踪 + 错误码）应在剩余对齐工作前优先补齐。
