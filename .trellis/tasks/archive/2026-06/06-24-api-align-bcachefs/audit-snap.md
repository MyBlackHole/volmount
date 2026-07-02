# 快照 API 差距分析：volmount-core vs bcachefs kernel

**日期**: 2026-06-24
**范围**: `volmount-core/src/snap/` ↔ `bcachefs-tools/fs/snapshots/`
**源文件**: format.h, types.h, snapshot.h, snapshot.c, check_snapshots.c

---

## 1. 类型定义对比

### 1.1 磁盘格式（On-Disk）

| 维度 | bcachefs `struct bch_snapshot` | volmount `SnapshotT` | 差距 |
|------|-------------------------------|----------------------|------|
| **序列化** | `__le32` 字段, `bch_val v` 头部, 固定布局, LE | `bincode` + `serde`, 无显式字节序 | **P2** — 无字节序处理, 非 big-endian 兼容 |
| **parent** | `__le32 parent` | `pub parent: u32` | ✅ |
| **children[2]** | `__le32 children[2]` | `pub children: [u32; 2]` | ✅ |
| **subvol** | `__le32 subvol` | `pub subvol: u32` | ✅ |
| **tree** | `__le32 tree` | `pub tree: u32` | ✅ |
| **depth** | `__le32 depth` | `pub depth: u32` | ✅ |
| **skip[3]** | `__le32 skip[3]` | `pub skiplist: [u32; 3]` | ⚠️ 命名不同 (`skip` vs `skiplist`) |
| **btime** | `bch_le128 btime` (16 字节) | `pub created_at: i64` (8 字节) | **P2** — 精度不同: `bch_le128` 是 128 位时间戳, i64 是 Unix 秒 |
| **flags** | `__le32 flags`, 4 个 bitmask | `BchSnapshotFlags(u8)` + `deleted: bool` | **P2** — bcachefs 的 `DELETED` 是 flag 位; volmount 用独立 bool |
| **bitmap** | 无磁盘 bitmap (仅在内存 `snapshot_t.is_ancestor[]`) | `pub bitmap: u128` | **P2** — 磁盘上有无用的 bitmap 字段, 从未填充 |
| **value header** | `struct bch_val v` (btree 值头部) | 无, 直接序列化 | **P2** — 缺少 btree 值类型标签 |

### 1.2 内存格式（In-Memory）

| 维度 | bcachefs `struct snapshot_t` | volmount `SnapshotT` | 差距 |
|------|-----------------------------|----------------------|------|
| **存在性** | ✅ 有内存表 `snapshot_table` RCU 保护 | ❌ 无内存表, 每次从 btree 读取 | **P1** — 核心架构差异 |
| **state** | `enum snapshot_id_state {empty, live, deleted}` | `deleted: bool` | **P2** — 缺少 `empty` / `live` 区分 |
| **is_ancestor** | `unsigned long is_ancestor[BITS_TO_LONGS(128)]` | `bitmap: u128` 但从不写入 | **P1** — bitmap 查询路径缺失 |
| **访问方式** | RCU 无锁读 | `engine.get_entry_raw()` btree 读 | **P1** — 每个操作都是 btree I/O |

### 1.3 标志位对比

| bcachefs bitmask | volmount flag | 备注 |
|-----------------|---------------|------|
| `BCH_SNAPSHOT_WILL_DELETE` | `BchSnapshotFlags::WILL_DELETE` | ✅ |
| `BCH_SNAPSHOT_SUBVOL` | `BchSnapshotFlags::SUBVOL` | ✅ |
| `BCH_SNAPSHOT_DELETED` | `SnapshotT.deleted: bool` | **P2** — bcachefs 是 flag, volmount 是独立字段 |
| `BCH_SNAPSHOT_NO_KEYS` | `BchSnapshotFlags::NO_KEYS` | ✅ |

---

## 2. 函数签名对比

### 2.1 快照创建

| 维度 | bcachefs | volmount |
|------|----------|----------|
| **函数** | `bch2_snapshot_node_create(trans, parent, new_snapids, snapshot_subvols, nr_snapids)` | `create_snapshot_btree(engine, parent_id, subvol)` |
| **语义** | parent=0 → 创建根(1个id); parent>0 → 创建 2 个孩子 id, 当前节点变 interior | 始终创建 1 个子节点 |
| **子节点数** | 始终分配 2 个 (快照对) | 分配 1 个 |
| **parent 转换** | parent 节点 → interior (children 填满, subvol=0, SUBVOL 清除) | parent.children[0] = new_id, 但 parent 仍保持 SUBVOL |
| **tree 创建** | 独立 `bch2_snapshot_tree_create()` | 调用者管理 tree |
| **返回** | int (错误码) + out 参数 | `Result<u32, StorageError>` |
| **深度** | `bch2_snapshot_depth()` 查询内存表 | `parent.depth + 1` 读取 btree |
| **skiplist** | `bch2_snapshot_skiplist_get()` 随机选择, bubble sort 排序 | `build_skip_list_from_btree()` 按深度比例确定性选择 |

**差距**: **P1** — bcachefs 快照创建是原子 "1 变 2" 操作, volmount 是 "1 变 1"。这直接影响 COW 语义:
- bcachefs: 快照 = 创建 2 个孩子, 原节点变 interior, 两个子卷分别指向不同的孩子
- volmount: 快照 = 创建 1 个孩子, 原节点仍然是 leaf

### 2.2 祖先查询

| 维度 | bcachefs `__bch2_snapshot_is_ancestor()` | volmount `is_ancestor_from_btree()` |
|------|------------------------------------------|--------------------------------------|
| **参数顺序** | `(trans, id, ancestor)` — id 是后代, ancestor 是祖先 | `(engine, ancestor, descendant)` — 顺序相反 |
| **bitmap 优化** | ✅ `test_ancestor_bitmap()` 128 位 O(1) | ❌ 无 bitmap 路径 |
| **skiplist 语义** | `get_ancestor_below()`: skip[x] <= ancestor 则跳 | `snap.skiplist[x] <= ancestor && > current` 则跳 |
| **数据源** | 内存表 (RCU 无锁) | Snapshots btree (每次读) |
| **深度限制** | `ancestor >= IS_ANCESTOR_BITMAP(128)` 时才 skiplist | 无限 skiplist |
| **ancestor=descendant** | 返回 true | 返回 true ✅ |
| **ancestor <= descendant** | btree 遍历 | 返回 false (基于 parent_id > child_id) |
| **参数位置** | `(id, ancestor)` | `(ancestor, descendant)` — **与 bcachefs 相反** |

**差距**: **P1** — 无 bitmap 优化路径; 参数顺序与 bcachefs 相反; 每次读取 btree 性能差 100x+

### 2.3 Skiplist 生成

| 维度 | bcachefs `bch2_snapshot_skiplist_get()` | volmount `build_skip_list_from_btree()` |
|------|------------------------------------------|----------------------------------------|
| **策略** | 随机: `get_random_u32_below(s->depth)` | 确定性: depth/4, depth/2, depth*3/4 |
| **排序** | `bubble_sort(n->v.skip, 3, cmp_le32)` | 按 steps 顺序赋值, 天然递增 |
| **数据源** | 内存表 (O(1)) | btree 遍历 (O(depth)) |
| **调用时机** | 每次创建 snapshot 时调用 3 次 | 调用一次批量计算 3 个 |
| **seed** | 随机 (每个 skip 独立随机) | 固定比例策略 |

**差距**: **P2** — 确定性 vs 随机策略。随机更均衡但不可预测; 确定性可复现但可能导致 hash 冲突。

### 2.4 快照删除

| 维度 | bcachefs `bch2_snapshot_node_set_deleted()` | volmount `delete_snapshot_btree()` |
|------|---------------------------------------------|-----------------------------------|
| **标记** | SET_BCH_SNAPSHOT_WILL_DELETE + SET_BCH_SNAPSHOT_DELETED | `snap.deleted = true`, `flags.insert(WILL_DELETE)` |
| **skiplist 清理** | 不修改内存表 skip | `snap.skiplist = [0,0,0]`, `bitmap = 0` |
| **btree 写入** | btree 更新 | Whiteout 条目 |
| **触发清理** | `set_bit(BCH_FS_need_delete_dead_snapshots)` | 无, 需手动调用 `delete_dead_snapshots()` |

**差距**: **P2** — volmount 删除时清理 skiplist/bitmap 而 bcachefs 不清理 (因为内存表与磁盘格式分离)

### 2.5 批量死快照清理

| 维度 | bcachefs `bch2_delete_dead_snapshots()` | volmount `delete_dead_snapshots()` |
|------|------------------------------------------|------------------------------------|
| **流程** | 多阶段: 叶→内部, 异步后台线程 | 全量扫描 + HashMap, 同步 |
| **volume 检查** | 通过 subvol btree | 通过 `has_subvol()` 和 DFS 子树检查 |
| **内部节点处理** | 完整: `delete_dead_interior_snapshots` 阶段 | 无内部节点特殊处理 |
| **并发** | 后台线程 + workqueue | 同步阻塞 |
| **进度报告** | `progress_indicator` + sysfs | 无 |

**差距**: **P2** — volmount 清理是简化版, 缺少内部节点删除和后台异步支持

### 2.6 缺失的 bcachefs API

以下 bcachefs API 在 volmount 中无对应实现:

| API | 功能 | 严重性 |
|-----|------|--------|
| `bch2_snapshot_root()` | 从任意节点找到树根 | **P2** — 可手动实现但缺失 |
| `bch2_snapshot_parent()` | 安全的 parent 获取(含 debug depth 校验) | **P3** — 直接读 parent 字段 |
| `bch2_snapshot_nth_parent()` | 向上跳 N 步 | **P3** |
| `bch2_snapshot_is_leaf()` | 快照是否为叶节点 | **P3** — 有 `SnapshotT::is_leaf()` ✅ |
| `bch2_snapshot_is_internal_node()` | 内部节点检查 | **P3** — 有 `is_interior()` ✅ |
| `bch2_snapshot_live_descendent()` | 找到活着的后代 | **P2** — 删除流程所需 |
| `bch2_snapshot_has_children()` | 是否有子节点 | **P3** — 等价于 `is_interior()` |
| `bch2_snapshot_tree_lookup()` | 查询快照树元信息 | **P2** — `SnapshotTreeT` 已定义但未提供查询 API |
| `bch2_snapshot_tree_create()` | 创建快照树 | **P2** — 根快照创建未关联 tree |
| `bch2_snapshot_exists()` | 检查 snapshot ID 是否存在 | **P2** — 需 btree 读 |
| `bch2_snapshot_id_state()` | 查询快照状态 | **P2** — 无 `empty/live/deleted` 三态 |
| `bch2_check_snapshots()` | 一致性校验 + 修复 | **P2** — 完全缺失 |
| `bch2_snapshot_is_ancestor_early()` | 线性父遍历(恢复期回退) | **P3** — 可自行实现 |
| `bch2_get_snapshot_overwrites()` | 查找覆盖键的快照 | **P2** — 快照感知读必需 |

---

## 3. 调用约定对比

| 维度 | bcachefs C | volmount Rust | 差距 |
|------|-----------|---------------|------|
| **错误处理** | `int` 返回码, `bch_err()` 日志 | `Result<T, StorageError>` | ✅ Rust 更安全 |
| **事务** | `struct btree_trans *` 显式事务 | `&BtreeEngine` 隐式 | **P1** — 无事务回滚支持 |
| **可变性** | 所有写操作需 intent lock | `&mut BtreeEngine` | ✅ |
| **读操作** | `struct btree_trans *` + intent/read lock | `&BtreeEngine` | 语义不同但等效 |
| **内存分配** | 内核 GFP_KERNEL / kmalloc | Rust `Vec` / `HashMap` | ✅ |
| **并发保护** | RCU + mutex + percpu rwsem | 无 (依赖调用者) | **P1** — 无并发控制 |
| **调试断言** | `EBUG_ON()`, `BUG_ON()` 运行时检查 | `#[cfg(test)]` 单元测试 | **P2** — 无运行时不变式检查 |

---

## 4. 命名对比

| bcachefs | volmount | 评价 |
|----------|----------|------|
| `bch2_snapshot_node_create()` | `create_snapshot_btree()` | ✅ 合理简化 |
| `bch2_snapshot_node_set_deleted()` | `delete_snapshot_btree()` | ⚠️ bcachefs 不删, 只标记; volmount 名实匹配 |
| `__bch2_snapshot_is_ancestor()` | `is_ancestor_from_btree()` | ⚠️ 参数顺序相反 |
| `bch2_snapshot_skiplist_get()` | `build_skip_list_from_btree()` | **P3** — 功能不同, 命名合理 |
| `bch2_snapshot_lookup()` | `read_snapshot_value()` | ✅ 等价 |
| `bch2_snapshot_to_text()` | (SnapshotMeta) | **P3** — 无 to_text 工具 |
| `skip[3]` | `skiplist: [u32; 3]` | ⚠️ 字段名不同 |
| `btime` | `created_at` | ⚠️ 字段名不同 |
| `struct bch_snapshot` | `SnapshotT` | ⚠️ 命名不一致 |

---

## 5. 已知测试失败与 API 偏差关联

### `test_skip_list_ordered` (已知失败)

**测试断言**:
```rust
assert!(snap.skiplist[0] < snap.skiplist[1], ...);
assert!(snap.skiplist[1] < snap.skiplist[2], ...);
```

**根因分析**: `build_skip_list_from_btree()` 的步进点计算为:
- `skip[2] = (depth * 3) / 4` 步的祖先
- `skip[1] = depth / 2` 步的祖先
- `skip[0] = depth / 4` 步的祖先

从父链上溯时, 步数越多 ID 越大, 所以理论上 `skip[2] > skip[1] > skip[0]` (ID 越大表示越远的祖先)。

但 `steps` 数组的赋值顺序是 `[skip2_step, skip1_step, skip0_step]`, 而 `for (i, &step)` 的 `i` 索引 0→2 对应 `skip[0]`、`skip[1]`、`skip[2]`。也就是说:
- `i=0` 步数 = `(depth*3)/4` → 存入 `skip[0]` (应该是 skip[2])
- `i=1` 步数 = `depth/2` → 存入 `skip[1]` (正确)
- `i=2` 步数 = `depth/4` → 存入 `skip[2]` (应该是 skip[0])

**结论**: `steps` 数组的顺序与 `for` 循环索引匹配错误。`skip[0]` 本该存储最近祖先但存了最远祖先。虽然在 `is_ancestor_from_btree` 中尝试顺序是 `skip[2]→skip[1]→skip[0]` 所以实际上不影响查询正确性 (因为三个值都是有效祖先)，但有序断言失败。

**与 bcachefs 的关联**: bcachefs 通过 `bubble_sort(n->v.skip, 3, cmp_le32)` 排序保证有序, 不存在此问题。volmount 放弃了排序而改用固定步进, 但索引映射有 bug。

### `test_create_snapshot_subvolume` (已知失败)

可能原因: `create_snapshot_btree()` 创建 snapshot 时与 `subvol/ops.rs::create_snapshot()` 的交互逻辑未对齐 — 两者都试图创建 snapshot 节点, 但 bcachefs 的创建是一次事务原子操作。

---

## 6. 差距摘要

### P1 (高 — 功能正确性或性能有重大影响)

| # | 差距 | 影响 |
|---|------|------|
| 1 | **无内存 snapshot 表**: 每次祖先查询都读 btree | 性能: bcachefs O(1) RCU 读 vs volmount O(log n) btree I/O |
| 2 | **无 bitmap 祖先优化**: `is_ancestor` 128 位 bitmap 不存在 | 近距离祖先查询退化为 btree walk |
| 3 | **无事务支持**: bcachefs 所有操作在 btree_trans 中, 支持回滚 | 数据一致性风险; 无原子批量更新 |
| 4 | **快照创建语义不匹配**: bcachefs 创建 2 个子节点并转换 parent; volmount 创建 1 个 | COW 快照行为不一致 |
| 5 | **参数顺序相反**: `is_ancestor_from_btree(ancestor, descendant)` vs bcachefs `(id, ancestor)` | 接口细微不兼容 |
| 6 | **无并发控制**: volmount 无 RCU/lock 保护 | 多线程安全需调用者保障 |

### P2 (中 — 功能性缺失或次要不兼容)

| # | 差距 | 影响 |
|---|------|------|
| 7 | 无 `bch2_snapshot_tree_lookup()` / `create()` API | 快照树管理不完整 |
| 8 | 无 `bch2_snapshot_exists()` / `id_state()` | 快照状态查询有限 |
| 9 | 无 `bch2_check_snapshots()` 一致性校验 | 数据损坏检测缺失 |
| 10 | 无字节序处理 (LE/BE) | 跨端兼容性 |
| 11 | `btime` vs `created_at` 精度不同 | 时间戳截断 |
| 12 | 磁盘上 `bitmap: u128` 从不填充 | 浪费 16 字节 |
| 13 | 无 `bch2_snapshot_live_descendent()` | 删除流程不完整 |
| 14 | skiplist 确定性 vs bcachefs 随机 | 树均衡性差异 |
| 15 | 无运行时不变式检查 (`EBUG_ON`) | 调试效率低 |

### P3 (低 — 装饰性或文档性)

| # | 差距 | 影响 |
|---|------|------|
| 16 | 字段命名: `skip[3]` vs `skiplist: [u32; 3]` | 文档/代码追踪 |
| 17 | 无 to_text 打印函数 | 调试/日志不便 |
| 18 | `SnapshotT` 命名 vs `bch_snapshot` | 一致性 |

---

## 7. 与测试失败关联总结

| 已知失败 | 根因 | 是否 API 偏差导致 |
|---------|------|------------------|
| `test_skip_list_ordered` | `build_skip_list_from_btree()` 中 `steps` 数组与 `for` 循环索引不匹配 | **部分**: bcachefs 用 bubble sort 保证有序, volmount 尝试确定性不会索引错乱 |
| `test_create_multiple` | (subvol 测试) 需进一步排查 | — |
| `test_create_snapshot_subvolume` | (subvol 测试) 可能 snapshot 创建与 subvol 创建交互问题 | **可能**: bcachefs 的 "1 变 2" 语义与 volmount "1 变 1" 语义差异 |

---

*生成工具: manual audit of bcachefs-tools commit and volmount-core current state*
