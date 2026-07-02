# Snap/Lock bcachefs 一致性修复

## 背景

继续之前的 bcachefs 一致性修复工作。已完成的工作包括：
- **Lock 模块**：4 个并发 bug 修复（e7d220d）、RCU 化 WaitFifo + DeadlockDetector + WRITE_BIT + should_sleep_fn（816f7c5）、waiter 参数（49b01ec）
- **Snap 模块**：snap×3 + alloc×3 P0 修复（6b1e352）—— 含 `is_ancestor` 穿越 Whiteout、`bch2_subvolume_snapshot` 双 child 创建变通方案

当前状态：
- **Lock 模块**：已无 P0/P1 未对齐差距，所有 126 个锁测试通过
- **Snap 模块**：约 100 个 snap 相关测试全部通过，AGENTS.md 中 3 个预存失败已全部修复。但存在 1 个 P0 和 4 个 P1 真实差距未修复

## 修复范围

### Snap 模块（主要）

#### P0 — `bch2_snapshot_get_subvol` SnapshotRef 序列化不匹配

- **位置**: `subvol/ops.rs:189-209`
- **问题**: 本地 `SnapshotRef` 结构体有 8 个字段（`flags: u32`, `parent`, `children`, `subvol`, `tree`, `depth`, `skip`, `btime_lo/btime_hi`），而实际 `SnapshotT` 有 11 个字段（含 `state`, `is_ancestor: u128`, `deleted: bool`、`flags: BchSnapshotFlags(u8)`、`btime: i64`）。bincode 严格按字段顺序序列化，调用时将反序列化出错误数据。当前虽未被外部调用，但属于定时炸弹。
- **修复**: 删除 `SnapshotRef`，直接对 `SnapshotT` 使用 `bincode::deserialize` 然后取 `.subvol`

#### P1-1 — `bch2_snapshot_node_create` 只创建单子节点

- **位置**: `snapshot.rs:277-324`
- **bcachefs**: `create_snapids()` 创建两个 child（快照对），分别给新快照和源子卷
- **volmount**: 只创建一个 child，`bch2_subvolume_snapshot`（ops.rs:105-119）手动调用两次并修补 parent 数组——这是脆弱的变通方案
- **修复**: 扩展 `bch2_snapshot_node_create` 支持双 child 创建，消除变通方案

#### P1-2 — `bch2_snapshot_skiplist_get` 简化实现（返回 parent 而非随机祖先）

- **位置**: `snapshot.rs:242-252`
- **bcachefs**: 返回随机祖先 ≈depth/2，skip 指数级分布
- **volmount**: 返回 parent（depth/1），skip 线性分布
- **影响**: 大深度快照树（>100）祖先查询性能降低
- **修复**: 实现 bcachefs 风格的随机上溯逻辑

#### P1-3 — 多 key 写入无事务原子性

- **位置**: `snapshot.rs:307-321`、`ops.rs:108-129`、`ops.rs:236-248`
- **问题**: `bch2_snapshot_node_create` 更新 parent + 插入 child 分开执行；`bch2_subvolume_snapshot` 的 `delete_entry` + `insert_entry_raw` 两步无原子性；`bch2_subvolume_delete` 子卷删除与快照删除无事务包裹
- **影响**: 崩溃在两步之间会导致孤儿子节点或悬挂引用
- **修复**: 至少添加 BtreeEngine::batch_write（含多个 entry 的批量写入），优先确保原子性

#### P1-4 — Skiplist 指数步进构建

- **位置**: `snapshot.rs:196-236`
- **bcachefs**: `skip[0]=parent`, `skip[1]=parent->skip[0]`, `skip[2]=parent->skip[1]`（指数级 2^i 步进）
- **volmount**: `build_skip_list_from_btree` 按 `depth/4, depth/2, depth*3/4` 比例步进
- **影响**: 不同的 skip 分布导致祖先查询性能特征不同
- **修复**: 改为 bcachefs 模式的递归 skip 构建：`skip[0]=parent`, `skip[1]=parent->skip[0]`, `skip[2]=parent->skip[1]`

### Lock 模块

Lock 模块已无 P0/P1 未对齐差距。但若 snap 修复涉及 btree 事务集成，需审查 `BtreeTransaction` 中的锁调用链。

## 验收标准

- [ ] 修复 `bch2_snapshot_get_subvol` 序列化不匹配（P0）
- [ ] `bch2_snapshot_node_create` 支持 bcachefs 风格的双 child 创建（P1-1），消除 `bch2_subvolume_snapshot` 中的变通方案
- [ ] `bch2_snapshot_skiplist_get` 返回随机祖先（P1-2）
- [ ] Skiplist 改为指数级步进构建（P1-4）
- [ ] 多关键写入通过批量写入确保原子性（P1-3）
- [ ] 现有测试零回归（~650 tests passed / 0 新增失败）
- [ ] `cargo clippy -p volmount-core` 无新增警告

## 不包含的范围

- Lock 模块深度扩展（slot_idx O(1) 自移除、loom 模型检查）
- `bch2_delete_dead_snapshots` 扩展为扫描全 btree 清理死快照引用（P2）
- `SnapshotTable` 增量更新改造（P2）
- btree 事务层的完整事务集成
