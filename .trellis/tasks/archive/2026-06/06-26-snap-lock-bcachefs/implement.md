# Snap/Lock bcachefs 一致性修复 — 执行计划

## 执行顺序

```
D1 (P0, 极低风险) → D4 (P1-3, 基础设施) → D2 (P1-1, 依赖D4) → D3 (P1-2+P1-4, 独立)
```

每步完成后运行验证，通过后进入下一步。

---

## Step 1: D1 — 修复 SnapshotRef 序列化不匹配

**文件**: `subvol/ops.rs`
**改动**:
1. 删除 `SnapshotRef` 结构体定义（约 lines 189-205）
2. 替换 `bincode::deserialize::<SnapshotRef>` 为 `bincode::deserialize::<SnapshotT>`
3. 更新 `bch2_snapshot_get_subvol` 函数从 `SnapshotT` 提取 `.subvol` 字段

**验证**:
```bash
cargo build -p volmount-core 2>&1 | head -20
cargo test -p volmount-core --lib -- subvol::ops::tests 2>&1 | tail -30
cargo test -p volmount-core --lib -- snap:: 2>&1 | tail -30
```

---

## Step 2: D4 — 批量写入原子性

**文件**: `btree/` — 新增 `batch_write` 方法；`subvol/ops.rs` 和 `snap/snapshot.rs` — 改造调用点

**改动**:
1. 在 `BtreeEngine` 上添加 `batch_write(&self, btree_type: BtreeType, entries: &[(Bpos, EntryType, &[u8])]) -> Result<(), BtreeError>`
2. 改造 `bch2_subvolume_snapshot` 中的 `delete_entry` + `insert_entry_raw` 两步为 `batch_write`
3. 改造 `bch2_subvolume_delete` 中多步删除为 `batch_write`
4. 改造 `bch2_snapshot_node_create` 中 parent update + child insert 为 `batch_write`

**验证**:
```bash
cargo build -p volmount-core
cargo test -p volmount-core --lib -- subvol:: 2>&1 | tail -30
cargo test -p volmount-core --lib -- snap:: 2>&1 | tail -30
```

---

## Step 3: D2 — 双 child 创建

**文件**: `snap/snapshot.rs` (核心)、`subvol/ops.rs` (清理)

**改动**:
1. 修改 `bch2_snapshot_node_create` 签名：增加 `child2: Option<u32>` 参数
2. 实现双 child 逻辑：`children = [child1, child2]`
3. 简化 `bch2_subvolume_snapshot`：移除手动双 create + parent 修补，改为单次调用
4. 更新 `snap/mod.rs` 中的重导出（如适用）
5. 更新旧名别名函数签名

**验证**:
```bash
cargo build -p volmount-core
cargo test -p volmount-core --lib -- subvol::ops::tests 2>&1 | tail -30
cargo test -p volmount-core --lib -- test_create_snapshot_subvolume 2>&1
cargo test -p volmount-core --lib -- snap::snapshot::tests 2>&1 | tail -30
```

---

## Step 4: D3 — Skiplist 随机祖先 + 指数步进

**文件**: `snap/snapshot.rs`

**改动**:
1. 重写 `bch2_snapshot_skiplist_get`：按 bcachefs 风格返回 `[parent, parent.skip[0], parent.skip[1]]`
2. 重写 `build_skip_list_from_btree`：使用新的 `bch2_snapshot_skiplist_get` 构建指数级 skip
3. 移除比例步进逻辑（`depth/4`, `depth/2`, `depth*3/4`）

**验证**:
```bash
cargo build -p volmount-core
cargo test -p volmount-core --lib -- snap::snapshot::tests 2>&1 | tail -30
cargo test -p volmount-core --lib -- test_skip_list_ordered 2>&1
cargo test -p volmount-core --lib -- snap::table::tests 2>&1 | tail -30
```

---

## Step 5: 全量回归

```bash
# 构建 + 完整测试
cargo build -p volmount-core
cargo test -p volmount-core --lib -- --skip storage 2>&1 | tail -20

# Clippy
cargo clippy -p volmount-core --all-targets 2>&1 | tail -20
```

## 回滚方案

每步独立 commit，出现问题时：
```bash
git checkout -- crates/volmount-core/src/  # 放弃全部改动
# 或
git revert <commit-hash>  # 回滚单步
```

## 验证命令汇总

```bash
# Step 验证
cargo build -p volmount-core
cargo test -p volmount-core --lib -- snap:: 2>&1 | tail -10
cargo test -p volmount-core --lib -- subvol::ops::tests 2>&1 | tail -10
cargo test -p volmount-core --lib -- test_skip_list_ordered 2>&1

# 全量
cargo test -p volmount-core --lib 2>&1 | grep "test result"
cargo clippy -p volmount-core --all-targets 2>&1 | grep -E "error|warning"
```
