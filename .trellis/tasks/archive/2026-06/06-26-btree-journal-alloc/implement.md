# 执行计划

## 批次划分

分 3 个子批次顺序执行，每个批次独立验证。

---

## Batch A: btree split 对齐修复 (P0 + P2.9)

### A1 — compact_fits 检查
- **文件**: `btree/btree.rs` line ~236-241
- **改动**: 在 `insert_multi()` 的 compact 重试路径中，添加 compact 有效性检查
- **逻辑**: 调用 `node.compact()` 后，检查 compact 后是否能容纳新 key（写对齐考虑）
- **参考**: bcachefs `bch2_btree_node_compact_fits()` (`interior.h:390-402`)

### A2 — 错误路径资源回滚
- **文件**: `btree/btree.rs` (`insert_multi`, `insert_routing_entry_at`)
- **改动**: split 失败时释放已分配的右节点（放回 cache），清理已插入的 routing entries
- **避免**: 新增 `RollbackGuard` 或类似 RAII 模式管理已分配节点

### A3 — format-aware split point
- **文件**: `btree/node.rs` `find_balanced_split()` (line ~973-1011)
- **改动**: 在确定 split 点时，模拟两侧的 packed size 而非只累加 entry_u64s
- **参考**: bcachefs `predict_split()` (`interior.c:2630-2695`)

### A4 — 75% 主动分裂阈值
- **文件**: `btree/btree.rs` `insert_multi()` (~line 606)
- **改动**: 在节点插入前检查使用率是否 > 75%，主动触发 split
- **参考**: `BTREE_SPLIT_THRESHOLD(c)` (`cache.h:189`)

### 验证
- `cargo test -p volmount-core --lib` (btree 相关测试全部通过)
- 新增 btree split 测试覆盖 compact_fits 边界

---

## Batch B: journal reclaim 对齐修复 (P1 + P2)

### B1 — seq 分配改为 per-entry
- **文件**: `journal/types.rs`
- **改动**: 
  - 将 seq 分配从 `journal_res_get_fast` (line ~811) 移到 `journal_cycle_locked`
  - 同一 entry 内多个 reservation 共享同一 seq
  - 修改 pin 的 key 从 seq (per-reservation) 变为 entry_seq
- **影响**: `bch2_journal_pin_*`、`update_last_seq`、`flush` 路径都需要调整
- **注意**: 这是最复杂的改动，可能涉及重新设计 pin_fifo 的数据结构

### B2 — reclaim 触发 btree flush
- **文件**: `journal/types.rs` 
- **改动**: `bch2_journal_reclaim()` 中，在推进 bucket 索引前，触发 associated 的 btree flush callbacks
- **整合**: 利用已有的 `JournalEntryPin.flush_callbacks`，在回收 seq 之前调用它们

### B3 — pin_copy/pin_drop/pin_flush
- **文件**: `journal/types.rs`
- **改动**: 为 `JournalEntryPin` 添加 copy/drop/flush 操作
- **参考**: bcachefs `reclaim.c:538-718`

### B4 — 错误处理增强
- **文件**: `journal/types.rs`
- **改动**: 添加 `JournalError::Stuck`, `JournalError::Full`, `JournalError::PinFull` 等细化类型
- 添加 `journal_error_check_stuck()` 等价逻辑（检测长期 stuck）

### 验证
- `cargo test -p volmount-core --lib` (journal 相关测试全部通过)
- 新增 journal reclaim 边界测试

---

## Batch C: alloc trigger 对齐修复 (P0 + P1)

### C1 — 事务原子性
- **文件**: `alloc/mod.rs`
- **改动**: `allocate_bucket_inner` 中 Alloc btree + Freespace btree 两步包装为"先验证 → 执行 → 失败回滚前一步"模式
- **限制**: 当前无 btree_trans，使用两步执行 + 手动回滚（如果第二步失败则逆向操作第一步）

### C2 — 版本号一致
- **文件**: `alloc/mod.rs` `bch2_trigger_extent()` + `allocate_bucket_inner()`
- **改动**: extent trigger 从 `BchVal` 中提取 ver 写入 BchAllocEntry，而非硬编码 0
- **影响**: 确保 alloc entry 的 version 与 freespace 中的 gen 同步

### C3 — need_discard 状态
- **文件**: `alloc/mod.rs` `bch2_bucket_free()` + `alloc/btree.rs` BchAllocEntry
- **改动**: 
  - 在 `BchAllocEntry` 中添加 `NeedDiscard` state（或等效标志位）
  - `bch2_bucket_free` 将 state 设为 NeedDiscard 而非 Free
  - 新增 `bch2_bucket_do_trim()` 后台不阻塞地将 NeedDiscard 改为 Free

### C4 — freespace 重建覆盖 hole
- **文件**: `alloc/mod.rs` `bch2_rebuild_freespace()`
- **改动**: 遍历 Alloc btree 时，同时追踪 range hole（连续缺失的 bucket 范围），将这些范围内的 bucket 也插入 freespace

### 验证
- `cargo test -p volmount-core --lib` (alloc 相关测试全部通过)
- 验证 alloc trigger + freespace 一致性

---

## 执行顺序 & 依赖

```
Batch A (btree split) → Batch B (journal reclaim) → Batch C (alloc trigger)
       ↓                         ↓                         ↓
  A1 compact_fits           B1 seq per-entry           C1 事务原子性
  A2 错误回滚               B2 reclaim flush           C2 版本号一致
  A3 format-aware           B3 pin 操作增强            C3 need_discard
  A4 75% 阈值               B4 错误处理                C4 freespace hole
```

各 Batch 之间无代码依赖（影响不同文件），但建议顺序执行以简化验证。

## 验证总则

每子批次完成：
1. `cargo build` 通过
2. `cargo test -p volmount-core --lib` 通过
3. `cargo clippy --all-targets` 干净
4. 提交并附变更说明

全任务完成：
5. spec/quality-guidelines.md 补充本次修复的约定
6. task 归档
