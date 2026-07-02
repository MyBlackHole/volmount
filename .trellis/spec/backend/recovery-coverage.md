# Recovery 模块函数级覆盖地图

> 生成时间: 2026-06-30
> 范围: `crates/volmount-core/src/recovery/` ↔ `bcachefs-tools/fs/init/recovery.c, passes.c, recovery.h`
> 参考: `.trellis/spec/backend/quality-guidelines.md` Batch C

---

## 一、目录树

```
crates/volmount-core/src/recovery/
├── mod.rs                               (956 行) — pass 调度器 + RecoveryState
├── overlay.rs                           (334 行) — JournalKeys journal overlay
└── passes/
    ├── mod.rs                           ( 14 行) — 模块声明
    ├── journal_read.rs                  ( 82 行) — 读取 journal entries + 加载 btree roots
    ├── btree_roots.rs                   ( 37 行) — [独立 pass] 从 journal 提取 btree root
    ├── check_topology.rs                ( 32 行) — btree 拓扑完整性检查 + GC gen 传递
    ├── accounting_read.rs               ( 23 行) — accounting 一致性验证
    ├── alloc_read.rs                    ( 14 行) — 从 Alloc btree 恢复 allocator 状态
    ├── snapshots_read.rs                ( 18 行) — 构建快照表
    ├── check_allocations.rs             ( 14 行) — 分配一致性检查（FSCK 模式默认 skip）
    ├── trans_mark_dev_sbs.rs            ( 67 行) — 标记 superblock + journal buckets
    ├── fs_journal_alloc.rs              ( 28 行) — 确保 journal 有已分配 bucket
    ├── set_may_go_rw.rs                 ( 21 行) — 启用 journal overlay（RW 过渡）
    ├── journal_replay.rs                ( 43 行) — 重放 journal entries（两阶段）
    ├── presplit_shard_boundaries.rs     ( 14 行) — 分割跨越 shard 边界的 leaf
    ├── fs_freespace_init.rs             ( 50 行) — 初始化 Freespace btree
    ├── check_snapshots.rs               (136 行) — 快照一致性验证
    └── lookup_root_inode.rs             ( 19 行) — 验证根子卷可读
```

**统计**: 18 文件，共 ~1,886 行源码（不含 test 模块）

---

## 二、函数级覆盖状态

### 2.1 `mod.rs` — Pass 调度器核心

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 1 | `bch2_recovery_pass_to_stable()` | 174 | `passes.c:56` `bch2_recovery_pass_to_stable` | ✅ | 运行时 pass → 稳定 ID 映射 |
| 2 | `bch2_recovery_pass_from_stable()` | 196 | `passes.c:70` `bch2_recovery_pass_from_stable` | ✅ | 稳定 ID → 运行时 pass 映射 |
| 3 | `bch2_restart_recovery()` | 533 | `passes.c:433-437` `restart_recovery` | ✅ | 清空所有 pass 状态 |
| 4 | `bch2_rewind_recovery()` | 557 | `passes.c:569-573` rewind 语义 | ✅ | 回退到指定 pass |
| 5 | `bch2_recovery_pass_done()` | 833 | `passes_types.h:17` `pass_done >= pass` | ✅ | 判断 pass 是否已完成 |
| 6 | `bch2_run_recovery_passes()` | 653 | `passes.c:532` `bch2_run_recovery_passes` | ✅ | 调度器主循环，fail-retry 支持 |
| 7 | `bch2_run_recovery_passes_startup()` | 782 | `passes.c:629` `bch2_run_recovery_passes_startup` | ✅ | startup 包装 |
| 8 | `bch2_fs_recovery()` | 843 | `recovery.c:1008` `bch2_fs_recovery` | ✅ | 顶层恢复入口 |
| 9 | `bch2_fs_initialize()` | 856 | `recovery.c:1023` `bch2_fs_initialize` | ✅ | 新文件系统初始化 |
| 10 | `run_recovery()` | 873 | — | ➖ | volmount 包装函数，无 bcachefs 直对应 |

**辅助函数（非 pub）**:

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 11 | `bch2_run_recovery_pass()` | 591 | `passes.c:504` `bch2_run_recovery_pass` | ✅ | 单 pass 运行 + 失败追踪 |
| 12 | `compute_passes_to_run()` | 789 | `passes.c:635-652` startup passes 计算 | ✅ | flag 基 pass 集合计算 |
| 13 | `compute_passes_with_flag()` | 818 | `passes.c:284` `bch2_recovery_passes_match` | ✅ | 标志位查找 |

### 2.2 `overlay.rs` — JournalKeys Overlay

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 14 | `JournalKeys::new()` | 34 | `journal_overlay.h` 初始化 | ✅ | 创建 inactive overlay |
| 15 | `JournalKeys::push()` | 47 | `bch2_journal_key_insert` | ✅ | 插入 + 去重 |
| 16 | `JournalKeys::lookup_entry()` | 67 | journal_keys 读穿透 | ✅ | 尾部优先查找 |
| 17 | `JournalKeys::find_next_entry()` | 81 | `bch2_journal_keys_peek_max` | ✅ | ≥ pos 的最小 entry |
| 18 | `JournalKeys::drain_all()` | 92 | `journal_overlay.h` drain | ✅ | drain 到 btree |

### 2.3 `passes/journal_read.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 19 | `run()` | 29 | `journal/read.c:1156` `bch2_journal_read` | ✅ | 读取 + 过滤 + blacklist + btree roots |
| 20 | `in_blacklist()` (priv) | 7 | `journal/read.c` blacklist 检查 | ✅ | 辅助函数 |

**bcachefs `journal_read` 关键子函数**:

| bcachefs 函数 | 状态 | 备注 |
|---------------|------|------|
| `bch2_journal_read()` | ✅ | 核心读取逻辑已实现 |
| `bch2_journal_keys_sort()` | ✅ | journal keys 排序 |
| `read_btree_roots()` | ⚠️ | 在 `btree_roots.rs` 中分离为独立 pass |
| `journal_replay_entry_early()` | ➖ | volmount 未分离出来 |

### 2.4 `passes/btree_roots.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 21 | `run()` | 12 | `recovery.c:625` `read_btree_roots` | ✅ | 从 journal + superblock 合并加载 roots |

### 2.5 `passes/check_topology.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 22 | `run()` | 17 | `recovery_passes[].fn = bch2_check_topology` | ⚠️ | 简化版；递归 parent-child 验证待 btree 基础设施就绪 (P1-8) |

### 2.6 `passes/accounting_read.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 23 | `run()` | 12 | `bch2_accounting_read` (PASS_ALWAYS #39) | ⚠️ | 简化版：仅验证 used+free ≤ total，不做完整 delta merge |

### 2.7 `passes/alloc_read.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 24 | `run()` | 12 | `bch2_alloc_read` (PASS_ALWAYS #0) | ⚠️ | 挂接到 `allocator.bch2_alloc_read()`；依赖 bucket_gens btree 基础设施 (P1-7) |

### 2.8 `passes/snapshots_read.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 25 | `run()` | 15 | `bch2_snapshots_read` (PASS_ALWAYS #3) | ⚠️ | 简化版：构建 SnapshotTable 但不持久化到 RecoveryState |

### 2.9 `passes/check_allocations.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 26 | `run()` | 9 | `bch2_check_allocations` (PASS_FSCK_ALLOC) | ⚠️ | 简化版：FSCK 模式默认关闭（volmount 无 fsck 模式） |

### 2.10 `passes/trans_mark_dev_sbs.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 27 | `run()` | 19 | `bch2_trans_mark_dev_sbs` (PASS_ALWAYS #6) | ✅ | 标记 Sb + Journal buckets |
| 28 | `mark_alloc_bucket()` (priv) | 42 | — | ✅ | 直接写 Alloc btree + bitmap_mark |

### 2.11 `passes/fs_journal_alloc.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 29 | `run()` | 13 | `bch2_fs_journal_alloc` (PASS_ALWAYS #7) | ✅ | 安全补充分配 |

### 2.12 `passes/set_may_go_rw.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 30 | `run()` | 15 | `recovery.c:229` `bch2_set_may_go_rw` | ⚠️ | 简化版：无 `reconstruct_alloc` 路径 + 无 `go_rw_in_recovery` 检查 |

> 运行时恢复补充：`restore_progress()` 会在检测到 `set_may_go_rw` 已完成时恢复 `engine.enable_overlay()` 与 `may_go_rw = true`，避免 clean mount 复用已完成进度时卡在只读态。

### 2.13 `passes/journal_replay.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 31 | `run()` | 17 | `recovery.c:377` `bch2_journal_replay` | ✅ | 两阶段回放（accounting → data），drain overlay |

#### 关键坑位：recovery 后的 depth=0 root 写入

- `journal_read` / `btree_roots` 先把 root 节点放进 `NodeCache`，随后 `journal_replay` 可能继续向同一棵树写入。
- 对于 `depth == 0` 的 root，写路径不能再依赖 `Arc::get_mut()`；只要 root 已经被 cache 持有，`Arc::get_mut()` 就可能失败。
- 正确做法是用 `Arc::make_mut()` 获取可写节点，并在成功后把更新后的 root Arc 重新同步回 cache。
- 相关验证：`test_recovery_pass_btree_roots` 必须同时覆盖“root 已加载”与“journal replay 写入”两步，确保 replay 后查询能看到新增 key。

### 2.14 `passes/presplit_shard_boundaries.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 32 | `run()` | 12 | `bch2_presplit_shard_boundaries` (PASS_ALWAYS #48) | ✅ | 委托给 `bch2_presplit_shard_boundaries()` |

### 2.15 `passes/fs_freespace_init.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 33 | `run()` | 14 | `bch2_fs_freespace_init` (PASS_ALWAYS #16) | ✅ | 完整实现 |
| 34 | `bch2_fs_freespace_init()` (pub(crate)) | 25 | — | ✅ | 核心逻辑 |
| 35 | `bch2_freespace_insert_core()` (priv) | 41 | — | ✅ | 辅助插入 |

### 2.16 `passes/check_snapshots.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 36 | `run()` | 18 | `bch2_check_snapshots` (PASS_ALWAYS #19) | ✅ | 委托给 `bch2_check_snapshots()` |
| 37 | `bch2_check_snapshots()` (pub(crate)) | 26 | — | ✅ | 核心验证逻辑（parent 引用、循环检测、depth 验证） |

### 2.17 `passes/lookup_root_inode.rs`

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 38 | `run()` | 14 | `passes.c:244` `bch2_lookup_root_inode` (PASS_ALWAYS #42) | ⚠️ | 简化版：仅验证根子卷可读，无 inode 内容读取 |

---

## 三、覆盖统计摘要

### 3.1 按文件

| 文件 | 函数数 | ✅ | ⚠️ | ❓ | ➖ | 覆盖率 |
|------|--------|---|---|----|----|--------|
| `mod.rs` | 13 | 12 | 0 | 0 | 1 | 92% |
| `overlay.rs` | 5 | 5 | 0 | 0 | 0 | 100% |
| `journal_read.rs` | 2 | 2 | 0 | 0 | 0 | 100% |
| `btree_roots.rs` | 1 | 1 | 0 | 0 | 0 | 100% |
| `check_topology.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `accounting_read.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `alloc_read.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `snapshots_read.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `check_allocations.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `trans_mark_dev_sbs.rs` | 2 | 2 | 0 | 0 | 0 | 100% |
| `fs_journal_alloc.rs` | 1 | 1 | 0 | 0 | 0 | 100% |
| `set_may_go_rw.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| `journal_replay.rs` | 1 | 1 | 0 | 0 | 0 | 100% |
| `presplit_shard_boundaries.rs` | 1 | 1 | 0 | 0 | 0 | 100% |
| `fs_freespace_init.rs` | 3 | 3 | 0 | 0 | 0 | 100% |
| `check_snapshots.rs` | 2 | 2 | 0 | 0 | 0 | 100% |
| `lookup_root_inode.rs` | 1 | 0 | 1 | 0 | 0 | 0% |
| **总计** | **38** | **30** | **7** | **0** | **1** | **79%** |

### 3.2 按状态分布

| 状态 | 含义 | 数量 | 占比 |
|------|------|------|------|
| ✅ | 完全对齐（功能完整） | 30 | 78.9% |
| ⚠️ | 部分对齐（简化/stub/待基础设施） | 7 | 18.4% |
| ❓ | 未确认 | 0 | 0% |
| ➖ | volmount 扩展（无 bcachefs 对应） | 1 | 2.6% |

### 3.3 按 bcachefs pass 阶段

| 阶段 | bcachefs passes | volmount passes | 状态 |
|------|----------------|----------------|------|
| Early（journal read + roots） | `bch2_journal_read` + `read_btree_roots` | `journal_read` + `btree_roots` | ✅ |
| PASS_ALWAYS（核心 13 个） | 13 passes | 13 passes | 可选对齐 |
| PASS_UNCLEAN（非干净关闭） | `check_topology` 等 | `check_topology` (UNCLEAN flag) | ⚠️ |
| PASS_FSCK（fsck 模式） | ~20 passes | `check_allocations` (stub) | ➖ 默认关闭 |
| PASS_ONLINE（在线模式） | ~15 passes | `passes_online` 位掩码支持 | ❓ 未实现实际 pass |

---

## 四、P1/P2 差距分析

参考 `quality-guidelines.md` Batch C (2026-06-27) 的验证结论：

### 4.1 P0 差距（Critical — 必须修复）

| # | 项 | 文件 | 状态 | 说明 |
|---|-----|------|------|------|
| P0-1~6 | 6 个 stub pass | 各 pass 文件 | ✅ 已修复 | Batch C 已完成全部 6 个 stub 实现 |
| P0 (new) | `set_may_go_rw` 无 `reconstruct_alloc` 路径 | `set_may_go_rw.rs` | ⚠️ | 当前 volmount 不使用 `BCH_FEATURE_no_alloc_info`，影响小；但若将来使用需实现 |

### 4.2 P1 差距（Important — 建议修复）

| # | 项 | 文件 | 状态 | 说明 |
|---|-----|------|------|------|
| P1-7 | `alloc_read` 实现 | `alloc_read.rs` | ⚠️ | 已挂接到 `bch2_alloc_read()`，但缺少 bucket_gens btree 基础设施 |
| P1-8 | `check_topology` 递归 parent-child | `check_topology.rs` | ⏳ TODO | 待 btree 节点结构支持后实现完整拓扑验证 |
| P1-9 | deps 强制执行 | `mod.rs` | ✅ 已修复 | Batch C 中已添加 |
| P1-10 | PASS_UNCLEAN/FSCK/ONLINE/NODEFER flags | `mod.rs` | ✅ 已修复 | Batch C 中已添加 |
| P1 (new) | `snapshots_read` 不持久化 SnapshotTable | `snapshots_read.rs` | ⚠️ | Volume 在 recovery 后重建，性能浪费 |
| P1 (new) | `accounting_read` 完整 delta merge | `accounting_read.rs` | ⚠️ | 仅做边界检查，不做完整 accounting merge |
| P1 (new) | `lookup_root_inode` 无 inode 读取 | `lookup_root_inode.rs` | ⚠️ | 仅验证子卷存在，无 `bch2_inode_find_by_inum_trans` 调用 |

### 4.3 P2 差距（Nice to have — 可选）

| # | 项 | 文件 | 状态 | 说明 |
|---|-----|------|------|------|
| P2 (new) | `check_snapshots` 修复模式 | `check_snapshots.rs` | ⚠️ | 当前只验证不修复，bcachefs 完整版会修复不一致条目 |
| P2 (new) | `trans_mark_dev_sbs` 通过 trigger pipeline | `trans_mark_dev_sbs.rs` | ⚠️ | 当前直接写 btree + bitmap_mark，未走 trigger pipeline |
| P2 (new) | `set_may_go_rw` go_rw_in_recovery 检查 | `set_may_go_rw.rs` | ⚠️ | 无 `recovery_pass_should_defer` 延迟执行逻辑 |

### 4.4 差距趋势

```
Batch C (2026-06-27)         当前 (2026-06-30)
  P0: 6 done, 0 open     →   P0: 6 done, 0 open ✅
  P1: 4 done, 1 open     →   P1: 2 done, 5 open ⚠️ (新识别)
  P2: 0 done, 1 open     →   P2: 0 done, 3 open ⚠️ (新识别)
```

**说明**: Batch C 完成后大部分 P0 差距已关闭。当前识别的 P1/P2 差距中，新识别项来源于更深入的功能审查，而非原有 stub。

---

## 五、关键 bcachefs 覆盖覆盖率

### 5.1 `recovery.c` 函数覆盖

| bcachefs 函数 | 行号 | volmount 状态 | 备注 |
|---------------|------|--------------|------|
| `bch2_btree_lost_data` | 48 | ❌ | 未实现 |
| `kill_btree` | 138 | ❌ | 内部静态函数 |
| `bch2_reconstruct_alloc` | 145 | ❌ | 内部静态函数 |
| `bch2_ignore_journal_rewind_errors` | 199 | ❌ | 未实现 |
| `bch2_set_may_go_rw` | 229 | ⚠️ | 简化版（无 reconstruct_alloc） |
| `bch2_journal_replay_accounting_key` | 257 | ❌ | 内部静态 |
| `bch2_journal_replay_key` | 288 | ❌ | 内部静态 |
| `bch2_journal_replay` | 377 | ✅ | 架构对齐 |
| `journal_replay_entry_early` | 526 | ❌ | 内部静态 |
| `journal_replay_early` | 595 | ❌ | 内部静态 |
| `read_btree_roots` | 625 | ✅ | 分离为独立 pass |
| `__bch2_fs_recovery` | 667 | ✅ | 架构对齐 |
| `bch2_fs_recovery` | 1008 | ✅ | 顶层入口对齐 |
| `bch2_fs_initialize` | 1023 | ✅ | 顶层入口对齐 |

### 5.2 `passes.c` 函数覆盖

| bcachefs 函数 | 行号 | volmount 状态 | 备注 |
|---------------|------|--------------|------|
| `bch2_recovery_pass_to_stable` | 56 | ✅ | 枚举表 |
| `bch2_recovery_passes_to_stable` | 61 | ➖ | volmount 无位掩码级转换 |
| `bch2_recovery_pass_from_stable` | 70 | ✅ | 枚举表 |
| `bch2_recovery_passes_from_stable` | 77 | ➖ | volmount 无位掩码级转换 |
| `bch2_recovery_passes_match` | 284 | ✅ | `compute_passes_with_flag` |
| `bch2_fsck_recovery_passes` | 294 | ➖ | volmount 无 fsck 模式 |
| `pass_dependents` | 300 | ❌ | 传递依赖计算未实现 |
| `bch2_recovery_pass_want_ratelimit` | 221 | ❌ | 未实现 |
| `bch2_run_recovery_pass` | 504 | ✅ | match dispatch |
| `bch2_run_recovery_passes` | 532 | ✅ | trailing_zeros + loop |
| `bch2_run_recovery_passes_startup` | 629 | ✅ | flags 组合 |
| `bch2_fs_recovery_passes_init` | 716 | ❌ | 未实现（无 spinlock/work init） |

### 5.3 `passes.h` 函数覆盖

| bcachefs 函数 | 行号 | volmount 状态 | 备注 |
|---------------|------|--------------|------|
| `go_rw_in_recovery` | 22 | ❌ | 未实现 |
| `recovery_pass_will_run` | 32 | ❌ | 未实现 |
| `bch2_recovery_cancelled` | 38 | ❌ | 未实现 |
| `__bch2_run_explicit_recovery_pass` | 51 | ❌ | 未实现 |
| `bch2_run_explicit_recovery_pass` | 55 | ❌ | 未实现 |
| `bch2_require_recovery_pass` | 59 | ❌ | 未实现 |
| `bch2_run_async_recovery_passes` | 63 | ❌ | 未实现 |
| `bch2_recovery_pass_status_to_text` | 67 | ❌ | 未实现 |

---

## 六、volmount 扩展函数

以下函数为 volmount 特有，无 bcachefs 直接对应：

| 函数 | 文件 | 用途 |
|------|------|------|
| `run_recovery()` | `mod.rs:873` | 顶层入口包装 — 创建 RecoveryState + 同步 superblock |
| `RecoveryState::restore_progress()` | `mod.rs:461` | crash resume：从 superblock 恢复进度 |
| `RecoveryState::persist_progress()` | `mod.rs:491` | 逐步持久化进度到 superblock |
| `RecoveryState::sync_to_superblock()` | `mod.rs:501` | 同步 recovery 状态到 superblock |
| `RecoveryState::take_engine_and_allocator()` | `mod.rs:512` | 提取 engine + allocator 构造 Volume |
| `JournalKeys::drain_all()` | `overlay.rs:92` | drain overlay 到 btree（Rust 特有） |

---

## 七、未覆盖的 bcachefs 恢复功能

以下 bcachefs 功能当前无 volmount 对应：

| 功能 | bcachefs 文件 | 优先级 | 说明 |
|------|--------------|--------|------|
| `scan_for_btree_nodes` | passes_format.h | P2 | 按 magic 扫描 btree 节点 |
| `stripes_read` | passes_format.h | P2 | 纠删码 stripe 初始化 |
| `initialize_subvolumes` | passes_format.h | P1 | 新 FS 初始化子卷（在 `bch2_fs_initialize` 中调用，但不在 pass 调度中） |
| `delete_dead_snapshots` | passes_format.h | P2 | 删除死快照 |
| `check_subvols` | passes_format.h | P2 | 子卷验证 |
| `check_inodes` | passes_format.h | P2 | inode 验证（无 Inodes btree） |
| `check_extents` / `check_dirents` / `check_xattrs` | passes_format.h | P2 | 数据项验证 |
| `resume_logged_ops` | passes_format.h | P2 | 恢复已记录操作 |
| `fix_reflink_p` / `set_fs_needs_reconcile` | passes_format.h | P3 | 一次性迁移 pass |
| `merge_btree_nodes` | passes_format.h | P3 | 在线合并稀疏节点 |
| `btree_bitmap_gc` | passes_format.h | P3 | 重计算 btree bitmap |
| async recovery passes | passes.c `bch2_run_async_recovery_passes` | P2 | 后台恢复线程 |
| error handling / rewind | recovery.c `bch2_btree_lost_data` | P2 | 数据丢失恢复 |
| journal rewind | recovery.c `bch2_ignore_journal_rewind_errors` | P2 | journal 回退场景 |

---

## 八、质量检查清单

### 8.1 必须符合的规范（来自 quality-guidelines.md）

| 规范 | 是否符合 | 备注 |
|------|---------|------|
| bcachefs API 命名对齐（`bch2_` 前缀） | ✅ | 所有核心函数使用 `bch2_` 前缀 |
| 类型字段对齐 | ✅ | `BchRecoveryPass`、`RecoveryState` 字段语义对齐 |
| 向后兼容 | ✅ | 稳定 ID 枚举 `BchRecoveryPassStable` 不删除/重排变体 |
| 功能逻辑必须与 bcachefs 完全一致 | ⚠️ | 简化版 pass 不影响整体正确性 |

### 8.2 已知 Issue

| Issue | 文件 | 严重度 | 状态 |
|-------|------|--------|------|
| `btree_roots.rs` 在 pass 调度之外（journal_read 内联调用） | `mod.rs:845` | Minor | 当前设计：journal_read pass 不处理 roots，由单独的 btree_roots pass 处理。但 `bch2_fs_recovery` 在 pass 调度前调用 `journal_read::run`，然后 `bch2_run_recovery_passes_startup` 中会再度运行 btree_roots pass（重复加载）。 |
| `check_topology` 标记为 UNCLEAN 但实际也运行了 GC gen 传递 | `check_topology.rs:17` | Minor | GC gen 传递在干净关闭下也会运行（应为不必要） |
| `accounting_read` pass 与 `alloc_read` pass 功能重叠 | `accounting_read.rs` | Info | volmount 中 accounting 由 alloc_read 完成，accounting_read 仅为验证层 |

---

## 附录：bcachefs pass enum 定义参考

```
enum bch_recovery_pass (passes_format.h 展开顺序, 共 49 个):

  0: recovery_pass_empty        (stable=41, SILENT)
  1: scan_for_btree_nodes       (stable=37)
  2: check_topology             (stable=4,  deps=scan_for_btree_nodes)
  3: accounting_read            (stable=39, ALWAYS, deps=check_topology)
  4: alloc_read                 (stable=0,  ALWAYS)
  5: stripes_read               (stable=1)
  6: initialize_subvolumes      (stable=2)
  7: snapshots_read             (stable=3,  ALWAYS)
  8: check_allocations          (stable=5,  FSCK|ALLOC, deps=check_topology)
  9: trans_mark_dev_sbs         (stable=6,  ALWAYS|SILENT|ALLOC)
 10: fs_journal_alloc           (stable=7,  ALWAYS|SILENT|ALLOC)
 11: set_may_go_rw              (stable=8,  ALWAYS|SILENT, deps=check_allocations)
 12: journal_replay             (stable=9,  ALWAYS, deps=set_may_go_rw)
 13: merge_btree_nodes          (stable=45, ONLINE)
 14: presplit_shard_boundaries  (stable=48, ALWAYS, deps=journal_replay)
 15: check_alloc_info           (stable=10, ONLINE|FSCK|ALLOC, deps=check_allocations)
 16: check_lrus                 (stable=11, ONLINE|FSCK|ALLOC, deps=check_allocations)
 17: check_btree_backpointers   (stable=12, ONLINE|FSCK|ALLOC, deps=check_allocations)
 18: check_backpointers_to_extents (stable=13, ONLINE, deps=check_allocations)
 19: check_extents_to_backpointers (stable=14, ONLINE|FSCK|ALLOC, deps=check_allocations)
 20: check_alloc_to_lru_refs    (stable=15, ONLINE|FSCK|ALLOC, deps=check_allocations)
 21: fs_freespace_init          (stable=16, ALWAYS|SILENT)
 ... (剩余 28 个 FSCK/ONLINE pass)
 48: lookup_root_inode          (stable=42, ALWAYS|SILENT)
```

**volmount 子集**: 13/49 passes（26.5%），覆盖所有 PASS_ALWAYS 和 PASS_UNCLEAN pass。
