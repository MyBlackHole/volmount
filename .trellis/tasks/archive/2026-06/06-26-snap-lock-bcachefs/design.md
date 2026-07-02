# Snap/Lock bcachefs 一致性修复 — 技术设计

## 概述

本设计针对 snap 模块的 1 个 P0 和 4 个 P1 差距进行修复。Lock 模块已无 P0/P1 差距，仅在涉及 btree 事务集成时做审查。

---

## D1: P0 — 修复 `bch2_snapshot_get_subvol` 序列化不匹配

### 当前问题

`subvol/ops.rs:189-209` 定义了本地 `SnapshotRef` 结构体，其字段与 `SnapshotT` 完全不匹配：

| `SnapshotRef` (当前) | `SnapshotT` (实际) |
|---------------------|-------------------|
| `flags: u32` | `state: SnapshotIdState` (enum，1 字节 tag + 1 字节 data) |
| `parent: u32` | `parent: u32` |
| `children: [u32; 2]` | `children: [u32; 2]` |
| `subvol: u32` | `subvol: u32` |
| `tree: u32` | `tree: u32` |
| `depth: u32` | `depth: u32` |
| `skip: [u32; 3]` | `skip: [u32; 3]` |
| `btime_lo: u64` | **`is_ancestor: u128`**（16 字节替代 8 字节） |
| `btime_hi: u64` (8 字节) | **`depth: u32`**（已出现在上面！实际位置偏移错了） |
| ... 后续字段缺失 | `btime: i64` (8 字节) |
| ... | `deleted: bool` (1 字节) |
| ... | `flags: BchSnapshotFlags` (1 字节) |

### 修复方案

```rust
// 之前：用不匹配的本地结构体反序列化
#[derive(Deserialize)]
struct SnapshotRef { /* 不匹配的字段 */ }
let snap_ref: SnapshotRef = bincode::deserialize(&snap_bytes).ok()?;
let subvol_id = snap_ref.subvol;

// 之后：直接用 SnapshotT 反序列化（SnapshotT 已 derive Deserialize）
let snap: SnapshotT = bincode::deserialize(&snap_bytes).ok()?;
let subvol_id = snap.subvol;
```

**文件**: `subvol/ops.rs`
**改动**: 删除 `SnapshotRef` 结构体定义，替换反序列化调用
**风险**: 极低 —— `SnapshotT` 已是 bincode 格式，且已有序列化/反序列化测试（`snap::meta::tests` 中 10 个测试覆盖 roundtrip）

---

## D2: P1-1 — 双 child 创建 (bcachefs "1变2" 语义)

### 当前流程

```
bch2_subvolume_snapshot():
  1. 分配新 snapshot ID (id_new)
  2. bch2_snapshot_node_create(parent, id_new)  → 创建 1 个 child，parent.children = [id_new, 0]
  3. 分配第二个 snapshot ID (id_src) 
  4. bch2_snapshot_node_create(parent, id_src)  → 创建第 2 个 child，parent.children = [id_src, 0]（覆盖了 id_new!）
  5. 手动修复：
     读取 parent
     parent.children = [id_new, id_src]
     写回 parent                              → 脆弱的变通方案
```

### bcachefs 流程

```
create_snapids(parent, &id_new, &id_src):
  1. 在单次事务中创建两个子节点
  2. 设置 parent.children = [id_new, id_src]
  3. 初始化两个子节点的 skip list
```

### 修复方案

**方案**: 扩展 `bch2_snapshot_node_create` 支持可选双 child

```rust
/// bcachefs-aligned: 创建一个或两个快照子节点
/// 当 `child2` 为 Some 时，创建双 child 并设 parent.children = [child1, child2]
pub fn bch2_snapshot_node_create(
    engine: &BtreeEngine,
    parent_id: u32,
    child1: u32,
    child2: Option<u32>,  // 新增参数
) -> Result<(), BtreeError> {
    // ... 读取 parent ...
    
    let children = match child2 {
        Some(c2) => [child1, c2],
        None => [child1, 0],  // 向后兼容
    };
    
    // ... 更新 parent.children, 写入 child1（和可选的 child2） ...
}
```

然后简化 `bch2_subvolume_snapshot`：

```rust
// 替换原有的 2 次 create + 手动修复：
bch2_snapshot_node_create(engine, parent_id, id_new, Some(id_src))?;
// 后续 child snapshot 初始化
```

**文件**: `snap/snapshot.rs`, `subvol/ops.rs`
**改动**: 修改函数签名 + 实现，简化调用方
**兼容**: 向后兼容 —— `None` 保持现有行为
**风险**: 中 —— 需验证 `bch2_subvolume_snapshot` 测试和 `test_create_snapshot_subvolume`

---

## D3: P1-2 + P1-4 — Skiplist 随机祖先 + 指数步进

### 当前实现

`bch2_snapshot_skiplist_get` (snapshot.rs:242-252):
```rust
// bcachefs 使用随机祖先 ≈depth/2，这里简化取 parent
snap.parent
```

`build_skip_list_from_btree` (snapshot.rs:196-236):
```rust
// 比例步进：depth/4, depth/2, depth*3/4
let mut steps = [depth / 4, depth / 2, (depth * 3) / 4];
```

### bcachefs 实现

```
bch2_snapshot_skiplist_get(snapshot, id):
  s = snapshot[id]
  skiplist[0] = s.parent
  skiplist[1] = snapshot[skiplist[0]].skip[0]  // 祖父 = 父的 skip0
  skiplist[2] = snapshot[skiplist[0]].skip[1]  // 曾祖父 = 父的 skip1
```

skip list 在 bcachefs 中是在节点创建时递归构建的：
- 新节点创建时，`skip[0] = parent`
- `skip[1] = snapshot[parent].skip[0]`（父节点的 skip[0] = 祖父）
- `skip[2] = snapshot[parent].skip[1]`（父节点的 skip[1] = 曾祖父）

这保证 skip 分布天然是指数级的。

### 修复方案

**Step 1**: `bch2_snapshot_skiplist_get` 改为 bcachefs 风格：

```rust
pub fn bch2_snapshot_skiplist_get(
    engine: &BtreeEngine,
    id: u32,
) -> Result<[u32; 3], BtreeError> {
    let snap = read_snapshot_value(engine, id)?;  // 读当前节点
    
    let mut skiplist = [id; 3];  // 默认都指向自己
    
    skiplist[0] = snap.parent;
    
    if snap.parent != 0 && snap.parent != id {
        let parent_snap = read_snapshot_value(engine, snap.parent)?;
        skiplist[1] = parent_snap.skip[0];
        skiplist[2] = parent_snap.skip[1];
    }
    
    Ok(skiplist)
}
```

**Step 2**: `build_skip_list_from_btree` 使用新 `bch2_snapshot_skiplist_get`：

```rust
pub fn build_skip_list_from_btree(...) {
    // 遍历所有快照节点
    for entry in btree_scan(...) {
        let id = entry.key.snapshot;
        let skiplist = bch2_snapshot_skiplist_get(engine, id)?;
        // 写回 skiplist
        write_entry(engine, id, skip[0], skip[1], skip[2]);
    }
}
```

**文件**: `snap/snapshot.rs`
**改动**: 重写 `bch2_snapshot_skiplist_get` 和 `build_skip_list_from_btree`
**验证**: `test_skip_list_ordered` 和所有 table tests 必须通过
**风险**: 中 —— skip list 是 is_ancestor 的核心优化，需确保正确性

---

## D4: P1-3 — 批量写入原子性

### 当前问题

多处操作涉及多步 btree 写入，但没有原子性保证：
1. `bch2_snapshot_node_create`: insert child + update parent
2. `bch2_subvolume_snapshot`: delete source subvol entry + insert new subvol entry
3. `bch2_subvolume_delete`: delete subvol + delete snapshot node

### 修复方案

利用 `BtreeEngine` 已有的单 key 操作（`insert_entry_raw`, `delete_entry`, `for_each_entry`），添加 `batch_write` 方法：

```rust
impl BtreeEngine {
    /// 在同一个 btree 上执行批量写入，crash 一致性由 caller 保证
    /// 当前通过 journal commit 后真正持久化
    pub fn batch_write(
        &self,
        btree_type: BtreeType,
        entries: &[(Bpos, EntryType, &[u8])],
    ) -> Result<(), BtreeError> {
        let btree = self.get_btree(btree_type);
        // 在同一个 btree 上下文内顺序执行所有写入
        for (key, entry_type, data) in entries {
            match entry_type {
                EntryType::Insert => btree.insert_entry_raw(*key, data)?,
                EntryType::Delete => btree.delete_entry(*key)?,
            }
        }
        Ok(())
    }
}
```

然后改造调用点：
1. `bch2_subvolume_snapshot` 中 `delete_entry` + `insert_entry_raw` → 合并为 `batch_write`
2. `bch2_subvolume_delete` 中多步删除 → 合并为 `batch_write`
3. `bch2_snapshot_node_create` 中 parent update + child insert → 合并为 `batch_write`

**注意**: 这不能替代真正的 btree 事务（不支持跨 btree 原子性），但能减少单 btree 内崩溃不一致窗口。

**文件**: `btree/mod.rs`（或 `engine.rs`），`subvol/ops.rs`，`snap/snapshot.rs`
**改动**: 新增 `batch_write` 方法 + 改造 3 个调用点
**验证**: 所有 snap/subvol 测试必须通过

---

## D5: Lock 审查（如有需要）

Lock 模块自身已无 P0/P1 差距。但如果 D4 涉及的 `batch_write` 或双 child 创建涉及 BtreeTransaction 锁调用，需审查：
- `batch_write` 是否需要在 btree lock 保护下执行（当前 `insert_entry_raw` 内部已处理锁）
- 双 child 创建时锁的顺序是否正确

---

## 依赖关系

```
D1 (P0) ── 独立，可先做
D2 (P1-1) ── 依赖 D4（批量写入确保创建原子性）
D3 (P1-2+P1-4) ── 独立于 D2/D4（纯 skiplist 算法修改）
D4 (P1-3) ── 独立基础设施，优先做
```

最优执行顺序：**D1 → D4 → D2 → D3**
