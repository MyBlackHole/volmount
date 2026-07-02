# Child-C: key cache 连接 (TC4+TC5)

## Goal

连接 key cache 的两条断裂路径：
- **TC4**: `trigger_key_cache_miss()` 在生产代码中从未被调用——当 btree 查找缓存未命中且需要 IO 时，应触发事务重启以便重试
- **TC5**: 写入路径对所有 key cache 条目调用 `invalidate()`（写穿），应改为对 `entry.cached == true` 的条目调用 `bch2_btree_insert_key_cached()`（写回）

## 背景

### TC4: trigger_key_cache_miss 未连接

`Btree::get_entry()`（`btree.rs:208`）当前流程：查缓存 → 命中返回 → 未命中走 btree → 插入缓存。但它不调用 `trigger_key_cache_miss()`。在 bcachefs 中，当 key cache miss 需要从磁盘读取 btree node 时，事务重启触发读入后再重试。当前 `get_entry` 没有 `BtreeTrans` 上下文，无法触发重启。

### TC5: bch2_btree_insert_key_cached 生产代码未使用

`bch2_btree_insert_key_cached()`（`key_cache.rs:239`）能创建 dirty cache 条目并注册 journal pin，但只被单元测试调用。

当前写入路径：
- `BtreeEngine::insert_entry()` → `Btree::insert()` → btree 写入 + `key_cache.invalidate()`
- `commit_with_engine()` Phase 2 使用 `engine.insert_entry()` （含 invalidate）

这意味着 key cache 只能通过 `get_entry()` 读路径获得干净条目，写路径永远是写穿（write-through）。

`entry.cached` 字段（`BtreeTransEntry` 结构体，Commit a56c033 已添加）指示该条目是否应写入 key cache。但 Phase 2 从未检查此字段。

## 当前代码状态分析

### trans_commit Phase 2（`transaction.rs:478-506`）
```rust
// Phase 2: 应用 journal 条目到 btree 节点
engine.insert_entry(entry.btree_id, entry.key.clone(), entry.value, self.journal_seq);
engine.delete_entry(entry.btree_id, &entry.key, self.journal_seq);
```

### BtreeEngine::insert_entry（`mod.rs:330-338`）→ Btree::insert（`btree.rs:322`）
→ 写入 btree 后 `key_cache.invalidate(&pos)` — 总是写穿

### BtreeEngine::insert_entry_raw（`mod.rs:341-343`）→ Btree::insert_entry（`btree.rs:277`）
→ `insert_entry_into_node()` + `key_cache.invalidate()` — 同样写穿

### 可用的不使缓存失效方法
- `Btree::insert_entry_skip_cache()`（`btree.rs:289`）— 写入 btree 但不 invalidate cache
- `KeyCache::bch2_btree_insert_key_cached()`（`key_cache.rs:239`）— 创建/更新脏缓存条目

## Requirements

### TC4: trigger_key_cache_miss 连接（P1 — 推荐）

1. `Btree::get_entry()` 当前没有 `BtreeTrans` 参数，无法直接触发重启。新增 `BtreeTrans` 感知的变体方法或在调用处处理
2. 在 Volume/engine 的查找路径中，当缓存未命中且已知需要 IO 时，调用 `trans.trigger_key_cache_miss()`
3. 事务重启后 cache 被正确填充

### TC5: bch2_btree_insert_key_cached 连接（P1 — 推荐）

1. `commit_with_engine()` Phase 2 中，对 `entry.cached == true` 的条目：
   - 使用 `insert_entry_skip_cache` 写入 btree（不 invalidate cache）
   - 调用 `bch2_btree_insert_key_cached` 将脏条目放入 key cache
2. 对 `entry.cached == false` 的条目：保持现有行为（`insert_entry` 含 invalidate）
3. `flush_cache_dirty_keys()` 在 trans_commit Phase 0 中已正常 flush 脏条目（无需修改）
4. journal pin callback（在 `pin_entry` 中注册）在 journal reclaim 到对应 seq 时触发 write-back

## Acceptance Criteria

### TC4 验收

- [ ] `trigger_key_cache_miss()` 在生产路径中被调用（不再仅测试使用）
- [ ] 事务重启后 cache 被正确填充
- [ ] 不改变现有 `Btree::get_entry()` 签名（新增变体而非修改现有方法）

### TC5 验收

- [ ] `commit_with_engine()` Phase 2 对 `entry.cached` 条目使用 `bch2_btree_insert_key_cached` 而非 `invalidate`
- [ ] `BtreeEngine` 新增 `insert_entry_cached()` 方法（或等效方式），在 btree 写入后不 invalidate 而是标记脏缓存
- [ ] 非 cached 条目保持现有 invalidate 行为不变
- [ ] `flush_cache_dirty_keys()` 在 trans_commit Phase 0 中正常 flush 脏条目
- [ ] journal pin callback 触发 write-back

### 质量验收

- [ ] `cargo build` 0 errors
- [ ] `cargo test -p volmount-core --lib` 763 passed, 5 pre-existing（0 新增失败）
- [ ] `cargo clippy --all-targets` 无新增 warning
- [ ] `cargo fmt --check` clean

### 集成验证

- [ ] cached 条目的写入 → key cache 为 dirty → flush_cache_dirty_keys 写回 btree → 数据一致
- [ ] 非 cached 条目的写入 → invalidate → 下次读取从 btree 重新加载
- [ ] journal pin callback 在 journal reclaim 时正确触发 flush_pending

## 排除范围

- 不修改 `flush_cache_dirty_keys()` 自身逻辑（已在 trans_commit Phase 0 中正确调用）
- 不修改 `bch2_btree_insert_key_cached()` 自身实现（仅改变调用时机）
- 不修改 `trigger_key_cache_miss()` 自身实现（仅改变调用位置）
- 不涉及 key cache 的 hash 表结构调整（LRU → hash 是另一任务）
- 不涉及 key cache writeback 的完整 writeback 机制（`batch_write`/`insert_guarded` 路径已在同步点任务中覆盖）

## 波及文件

| 文件 | 改动 |
|------|------|
| `btree/transaction.rs` | Phase 2 新增 cached 分支：`insert_entry_skip_cache` + `bch2_btree_insert_key_cached` |
| `btree/mod.rs` (BtreeEngine) | 新增 `insert_entry_cached()` 方法（若需要） |
| `btree/btree.rs` | 如需新增 trigger_key_cache_miss 调用点 |
| `btree/key_cache.rs` | 不变（`bch2_btree_insert_key_cached` 和 `pin_entry` 已完整） |
