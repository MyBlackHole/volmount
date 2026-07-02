# P0 功能逻辑修复 — 执行计划

## 执行顺序

1. **R2 — btree root level**（最简单，无依赖）
2. **J1 — CRC 全覆**（格式变更，需向后兼容）
3. **C1 — dirty.clear()**（独立，cache.rs 内修改）
4. **R1 — recovery 集成**（依赖 R2，最复杂）

---

## 1. R2 — btree root level 修复

**文件**: `recovery/btree_roots.rs`, `recovery/mod.rs`
**参考**: bcachefs `super-io.c:598-625` (`bch2_sb_btree_read`)

**操作**:
- `BtreeRoots` 结构增加 `levels: HashMap<BtreeId, u32>` 或并入 entry 结构
- `load_from_superblock()` 从 `BtreeRoot` 字段读取 level
- `get_root()` 返回 `(BtreeNodePtr, u32)` 含 level 信息
- 更新 `recovery/mod.rs` 中所有调用的适配

**验证**: `cargo build`, `cargo test -p volmount-core --lib`

---

## 2. J1 — CRC 全覆盖

**文件**: `journal/jset.rs`, `journal/types.rs`
**参考**: bcachefs `journal.c:378-412` (Jset 校验和)

**操作**:
- 扩展 `Jset::crc()` 范围包含 `magic + header fields + entries`
- 保持向后兼容：读取时尝试新 CRC 计算方式，失败回退旧方式
- 如果 `Jset` 包含 `version` 字段（目前无），可用 version 区分新旧格式

**验证**: 写入 Jset → 序列化 → 反序列化 → CRC 验证；现有测试通过

---

## 3. C1 — dirty.clear() 修复

**文件**: `btree/cache.rs`
**参考**: bcachefs `cache.c:247-260` (dirty list management)

**操作**:
- 当前 `if inner.dirty.len() >= MAX_DIRTY { inner.dirty.clear(); }` — 删除此路径
- 改为：`flush_all_dirty()` 遍历 dirty 集合，对每个节点调用写回
- 写回完成后 `dirty.clear()`
- 确保 flush 期间新加入的 dirty 节点不被影响（使用 `drain()` 替换 `clear()`）

**验证**: `cargo test -p volmount-core --lib`, clippy

---

## 4. R1 — recovery 集成

**文件**: `volume/mod.rs`, `recovery/mod.rs`
**参考**: bcachefs `recovery.c:241-310` (`bch2_fs_recovery`)

**操作**:
- 在 `Volume::new()` 中找到合适位置调用 `bch2_fs_recovery()`
- 处理 recovery 成功/失败路径
- 确保 `bch2_fs_recovery()` 不假定已初始化的成员（如 `Volume::new()` 已完成 superblock 加载后调用）
- 引入必要的 recovery pass 依赖（journal_read, alloc_read 等）

**验证**: Volume 创建 + recovery 执行流程通过

---

## 验证命令

```bash
# 每次修改后
cargo build -p volmount-core
cargo test -p volmount-core --lib
cargo clippy --all-targets
```

## 回滚计划

每个修改为独立原子 commit → 回滚只需 revert 单个 commit。
