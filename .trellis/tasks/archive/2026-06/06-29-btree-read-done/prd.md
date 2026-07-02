# btree key_cache journal_flush 实现

## Goal

实现 `bch2_btree_key_cache_journal_flush()`，使其不再是空 stub，而是当 journal reclaim 触发时实际将对应的脏 key cache 条目写回 btree，对齐 bcachefs 语义。

## 背景

当前 `KeyCache::bch2_btree_key_cache_journal_flush()` 是空实现（返回 0）。

脏条目的实际写回通过 `flush_dirty()` + `BtreeEngine::flush_cache_dirty_keys()` 的外部显式调用完成，而非 journal reclaim 驱动。

bcachefs 中，`bch2_btree_key_cache_journal_flush` 注册为每个脏 key cache 条目的 journal pin flush callback，在 journal reclaim 触发时直接执行写 btree 操作。

## 确认事实

- `bch2_btree_key_cache_journal_flush()` 在代码库中**未被调用** — 仅作为 bcachefs API 对齐占位
- `KeyCache::pin_entry()` 在每个脏条目上注册了 journal pin callback，但 callback 仅设置 `flush_pending = true`，不执行实际写回
- `KeyCache::flush_dirty()` 已实现三阶段 flush 模式（收集→写 btree→清除 dirty），通过 BtreeEngine 调用
- bcachefs 参考: `fs/btree/key_cache.c:520-579` — journal_flush callback 直接调用 `btree_key_cache_flush_pos()`
- volmount 的 journal reclaim (`journal_flush_pins()`) 会遍历所有 pin 并调用其 flush callback (reclaim.rs:1106-1108)

## Requirements

1. **非空实现**: `bch2_btree_key_cache_journal_flush()` 不再返回 0，而是根据 bcachefs 语义实际触发对应脏条目的写回
2. **与 journal reclaim 集成**: 当 journal_flush_pins 遍历到 key cache 条目的 pin 时，flush callback 应实际开始写回流程而非仅设标志
3. **兼容现有架构**: volmount 当前使用 `flush_dirty()` 三阶段模式（收集→写回→清理），新的 journal_flush 应与此集成而非完全替换
4. **并发安全**: journal reclaim 上下文中的 flush callback 不能持 journal 锁写 btree
5. **No regression**: 现有 key_cache 测试全部通过（`test_engine_flush_cache_dirty_keys` 等）

## Acceptance Criteria

- [ ] `bch2_btree_key_cache_journal_flush` 不再为空 stub，在 journal reclaim 时触发实际 flush
- [ ] journal_flush_pins 对 key cache pin 的 flush callback 触发后，脏条目最终被写回 btree
- [ ] `flush_pending` 标志在 flush 成功后正确清除
- [ ] `nr_dirty` 在 flush 成功后正确递减
- [ ] 所有现有 key_cache 单元测试通过
- [ ] `cargo clippy --all-targets` 通过
- [ ] `cargo test -p volmount-core --lib` 通过

## 非本次范围

- `btree/update.rs` 内部更新状态机激活（后续任务）
- `btree/transaction.rs::trans_get()` 修复（后续任务）
- `cache/mod.rs` 顶层模块清理（需先确认是否废弃）
- bcachefs 中 `bch2_btree_key_cache_flush_going_ro` 的完整实现

## Open Questions

- journal_flush callback 签名：当前 volmount 的 pin callback 是 `Box<dyn Fn()>` (无参数)，无法区分被 flush 的是哪个 entry。是否需要改为 `Box<dyn Fn(u64)>` 接收 seq 参数，从而与 bcachefs 的 `(j, pin, seq)` 对齐？

