# Phase 4: write_buffer P0 修复 — 执行计划

## 范围

单文件修改：`crates/volmount-core/src/btree/write_buffer.rs`
不修改其他文件。

## 执行清单

### Step 1: 数据结构对齐（P0-9）
- [ ] `BtreeWriteBufferedKey`: 将 `k: Vec<u8>` 替换为 `btree_id: BtreeId, key: BtreeKey, value: BchVal, key_type: KeyType`
- [ ] `BtreeWriteBufferKeys`: 新增 `lock: Mutex<()>`
- [ ] 添加 `WbKeyRef` 排序索引结构体（内部使用，不导出）
- [ ] 更新 `btree_write_buffer_new()` 初始化 lock

### Step 2: 排序实现
- [ ] 实现 `wb_key_cmp(a, b)` 比较 (btree_id, bpos)
- [ ] 实现 `build_sorted_index(flushing) -> Vec<WbKeyRef>`
- [ ] 实现 `wb_sort(&mut [WbKeyRef])` 按 (btree_id, inode, offset, snapshot) 排序

### Step 3: 核心 flush 管线（P0-6 + P0-10）
- [ ] 实现 `move_keys_from_inc_to_flushing()` 
- [ ] 实现 `dedup_sorted_refs()` — 相同 pos 条目合并/丢弃
- [ ] 实现 `flush_fastpath()` — 遍历 sorted refs → engine.insert_entry
- [ ] 实现 `flush_slowpath()` — 失败 key 通过事务重试
- [ ] 实现 `bch2_btree_write_buffer_flush_locked(wb, engine, journal)` — 组装完整管线
- [ ] 实现 `bch2_btree_write_buffer_flush(wb, engine, journal)` — 获取 lock 后调用 flush_locked

### Step 4: insert 入口（P0-5）
- [ ] 实现 `bch2_journal_key_to_wb(wb, btree_id, key, val, journal_seq)` — 锁 inc → 追加 key → 解锁

### Step 5: need_flush 判断（P0-8）
- [ ] 实现 `bch2_journal_write_buffer_need_flush(wbs: &[BtreeWriteBuffer]) -> bool` — 检查所有 wb 的总 pending key 数

### Step 6: 更新公开 API 桩函数（P0-7 确认）
- [ ] 确认 `bch2_btree_write_buffer_must_wait()` 实现正确（容量检查）
- [ ] `bch2_btree_write_buffer_flush_sync()` 调用 `bch2_btree_write_buffer_flush()`
- [ ] `bch2_btree_write_buffer_tryflush()` 调用 `bch2_btree_write_buffer_flush()`
- [ ] `bch2_btree_write_buffer_maybe_flush()` 调用 `bch2_btree_write_buffer_flush()`

### Step 7: 测试
- [ ] 更新现有测试适配新数据结构
- [ ] 新增 `test_write_buffer_insert_and_flush()`
- [ ] 新增 `test_write_buffer_dedup()`
- [ ] 新增 `test_write_buffer_noop_elimination()`
- [ ] 新增 `test_write_buffer_sort_order()`

## 验证命令

```bash
# 编译（每次 step 后运行）
cargo build -p volmount-core 2>&1

# 测试
cargo test -p volmount-core 2>&1 | tail -5

# Clippy
cargo clippy -p volmount-core 2>&1 | grep "warning:" | wc -l

# 确认无新增 warning（基线 49）
```

## 回滚点

- Step 1 完成后（数据结构变更可能影响测试编译）
- Step 3 完成后（flush 管线核心逻辑）
- Step 7 测试通过后（最终验证）

## 风险

1. **`BtreeWriteBufferedKey` 字段变更** 会破坏现有测试（`test_write_buffer_must_wait` 和 `test_write_buffer_create` 创建实例）。需同步更新测试。
2. **Flush 需要 `BtreeEngine` + engine 的能力**（get_entry/insert_entry）。当前 API 签名需调整或传引用。
3. **slowpath flush 需要 journal**（`flush_via_transaction` 需要 Journal 引用）。需要确定是否传 Option<&Journal>。

## 执行策略

使用子代理执行：将 Step 1-3 合并为一个 deep 子代理（强相关），Step 4-6 为第二个子代理，Step 7 为第三个子代理。

```bash
# 但第一步使用主会话直接修改数据结构，然后委托 flush 管线实现
```
