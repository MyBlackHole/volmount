# Phase 5: gc P0 fixes

## Goal

实现 gc.rs 的 P0 功能 — GC generation 传递（P0-3）+ 拓扑/分配一致性检查（P0-4）。

## 确认事实

### volmount 当前状态
- `gc.rs` 210 行，核心函数均为骨架实现（返回 0/true）
- 数据结构已有：`GcPhase`, `GcPos`, `BtreeGc`（含 running/triggered 状态）
- 辅助函数已有：`gc_pos_cmp`, `gc_visited`, `bch2_gc_pos_to_text`
- 骨架函数：`bch2_gc_gens()`, `bch2_check_topology()`, `bch2_check_allocations()` 均返回 0
- 已有 5 个单元测试

### P0-3: `bch2_gc_gens()`
bcachefs 参考：遍历所有 btree 中的 extent → 对每个 extent 找到对应 bucket → 更新 generation 计数

### P0-4: `bch2_check_topology()` / `bch2_check_allocations()`
- check_topology: 验证 btree 内部节点 routing key 范围一致
- check_allocations: 验证 alloc btree 与实际 backend 分配一致

### 可用基础设施
- `BtreeEngine` — 节点/条目查询
- `crate::alloc::Allocator` — bucket 管理

## Requirements

1. **P0-3 (GC gens)**: `bch2_gc_gens()` 实现标记使用中的 bucket
2. **P0-4 (check topology)**: 验证 btree 节点间 key 范围一致性
3. **P0-4 (check allocations)**: 验证 alloc btree 与实际分配一致

## Acceptance Criteria

- [ ] P0-3: `bch2_gc_gens()` 遍历 Extents 并更新 bucket generation
- [ ] P0-4: `bch2_check_topology()` 验证 btree 节点 key 范围
- [ ] P0-4: `bch2_check_allocations()` 验证分配一致性
- [ ] 所有现有测试通过
- [ ] `cargo build` + `cargo test` + `cargo clippy` 通过

## Out of Scope

- 增量/并发 GC（P1），GC 后 bucket 回收（P1）
- btree node compaction（P1），GC pos 序列化（P2）
