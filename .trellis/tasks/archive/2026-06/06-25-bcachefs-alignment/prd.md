# 任务：bcachefs 核心模块完全对齐（Alloc / Btree / Journal / Lock / Volume / Snap / Subvol / Recovery）

## 目标

使 volmount-core 的 8 个核心模块在 API 命名、功能覆盖、行为细节上与 bcachefs C 源码完全一致对齐。

## 已确认事实（代码探索结果）

### 当前对齐进度概览

从 git 历史看，已完成大量 bcachefs API 命名对齐工作：
- alloc/btree/journal 模块类型重命名
- Watermark::NR + BtreeId::ALL 常量对齐
- snap/subvol 类型重命名
- storage/recovery 类型重命名
- BCH_DATA_TYPES 从 5→12 种扩展
- Volume 状态机
- Recovery passes_failing + restart_recovery
- Journal btree pin 回调
- Alloc disk_reservation + open_bucket sectors_free
- Lock should_sleep_fn waiter 参数
- Subvol↔snap 集成 Batch 2.4

### 各模块当前实现边界

| 模块 | 文件行数 | 已实现的 bcachefs 对齐内容 | 已知未覆盖项 |
|------|---------|--------------------------|------------|
| **alloc** | ~800 | BlockAllocator, AllocRequest, AllocGroup, DiskReservation, OpenBucketPool, WritePointPool, BchDataType(12种), AllocEntry, Freespace trigger, gc_trigger, Alloc btree 同步 | Need deeper analysis |
| **btree** | ~2000+ | Btree(6种BtreeId), BtreeNode, Bpos/BtreeKey, BchVal, BtreeTrans, TriggerRegistry, SnapshotTree, KeyCache, Checkpoint, BucketIO, BtreeIter | Need deeper analysis |
| **journal** | ~500 | Journal 双缓冲流水线, Jset/JsetEntry, JournalReplayer, btree pin 回调, seq 分配 | Need deeper analysis |
| **lock** | ~500+ | SixLock(3状态), WaitFifo, DeadlockDetector, try→spin→sleep 三级等待 | Need deeper analysis |
| **volume** | ~700 | Volume 聚合容器, VolumeState 状态机, create/delete/rollback snapshot, create/delete subvol, write/delete extent | Need deeper analysis |
| **snap** | ~500 | Snapshot btree 操作, SnapshotMeta, BchSnapshotFlags, SnapshotTable | Need deeper analysis |
| **subvol** | ~300 | SubvolumeManager(create/delete/load/reparent), BchSubvolume, BchSubvolumeFlags | Need deeper analysis |
| **recovery** | ~400 | RecoveryState, 5 passes, JournalKeys overlay, fail-retry, restart_recovery | Need deeper analysis |

## 需求

- 8 个核心模块的公开 API 命名与 bcachefs C 源码一致（如 `bch2_alloc_*`, `bch2_btree_iter_*` 等的 Rust 移植）
- 功能点覆盖 bcachefs 对应模块的主要路径
- 行为细节对齐（错误处理、边角情况、内存序语义）
- 每个模块的测试覆盖对齐后的 API

## 验收标准

- [ ] 每个模块的公开 API 名称与 bcachefs 对应头文件/实现一致
- [ ] 核心功能路径完全覆盖（非所有边角细节，但主要路径不能缺）
- [ ] 现有测试全部通过（含已知 3 个预存失败测试）
- [ ] `cargo test -p volmount-core --lib` 通过
- [ ] `cargo clippy --all-targets` 干净

## 开放问题（需用户决策）

1. **对齐深度**：bcachefs 每个模块都非常复杂（例如 alloc 有优先级、discard、gc、balance 等子系统）。"完全对齐"的深度要求是什么？
   - A) API 命名 + 核心功能路径覆盖（≈ 80% 对齐）
   - B) 所有公开 API + 主要子功能（≈ 95% 对齐）
   - C) 逐函数逐字段完全复制（100%，内核级对齐）

2. **执行顺序**：8 个模块是并行推进还是逐个模块批处理？

3. **bcachefs 源码版本**：对齐目标基于哪个 bcachefs 版本/提交？

## Out of Scope

- fs 层 inode/目录/权限/xattr 等文件系统语义（user 明确表示不做 fs）
- NBD/HTTP/CLI 层（对齐目标限于 core）
- 性能基准测试（对齐完成后再做）
