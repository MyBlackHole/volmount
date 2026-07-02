# Phase 5: gc P0 修复 — 执行计划

## 范围

单文件修改：`crates/volmount-core/src/btree/gc.rs`

## 执行清单

### Step 1: P0-3 实现 (`bch2_gc_gens`)
- [ ] 变更签名：添加 `engine: &BtreeEngine, allocator: &mut Allocator` 参数，返回 `Result<(), StorageError>`
- [ ] 实现：遍历 engine 中所有有数据指针的 btree，收集 extent 引用的 paddr
- [ ] 对每个 paddr，在 allocator 中标记对应 bucket 为 allocated
- [ ] 更新 `gc.triggered` 状态标记

### Step 2: P0-4 拓扑检查 (`bch2_check_topology`)
- [ ] 变更签名：添加 `engine: &BtreeEngine` 参数，返回 `Result<(), StorageError>`
- [ ] 实现：对每个 btree 类型，从根节点递归遍历所有层级
- [ ] 验证内部节点 routing key 覆盖子节点范围
- [ ] 验证 leaf 节点内 key 有序

### Step 3: P0-4 分配检查 (`bch2_check_allocations`)
- [ ] 变更签名：添加 `engine: &BtreeEngine, allocator: &Allocator` 参数，返回 `Result<(), StorageError>`
- [ ] 实现：收集 extents 引用的 bucket，与 allocator 状态交叉验证

### Step 4: 测试
- [ ] 更新现有 5 个测试适配新签名
- [ ] 新增 `test_gc_gens_basic`
- [ ] 新增 `test_check_topology_basic`
- [ ] 新增 `test_check_allocations_basic`

## 验证命令

```bash
cargo build -p volmount-core
cargo test -p volmount-core
cargo clippy -p volmount-core
cargo test -p volmount-core -- btree::gc  # gc 专用
```

## 风险

1. `engine.for_each_entry()` — 需确认 volmount 引擎是否有迭代器 API。若无，可能需使用 `BtreeIter` 手动遍历
2. `BtreeNode::min_key/max_key` — 需确认节点 key 范围 API 是否存在
3. 签名变更可能影响 mod.rs 的 re-export
