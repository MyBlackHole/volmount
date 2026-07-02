# Phase 4: write_buffer P0 修复 — 技术设计

## 架构边界

Write buffer 位于 journal 和 btree 之间：
```
Journal → [bch2_journal_key_to_wb] → WriteBuffer(inc) → flush → WriteBuffer(flushing) → Btree(engine)
```

所有启用 write buffer 的 btree（Accounting/LRU/Backpointers/DeletedInodes 等 11 种）的更新不直接写入 btree 节点，而是先进入 write buffer 的 inc 队列，在 flush 时批量排序合并后写入 btree。

### 不改变的边界
- `BtreeEngine` 的 `insert_entry`/`delete_entry` API 不变（flush 时调用）
- Journal 的 append/pin 机制不变
- Transaction commit pipeline (`trans_commit`) 不变

### 需要修改的文件
- `crates/volmount-core/src/btree/write_buffer.rs` — 核心实现
- `crates/volmount-core/src/btree/update.rs` — 可能添加 `write_buffer_insert` 调用点（P1 暂保留）

## 数据结构设计

### 1. `BtreeWriteBufferedKey` — 对齐 bcachefs `struct btree_write_buffered_key`

```rust
/// 写缓冲区中的 key 条目
/// 对应 bcachefs: struct btree_write_buffered_key { u64 journal_seq; __BKEY_PADDED(k, ...); }
#[derive(Debug, Clone)]
pub struct BtreeWriteBufferedKey {
    pub journal_seq: u64,
    pub btree_id: BtreeId,
    pub key: BtreeKey,
    pub value: BchVal,
    pub key_type: KeyType,
}
```

**vs bcachefs**: bcachefs 使用变长 `bkey_i` + inline value（darray_u64 flat 存储）。
volmount 使用固定大小 `(BtreeKey, BchVal)` 以简化实现。
P0-9 满足（功能对齐，内存布局差异为 P2）。

### 2. `BtreeWriteBufferKeys` — 对齐 bcachefs `struct btree_write_buffer_keys`

```rust
pub struct BtreeWriteBufferKeys {
    pub keys: Vec<BtreeWriteBufferedKey>,
    pub lock: Mutex<()>,
    pub nr: usize,
}
```

新增 `lock` 字段对应 bcachefs 的 `struct mutex lock`（用于 inc 并发保护）。

### 3. `WbKeyRef` — 排序索引（新增）

```rust
/// 排序用轻量级引用 — 对应 bcachefs struct wb_key_ref
struct WbKeyRef {
    idx: u32,       // keys 中的索引
    btree_id: u8,   // btree 类型
    inode: u64,     // bpos.inode — 排序键
    offset: u64,    // bpos.offset — 排序键
    snapshot: u32,  // bpos.snapshot — 排序键
}
```

## 核心管线设计

### P0-5: `bch2_journal_key_to_wb()`

```
输入: (btree_id, key, value, journal_seq)
逻辑:
  1. 验证 btree_id 启用了 write buffer（通过 BtreeId 标志位检查）
  2. 锁定 wb.inc.lock
  3. 追加 BtreeWriteBufferedKey 到 wb.inc.keys
  4. wb.inc.nr++
  5. 解锁
  返回: 0 (成功)
```

### P0-6/P0-10: `bch2_btree_write_buffer_flush_locked()`

```
输入: (wb, engine)
流程:
  1. move_keys_from_inc_to_flushing():
     - 锁定 wb.inc.lock
     - 将 wb.inc 的所有 key 移到 wb.flushing.wb_keys
     - 重置 wb.inc
     - 解锁
  
  2. if wb.flushing.nr == 0: return Ok(0)
  
  3. build_sorted_index():
     - 为 flushing 中每个 key 创建 WbKeyRef
     - WbKeyRef.idx = key 在 flushing.keys 中的索引
     - WbKeyRef.btree_id / pos = key 的 btree_id / bpos
  
  4. wb_sort():
     - 按 (btree_id, inode, offset, snapshot) 排序 WbKeyRef 数组
     - 使用 Rust sort_unstable_by
  
  5. dedup():
     - 遍历 sorted_refs，相邻相同 pos 的条目合并
     - 保留 journal_seq 较大的条目（较新的写入）
     - 丢弃重复条目
     - 被丢弃条目的 journal_seq 置 0
  
  6. fastpath flush (单线程):
     - 初始化 current_btree_id = None / current_iter = None
     - 遍历 sorted_refs:
       a. 如果 btree_id 变化 → 在 engine 中建立新遍历
       b. engine.get_entry() 检查当前 key
       c. 如果值相同 → noop（跳过）
       d. engine.insert_entry() 插入到 engine（直接写 btree）
       e. 成功后标记 journal_seq = 0（已 flush）
  
  7. slowpath flush:
     - 遍历 flushing.keys，找到 journal_seq != 0 的条目
     - 对每个失败 key：
       a. 创建事务
       b. engine.insert_entry() 通过事务写入
       c. 事务提交
     - 如果全部完成 → 清空 flushing（nr = 0）
  
  返回: Ok(())
```

### P0-7: `bch2_btree_write_buffer_must_wait()`

**已实现**（检查 inc.nr + flushing.nr 超过 inc.capacity * 3/4）。
验证当前实现正确。

### P0-8: `journal_write_buffer_need_flush()`

```
逻辑:
  - 对所有 BCH_WB_BTREE_NR 个 write buffer 求和 inc.nr + flushing.nr
  - 如果总数 > 0 → 需要 flush（有等待刷入的 key）
  - 返回 bool
```

## flush 慢路径策略

当 fastpath 的 `engine.insert_entry()` 因为节点满/分裂等原因失败时，fallback 到事务路径：

```rust
// slowpath: 通过事务提交
fn flush_via_transaction(
    engine: &mut BtreeEngine,
    journal: &Journal,
    key: &BtreeWriteBufferedKey,
) -> Result<(), StorageError> {
    let mut trans = BtreeTrans::default();
    trans.begin();
    trans.journal_insert(key.btree_id, key.key, key.value);
    trans.trans_commit(journal, engine)?;
    Ok(())
}
```

## 与 Journal 的交互

当前实现不涉及 journal pin 集成（P1）。Flush 操作直接调用 `engine.insert_entry()`，不需要 journal 预留/提交（因为写入 btree 是就地更新）。

但 `flush_via_transaction` slowpath 需要 journal 引用以创建 journal 条目。

**设计决定**: `flush_locked` 接受 `Option<&Journal>` 参数，slowpath 时使用。

## 测试策略

### 单元测试
1. `test_write_buffer_insert_and_flush`: 插入 key → flush → 验证 key 出现在 engine 中
2. `test_write_buffer_dedup`: 插入 2 个相同 pos 的 key → flush → 验证 engine 中只有最新值
3. `test_write_buffer_noop_elimination`: 插入与 engine 现有值相同的 key → flush → 验证无实际写入
4. `test_write_buffer_sort_order`: 无序插入 → flush → 验证 btree 中 key 顺序正确
5. `test_write_buffer_should_flush`: 验证容量阈值

### 集成验证
- 无需修改现有 627 测试
- `cargo test -p volmount-core` 全部通过

## 已知限制（P2）
1. 无分片并行 flush（bcachefs 多 shard 优化）
2. 无 accounting key eytzinger 查找/累加
3. 无 journal pin 自动 flush 回调
4. 无 `bch2_trans_update_buffered` 事务内注入 API
5. BtreeWriteBufferedKey 固定大小（非变长）
