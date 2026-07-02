# Phase 4: write_buffer P0 修复

## Goal

实现 volmount 的 btree write buffer 核心功能，使全部 6 项 P0 功能缺失（P0-5~10）得到修复。Write buffer 是 bcachefs 的延迟写入层，用于批量处理大量小更新（backpointers、LRU、accounting 等），通过排序+合并+批量 btree 插入来摊销 btree 遍历成本。

## 确认事实（来自代码探索）

### volmount 当前状态
- `write_buffer.rs` 全部 15+ 个公开函数均为骨架实现（返回 0/true 或空操作）
- `BtreeWriteBufferedKey` 使用 `Vec<u8>` 存储序列化 key（非对齐的 darray 风格）
- `BtreeWriteBufferKeys` 使用 `Vec<BtreeWriteBufferedKey>`（bcachefs 使用 flat darray_u64 + offset 计算的变长结构）
- `wb_key_cmp()` 恒返回 `Equal`
- `bch_wb_btree_idx()` 恒返回 `BchWbBtree::Accounting`
- 无 `write_buffer_insert` 调用点（update.rs 未连接 write buffer 路径）
- 无 journal pin 集成
- 无排序逻辑
- 无 flush worker 线程

### bcachefs 参考 (`write_buffer.c` ~1500 行)
- **`bch2_journal_key_to_wb()`** (write_buffer.h L179-192): 将 journal key 插入 write buffer。根据 key type 分派：
  - Accounting key → `bch2_accounting_key_to_wb()`（ezytnger 查找 + 累加）
  - 其他 → `__bch2_journal_key_to_wb()`（快速路径: 预留空间 + bkey_copy，慢路径: `bch2_journal_key_to_wb_slowpath`）
- **`bch2_btree_write_buffer_flush_locked()`** (L593-799): 核心 flush 管线：
  1. `move_keys_from_inc_to_flushing()` — 将 inc 中的 keys 移到 flushing
  2. 构建 `wb_key_ref[]` 排序索引数组（idx + bpos）
  3. `wb_sort()` — 按 bpos 排序
  4. pre-flush dedup — 相同 pos 的邻接条目合并/丢弃
  5. `wb_flush_sorted_sharded()` — 分片并行 fastpath（`wb_flush_one`）
  6. Slowpath — 按 journal_seq 重新排序，单线程 `btree_write_buffered_insert()`
  7. Pin drop / flush 清理
- **`wb_flush_one()`** (L184-258): 单 key flush：遍历 iter → 检查 noop → 节点写锁 → btree insert

### 关键数据结构差异
| 方面 | bcachefs | volmount | P0 |
|------|----------|----------|-----|
| Key 存储 | flat darray_u64 + offset 计算 | Vec<BtreeWriteBufferedKey> | P0-9 |
| 排序 | wb_sort / wb_key_ref | 无排序 | P0-10 |
| insert API | bch2_journal_key_to_wb | todo!() | P0-5 |
| flush 管线 | 完整 7 步 | todo!() | P0-6 |
| should_flush | 检查 inc/flushing 容量 | return false | P0-7 |
| need_flush | 基于 log write 状态 | return false | P0-8 |

### 依赖关系
- `write_buffer_flush` 依赖 journal seq 管理和 pin 机制
- `write_buffer_insert` 依赖排序后的 write buffer 数据结构
- journal pin 回调依赖 flush 逻辑
- 测试需要 journal 实例（现有 journal test 基础设施可用）

## Requirements

1. **P0-5 (write_buffer_insert)**: 实现 `bch2_journal_key_to_wb()` 的等效功能 — 将 journal key 正确插入 write buffer 的 inc 队列
2. **P0-6 (write_buffer_flush)**: 实现完整 flush 管线 — move_keys → sort → dedup → btree insert
3. **P0-7 (btree_write_buffer_should_flush)**: 基于 inc/flushing 容量判断是否需要 flush
4. **P0-8 (journal_write_buffer_need_flush)**: 基于 journal 写入状态判断是否需要 flush write buffer
5. **P0-9 (keys_to_write 数据结构)**: 对齐 bcachefs 的 darray 风格 flat 存储（至少提供兼容的 API）
6. **P0-10 (flush → btree insert 核心循环)**: 实现 `wb_flush_one` 和 `btree_write_buffered_insert` 的等效功能

## Acceptance Criteria

- [ ] P0-5: `bch2_journal_key_to_wb()` 正确将 key 插入 write buffer inc 队列，返回值正确
- [ ] P0-6: `bch2_btree_write_buffer_flush_locked()` 完成完整 flush 管线（move → sort → dedup → insert），返回成功
- [ ] P0-7: `bch2_btree_write_buffer_must_wait()` 基于实际容量返回正确值（已实现，verify）
- [ ] P0-8: `journal_write_buffer_need_flush()` 返回基于 write buffer 状态的正确判断
- [ ] P0-9: 数据结构对齐完成（至少 inc/flushing 使用兼容 flat 存储）
- [ ] P0-10: flush 循环正确遍历 sorted keys，调用 btree insert
- [ ] `cargo build` 通过，无新增警告
- [ ] `cargo test -p volmount-core` 全部 627+ 测试通过
- [ ] `cargo clippy -p volmount-core` 无新增警告

## Out of Scope

- Journal pin 集成（P1-22~25）— 设计为 P1，不在本阶段修复
- `bch2_btree_write_buffer_flush_sync` 完整实现（需要 btree_trans 集成）— 作为桩函数保留
- Accounting key 的 eytzinger 查找/累加优化 — 简化为遍历查找
- flush worker 线程 — 同步 flush 实现即可
- 并发分片 flush（多 shard）— 单线程实现即可
- `bch2_trans_update_buffered` 事务内 write buffer 注入路径（P1）
- 持久化 write buffer 状态跨挂载（P1）

## Open Questions

1. **实现深度**: 是完整实现 ~1500 行的 flush 管线（包括 slowpath 重排序），还是最小实现只覆盖 fastpath？
2. **数据结构对齐**: 必须完全对齐 darray 风格 flat 存储，还是 Vec 封装 + 正确排序接口即可？
3. **集成点**: 是否需要在 `update.rs` 或 `transaction.rs` 中添加 `write_buffer_insert` 调用？
