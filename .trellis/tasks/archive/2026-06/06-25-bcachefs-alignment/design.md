# Design: bcachefs 核心模块完全对齐

## 参考源码

- bcachefs-tools: `/home/black/Documents/bcachefs-tools/`
- 核心源码目录: `fs/` (alloc/, btree/, journal/, init/, snapshots/, util/, sb/, data/ 等)
- volmount-core: `crates/volmount-core/src/` (alloc/, btree/, journal/, lock/, volume/, snap/, subvol/, recovery/)

## 各模块差距分析

### 1. lock (SixLock)

**参考文件**: `fs/util/six.c`, `fs/util/six.h`
**volmount 文件**: `lock/six.rs`, `lock/wait_fifo.rs`, `lock/deadlock.rs`, `lock/mod.rs`

**当前状态**: 已有基本骨架，对齐了 try→spin→sleep 三级等待，WaitFifo，死锁检测。

**bcachefs 完整 API (37 个入口)**:

| 类别 | bcachefs API | volmount 状态 |
|------|-------------|--------------|
| 生命周期 | `__six_lock_init`, `six_lock_exit` | 有 `SixLock::new()` 但缺 `exit` |
| 核心 trylock | `six_trylock_ip` (通用, 带 ip) | 有 `try_lock_read/intent/write` 但缺通用 trylock |
| 核心 lock | `six_lock_ip_waiter` (最通用，含 should_sleep_fn + waiter) | 有 `lock_*` 方法但缺 waiter 接口 |
|  contended | `six_lock_contended` (跳过初始 trylock) | **缺失** |
| relock | `six_relock_ip` (带 seq 检查) | **缺失** |
| unlock | `six_unlock_ip` (通用, 处理 recurse + seq) | 有 `unlock_*` 但细节可能不匹配 |
| downgrade | `six_lock_downgrade` | 有 `downgrade` |
| tryupgrade | `six_lock_tryupgrade` | 有 `try_upgrade` |
| trylock_convert | `six_trylock_convert` (read↔intent 通用转换) | **缺失** |
| increment | `six_lock_increment` (重入计数) | **缺失** |
| wakeup_all | `six_lock_wakeup_all` | **缺失** |
| counts | `six_lock_counts` (统计每种锁持有数) | **缺失** |
| readers_add | `six_lock_readers_add` (死锁恢复) | **缺失** |
| seq | `six_lock_seq` | 有 `seq()` |
| 宏生成类型化函数 | `six_trylock_read/intent/write`, `six_relock_*`, `six_unlock_*` 等 18 个函数 | 部分有，命名需对齐 |
| should_sleep_fn 签名 | `int (*)(struct six_lock *, struct six_lock_waiter *)` | 已有 `should_sleep_fn` 但缺 `waiter` 参数 |

**对齐工作量**: 中 (~10 个新增/修改函数)

### 2. btree

**参考文件**: `fs/btree/` (48 个文件)
**volmount 文件**: `btree/` (btree.rs, node.rs, key.rs, iter.rs, search.rs, transaction.rs, trigger.rs, types.rs, update.rs, op.rs, cache.rs, key_cache.rs, checkpoint.rs, bucket_io.rs, snapshot.rs, mod.rs)

**当前状态**: 已有 Btree/BtreeEngine/BtreeNode/BtreeTrans/BtreeIter/TriggerRegistry/KeyCache 等基本实现，6 种 BtreeId。

**bcachefs API 概览 (16 个子系统, 数百个函数)**:

| 子系统 | bcachefs 关键 API | volmount 状态 |
|--------|-----------------|--------------|
| **bkey 类型** | `struct bpos {inode,offset,snapshot}` / `struct bkey` / `struct bkey_i` / `struct bkey_s_c` / `struct bkey_s` / BCH_BKEY_TYPES 宏 | 有 `Bpos{vol_id,offset,snapshot}` / `BchVal` / `BtreeEntry` / `KeyValue` — 命名和字段布局需对齐 |
| **bpos 操作** | `bpos_eq/lt/le/gt/ge/cmp` / `bpos_successor/predecessor` / `SPOS_MIN/MAX` | 有 `Bpos::cmp/lt/le/gt/ge` 等，命名需对齐 |
| **bkey 打包/解包** | `bch2_bkey_pack_key/unpack_key` / `bch2_bkey_transform` / `bkey_format` | **未实现** (当前用 bincode 序列化) |
| **bset 操作** | `bch2_bset_init_first/next` / `bch2_bset_insert/delete` / `bch2_bset_build_aux_tree` / `btree_node_iter_*` | 部分有，需对齐命名和细节 |
| **btree_iter** | `bch2_path_get` / `bch2_btree_iter_peek/next/prev/peek_slot` / `for_each_btree_key` 宏族 | 有 `BtreeIter` + 基本 peek/next，缺完整 API surface |
| **btree_trans** | `__bch2_trans_get/put` / `bch2_trans_begin` / `bch2_trans_commit` / `lockrestart_do` / `commit_do` | 有 `BtreeTrans` (begin/journal_insert/journal_delete/commit_with_journal)，缺完整事务重启 |
| **更新** | `bch2_trans_update` / `bch2_btree_insert/delete/delete_range` / `bch2_bkey_make_mut` | 有 `insert_entry/delete_entry`，缺高级操作 |
| **锁** | `bch2_btree_node_lock/unlock/relock/upgrade` / `bch2_check_for_deadlock` | 部分有，需对齐命名 |
| **缓存** | `bch2_btree_node_get/get_noiter/prefetch/evict` / `bch2_btree_cache_cannibalize_lock` | 有 `NodeCache` + `Cache`，缺完整 cache 管理 |
| **内部节点** | `btree_split/merge/increase_depth` / `btree_node_rewrite/update_key` | **缺失** (split/merge 未实现) |
| **写** | `__bch2_btree_node_write` / `bch2_btree_node_write_trans` / `bch2_btree_init_next` | 有 `flush_dirty_nodes`，缺完整写路径 |
| **读** | `bch2_btree_node_read/root_read` / `bch2_validate_bset` / `bch2_btree_node_read_done` | 有 `load_root` / `bucket_io`，缺完整读路径 |
| **Journal overlay** | `bch2_journal_keys_peek_*` / `btree_and_journal_iter_*` / `journal_key_insert/delete` | 有 `JournalKeys` / `insert_guarded`，缺完整 overlay 迭代器 |
| **Key cache** | `bch2_btree_path_traverse_cached` / `bch2_btree_insert_key_cached` / `bch2_btree_key_cache_drop` | 有 `KeyCache`，缺完整接口 |
| **Write buffer** | `bch2_btree_write_buffer_flush_sync/tryflush` / `bch2_journal_key_to_wb` | **未实现** |
| **Sort/compact** | `bch2_key_sort_fix_overlapping` / `bch2_sort_repack` / `bch2_btree_node_sort/compact` | **未实现** |
| **Check/GC** | `bch2_check_topology` / `bch2_check_allocations` / `bch2_gc_gens` | **未实现** (有 `gc_trigger` 但无完整 GC passes) |
| **Init** | `bch2_fs_btree_exit/init/init_early/init_rw` | **未实现** |

**对齐工作量**: 极大 (~50+ 个新增/修改函数，非常复杂)

### 3. journal

**参考文件**: `fs/journal/` (journal.c, journal.h, journal_io.c, journal_reclaim.h 等)
**volmount 文件**: `journal/mod.rs`, `journal/types.rs`, `journal/jset.rs`, `journal/replay.rs`

**当前状态**: 已有 Journal 双缓冲流水线、Jset、JsetEntry、JournalReplayer、btree pin 回调。

**已知差距** (基于文件结构推断，需进一步分析):
- bcachefs `fs/journal/` 包含 journal_io.c, journal_reclaim.c, journal_reservations.c, journal_seq_blacklist.c 等
- 当前可能缺: journal reclaim (bucket 回收)、journal reservation 系统、seq blacklist、discard 支持、error 处理路径

**对齐工作量**: 大

### 4. alloc

**参考文件**: `fs/alloc/` (alloc.c, alloc.h, alloc_background.c, alloc_foreground.c, discard.c 等)
**volmount 文件**: `alloc/mod.rs`, `alloc/bucket.rs`, `alloc/btree.rs`, `alloc/open_bucket.rs`, `alloc/reservation.rs`, `alloc/write_point.rs`

**当前状态**: 已有 BlockAllocator (多 AG)、AllocRequest、DiskReservation、OpenBucketPool、WritePointPool、BchDataType(12种)、AllocEntry、trigger、Freespace 同步。

**已知差距**:
- bcachefs alloc 源码分散在 alloc.c, alloc_background.c, alloc_foreground.c, discard.c, freelist.c 等
- 当前缺: alloc 后台线程 (bucket 优先级/GC)、discard 支持、bucket 迁移、bucket 老化、bucket 优先级分桶、freelist btree 操作

**对齐工作量**: 大

### 5. snap (快照)

**参考文件**: `fs/snapshots/snapshot.c`, `fs/snapshots/snapshot.h`, `fs/snapshots/check_snapshots.c`
**volmount 文件**: `snap/mod.rs`, `snap/snapshot.rs`, `snap/meta.rs`, `snap/table.rs`

**当前状态**: 已有 Snapshot btree 操作 (create_root/create/delete/list/read/is_ancestor)、SnapshotMeta、BchSnapshotFlags、SnapshotTable。

**已知差距**:
- bcachefs snapshot 子系统涉及: snapshot tree 创建/删除、skip_list、祖先缓存、死快照清理、快照重命名、快照继承
- 当前已知 `test_skip_list_ordered` 测试失败
- 缺: 完整的 skip_list 实现、祖先缓存优化、死快照批量清理、快照子树删除

**对齐工作量**: 中-大

### 6. subvol (子卷)

**参考文件**: `fs/snapshots/subvolume.c`, `fs/snapshots/subvolume.h`
**volmount 文件**: `subvol/mod.rs`, `subvol/ops.rs`, `subvol/types.rs`

**当前状态**: 已有 SubvolumeManager (create/delete/load/reparent_children)、BchSubvolume、BchSubvolumeFlags。

**已知差距**:
- bcachefs subvolume 涉及: 子卷创建/删除/快照/clone、深度/宽度统计、UNLINKED 标记、WILL_DELETE 标记
- 当前 `test_create_multiple` 和 `test_create_snapshot_subvolume` 已知失败
- 缺: 完整快照创建子卷流程、子卷统计、子卷遍历

**对齐工作量**: 中

### 7. recovery

**参考文件**: `fs/init/recovery.c`, `fs/init/recovery.h`, `fs/btree/journal_overlay.c`
**volmount 文件**: `recovery/mod.rs`, `recovery/overlay.rs`, `recovery/passes/`

**当前状态**: 已有 RecoveryState、5 个 pass (JournalRead/BtreeRoots/AllocRead/SetMayGoRw/JournalReplay)、JournalKeys overlay、fail-retry、restart_recovery。

**已知差距**:
- bcachefs recovery 有更多 passes (如 check_allocations, check_gc, check_extents, check_dirents, check_xattrs 等 fs 层 pass)
- 但用户明确不做 fs 层对齐，所以 recovery pass 数量保持精简合理
- 需要对齐: pass 的 flags 语义、`pass_done` 持久化、`rewound_to` 机制、errors_to_text
- `restart_recovery` 已实现但可能缺 `bch_err_throw` 的完整语义

**对齐工作量**: 小-中

### 8. volume (卷管理)

**参考文件**: `fs/bcachefs.h` (主 fs 结构体), `fs/sb/` (superblock)
**volmount 文件**: `volume/mod.rs`

**当前状态**: 已有 Volume 聚合容器、VolumeState 状态机 (Created→Starting→Running→Stopping→Stopped)、快照/子卷操作、extent 写入/删除。

**已知差距**:
- bcachefs 的 `bch_fs` 结构体极其庞大 (含数百个字段)
- volmount 的 Volume 是轻量聚合容器，不直接对标 bch_fs
- 需要对齐: 状态转换的 `BCH_FS_*` 标志位语义、Volume 生命周期管理函数命名
- superblock 操作 (read/write) 在 `storage/superblock.rs` 中

**对齐工作量**: 小 (因为 Volume 角色设计本身就不对标 bch_fs)

## 分批并行计划

基于模块独立性和依赖关系:

### 批次 1 (无外部依赖，可完全并行)
- **lock** → 独立模块，不依赖其他模块
- **volume** → 独立模块，不依赖其他模块 (除 superblock)

### 批次 2 (依赖 lock 基础)
- **alloc** → 依赖 lock (SixLock 用于 alloc btree)
- **btree** → 依赖 lock (SixLock 用于 btree node 锁)

### 批次 3 (依赖 btree 基础)
- **journal** → 依赖 btree (journal btree pin 回调)
- **recovery** → 依赖 btree + journal + alloc
- **snap** → 依赖 btree (snapshot btree 操作)

### 批次 4 (依赖 snap/subvol)
- **subvol** → 依赖 snap (snapshot 指针)

## 模块依赖图

```
lock ──→ btree ──→ journal ──→ recovery
                     ↓
              alloc ──→ recovery
              
btree ──→ snap ──→ subvol
btree ──→ volume
lock  ──→ volume (不直接，但 engine 使用锁)
```
