# Phase 5: GC 模块实现 — 验证通过

## 完成情况

所有 14 项 P0 bcachefs 不一致（Phase 1-5）已全部修复。

### Phase 5 变更

1. **`recovery/passes/check_topology.rs`**: 集成 `bch2_gc_gens` 调用，匹配已有注释"同时包含 gc_gens"
2. **`recovery/passes/gc.rs`**: 删除死代码（从未在 mod.rs 注册，功能已拆分到 check_topology + check_allocations）

### 测试

- **btree::gc**: 13 passed ✅
- **全量**: 762 passed, 5 known fail, 9 ignored ✅（基线无变化）
- **clippy**: 无新增 warning ✅

## Acceptance Criteria

- [x] GC recovery pass 接线完成
- [x] 测试全部通过
- [x] 无新增 clippy warning
