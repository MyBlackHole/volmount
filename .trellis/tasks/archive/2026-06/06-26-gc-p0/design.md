# Phase 5: gc P0 修复 — 技术设计

## 架构边界

GC 子系统在 bcachefs 启动/恢复时运行一次，验证数据完整性：
- `bch2_gc_gens()`: 标记 pass — 遍历所有 extents，更新 bucket generation
- `bch2_check_topology()`: 验证 btree 结构一致性
- `bch2_check_allocations()`: 验证分配一致性

### 不修改的内容
- BtreeEngine、BtreeTrans、Journal、Allocator 的内部结构
- gc.rs 的现有数据结构（GcPhase、GcPos、BtreeGc）

### 修改的文件
- `crates/volmount-core/src/btree/gc.rs`（仅此一个）

## P0-3: `bch2_gc_gens()` 设计

### 签名变更
```rust
// 当前: pub fn bch2_gc_gens() -> i32
// 改为接收 engine 和 allocator:
pub fn bch2_gc_gens(engine: &BtreeEngine, allocator: &mut Allocator) -> Result<(), StorageError>
```

### 算法
```
1. 收集所有 btree 中的所有 extent 的 paddr
   - 遍历 engine 中所有有数据指针的 btree（Extents + 其他）
   - 对每个条目，记录 (BtreeId, paddr)
2. 对每个唯一的 paddr（bucket），标记 generation
   - 在 allocator 中查找对应 bucket
   - 调用 bucket.mark_allocated() 或更新 gen 计数
3. 返回 Ok(())
```

**简化**: 不对每个 bucket 做精确的 gen++，而是遍历 extents → 收集所有引用的 bucket → 标记为使用中。这是 GC mark 的核心语义。

## P0-4: `bch2_check_topology()` 设计

### 签名变更
```rust
pub fn bch2_check_topology(engine: &BtreeEngine) -> Result<(), StorageError>
```

### 算法
```
为每个 btree 类型:
  1. 获取根节点
  2. 递归遍历所有 btree 层级:
     - 内部节点: 验证 routing key 覆盖子节点 key 范围
     - Leaf 节点: 验证节点内 key 有序
  3. 返回发现的错误列表（或 Ok 通过） 
```

**简化**: 对 volmount 当前的小规模数据，使用简单递归遍历即可。不实现 bcachefs 的增量检查逻辑。

## P0-4: `bch2_check_allocations()` 设计

### 签名变更
```rust
pub fn bch2_check_allocations(engine: &BtreeEngine, allocator: &Allocator) -> Result<(), StorageError>
```

### 算法
```
1. 从 allocator 获取所有已分配的 bucket 列表
2. 遍历 Extents btree 中所有条目:
   - 记录每个引用到的 paddr/bucket
3. 对比:
   - 已分配但未被引用的 bucket → 可能泄漏
   - 被引用但未分配的 bucket → 不一致
   - 匹配的 → OK
4. 返回发现的差异
```

## 可重用基础设施

- `engine.get_entry(ty, key)` — btree 查询
- `engine.for_each_entry(ty)` — 遍历 btree 条目（需确认是否存在）
- `allocator.bucket(paddr)` — 获取 bucket 状态
- `Btree::depth()` / `Btree::root()` — 节点访问
- `BtreeNode` key 范围检查 — `node.min_key()` / `node.max_key()`（需确认）

## 测试策略

1. **test_gc_gens_basic**: 创建 engine + allocator，插入 extents，调用 gc_gens，验证 bucket gens 更新
2. **test_check_topology_basic**: 创建 btree，验证拓扑检查通过
3. **test_check_allocations_basic**: 验证 alloc 一致性检查
