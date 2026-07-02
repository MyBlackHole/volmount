# Batch E — Btree IO 节点读写对齐：技术设计

## 1. 当前状态分析

### 1.1 volmount `io.rs` 现状

```
io.rs (319 行)
├── Read Path (stubs):
│   ├── bch2_btree_node_io_lock          → no-op (等待 write_in_flight)
│   ├── bch2_btree_node_io_unlock        → no-op (清除 write_in_flight)
│   ├── bch2_btree_node_wait_on_read     → no-op
│   ├── bch2_btree_node_wait_on_write    → no-op
│   ├── bch2_btree_node_read             → calls bucket_io::load_btree_node
│   ├── bch2_btree_root_read             → calls bucket_io::load_btree_node
│   ├── bch2_btree_node_read_done        → 仅检查 key_count>0 && data.is_empty()
│   ├── bch2_validate_bset              → no-op
│   ├── bch2_btree_node_drop_keys_outside_node → no-op
│   └── bch2_btree_flush_all_reads      → true
│
├── Write Path:
│   ├── bch2_btree_node_write            → serialize + write + journal_pin_add
│   ├── __bch2_btree_node_write          → 同上，多一个 cache 参数
│   ├── bch2_btree_node_write_trans      → 调用 write
│   ├── bch2_btree_post_write_cleanup    → compact + init_next ← 基本正确
│   ├── bch2_btree_init_next             → 分配下一个 bset slot      ← 基本正确
│   ├── btree_node_write_if_need         → 始终写入
│   └── flush/cancel all writes          → no-ops
│
└── Compat (stubs): compat_bformat / compat_bpos → no-ops
```

### 1.2 bcachefs `read.c` 关键路径

**`bch2_btree_node_read()` (read.c:1025-1095)**
```
bch2_btree_node_read(trans, b, sync)
├── bch2_bkey_pick_read_device()       ← 选择读取设备（多副本）
├── 分配 bio (bio_alloc_bioset)
├── 初始化 btree_read_bio (含 work 回调 → btree_node_read_work)
├── submit_bio_wait(bio)              ← sync 模式
└── btree_node_read_work(&rb->work)   ← 同步模式：直接调用回调
    └── bch2_btree_node_read_done()   ← 核心验证函数
        └── 成功后: clear_btree_node_read_in_flight + wake_up_bit
```

**`bch2_btree_node_read_done()` (read.c:574-913, ~340 行)**
```
read_done(c, ca, b, failed, err_msg)
│
├── [初始化] mempool_alloc fill_iter; sort_iter_init
├── [Magic] b->data->magic != bset_magic(c) → btree_err(bad_magic)
│
├── [遍历所有 bset] while (b->written < ptr_written)
│   ├── first? i = &b->data->keys : i = &write_block(b)->keys
│   │   └── 验证 bset seq 与节点 seq 一致（非 first 时 break 否则）
│   ├── csum_type = BSET_CSUM_TYPE(i)
│   ├── 校验和类型合法检查
│   ├── csum_vstruct 验证 btree_node / btree_node_entry 的 checksum
│   ├── bset_encrypt (解密，如果加密的话)
│   ├── first?: 验证 bp->seq == b->data->keys.seq; NEW_EXTENT_OVERWRITE flag
│   ├── b->version_ondisk = min(version)
│   ├── bch2_validate_bset(c, ca, b, i, offset, READ)  ← header 格式验证
│   ├── bch2_validate_bset_keys(c, ca, b, i, READ)      ← key 排序验证
│   ├── journal_seq_is_blacklisted?  → skip
│   ├── sort_iter_add(iter, i->start, vstruct_last(i))   ← 收集所有 key
│   └── max_journal_seq = max
│
├── [全局 key 排序] sort_iter_sort(iter)
├── [sorted buffer] bch2_bounce_alloc → bch2_key_sort_fix_overlapping
│   └── 处理 extent 重叠，生成全局排序的单 bset
├── [替换节点数据] b->data = sorted; b->nsets = 1
├── [每个 key 验证] btree_node_bkey_val_validate (语义)
├── [条件性丢弃范围外 key]
│   if (updated_range) → bch2_btree_node_drop_keys_outside_node(b)
├── [构建 aux tree] bch2_bset_build_aux_tree(b, set, false)
├── [节点重写标记] if (!ptr_written) → set_need_rewrite
└── [清理] clear_btree_node_read_in_flight + wake_up_bit
```

### 1.3 bcachefs `write.c` 关键路径

**`__bch2_btree_node_write()` (write.c:336-650)**
```
__bch2_btree_node_write(trans, b, flags)
│
├── [CAS 原子锁] while (!try_cmpxchg_acquire(&b->flags, &old, new))
│   ├── 检查 dirty / need_write / never_write / write_blocked
│   ├── 检查 write_in_flight 已设置 → return
│   └── 成功: 清除 dirty+need_write, 设置 write_in_flight+write_idx
│
├── [whiteout 排序] bch2_sort_whiteouts(c, b)
├── [sort_iter 初始化] sort_iter_stack_init(&sort_iter, b)
├── [[遍历 bsets]] for_each_bset(b, t)
│   └── bset_written()? → skip; 否则 sort_iter_add(未写的 key 范围)
├── [收集 whiteout] sort_iter_add(unwritten_whiteouts_start/end)
├── [排序输出] bch2_sort_keys_keep_unwritten_whiteouts(i->start, &sort_iter)
│   └── 产生单一排序 bset
├── [计算 checksum] csum_vstruct(c, BSET_CSUM_TYPE(i), nonce, bn/bne)
├── [提交 bio] bio_alloc → bch2_bio_map → queue on trans->queued_write_bios
│   └── bio 完成后: btree_node_write_endio → queue_work(write_work_queue)
│       └── btree_node_write_work:
│           ├── btree_node_write_update_key(trans, wbio, b)
│           └── __btree_node_write_done(trans, b)
│               ├── journal_pin_drop
│               ├── 清除 write_in_flight, wake_up
│               ├── 如果仍 dirty+need_write → 递归重新写入
│               └── bch2_btree_node_write_done_clean
│
└── b->written += sectors_to_write
```

**`bch2_btree_post_write_cleanup()` (write.c:693-730)**
```
post_write_cleanup(c, b)
├── 检查 btree_node_just_written → 清除该标志
├── if (nsets > 1)
│   └── bch2_btree_node_sort(c, b, 0, b->nsets)  ← 合并到单一 bset
│   else
│   └── bch2_drop_whiteouts(b, COMPACT_ALL)
├── 设置所有 bset 的 needs_whiteout
├── want_new_bset + bch2_bset_init_next(b, bne)     ← 准备下一个增量 bset
└── bch2_btree_build_aux_trees(b)                    ← 重建 aux 搜索树
```

## 2. 设计决策（已修正）

### 2.1 IO 锁设计：flag-based protocol

**修正**：bcachefs 的 `io_lock`/`io_unlock` 不是 Mutex，而是 `wait_on_bit_lock` on `BTREE_NODE_write_in_flight` flag。这是一个标志位协议，防止并发写入。

```
bcachefs io_lock:
  wait_on_bit_lock(b->flags, BTREE_NODE_write_in_flight)
  → 忙等直到 write_in_flight 清除，然后原子设置它

volmount 实现:
  当前 I/O 是同步的（写入在返回前完成），write_in_flight 不会真正持续。
  保持 sync 模型，添加 AtomicBool 标志位 + 简单的 spin 等待：
  - io_lock: spin 等待 flag false → CAS 设为 true
  - io_unlock: flag = false
```

**为什么不是 Mutex**：bcachefs 使用 flag 而非 Mutex 是为了与 wait/wake 位操作集成（`wake_up_bit`）。volmount sync 模型不需要等待，但接口必须正确定义语义。

### 2.2 写路径排序：sort_iter 模式

**修正**：bcachefs 写入路径不使用 `compact()` 或 `bch2_btree_node_sort()` 做预排序。它使用独立的 **sort_iter** 模式：

```
1. sort_iter_stack_init + 遍历 bsets 收集所有未写入的 key 范围
2. bch2_sort_keys_keep_unwritten_whiteouts(目标 buffer, &sort_iter)
   → 将所有分散的 key 排序输出到单个 bset
3. 设 i->u64s = 排序后的 key 总数
```

volmount 当前在 `bch2_btree_node_write` 中直接 `serialize_to_bucket()`，未做写完前 sort merge。需要在序列化前执行 sort_iter 模式。

### 2.3 读路径验证：sort_iter + 节点替换

**修正**：bcachefs `read_done` 不使用 `compact()`。它：
```
1. sort_iter 收集所有 bsets 的 keys
2. bch2_key_sort_fix_overlapping → 全局排序（含 extent 重叠处理）
3. 用 sorted buffer 替换整个 b->data
4. 构建 aux tree
```

volmount 的 `read_done` 当前几乎不做任何事情，需要完整实现。

### 2.4 `drop_keys_outside_node` 条件性

**修正**：不每次 read_done 都执行。仅在 `btree_ptr_v2.range_updated` 时调用：
```c
// read_done 中:
if (updated_range)
    bch2_btree_node_drop_keys_outside_node(b);
```

### 2.5 `post_write_cleanup` 时机

volmount 当前实现与 bcachefs 语义一致（nsets>1→sort, else→drop whiteouts, init_next, build_aux），只需微调。

## 3. 数据结构变更

### 3.1 `BtreeNode` 新增标志位

```rust
pub struct BtreeNode {
    // 已有字段...

    // 新增标志位（Batch E）:
    pub write_in_flight: AtomicBool,   // bcachefs BTREE_NODE_write_in_flight
    pub read_in_flight: AtomicBool,    // bcachefs BTREE_NODE_read_in_flight
    pub just_written: AtomicBool,      // bcachefs BTREE_NODE_just_written
}
```

注意：不用 Mutex。这些是 AtomicBool 标志位，仅在写入/读取期间短暂为 true。

### 3.2 `StorageError` 新增变体

```rust
pub enum StorageError {
    // 已有...
    CorruptData(String),       // 节点结构损坏
    ChecksumMismatch(String),   // 校验和不匹配
}
```

## 4. 函数映射（已修正）

| volmount 函数 | bcachefs 参考 | 当前 | 目标 |
|---|---|---|---|
| `io_lock` | `wait_on_bit_lock(write_in_flight)` | no-op | spin wait + CAS flag |
| `io_unlock` | `clear_bit(write_in_flight)` + wake | no-op | clear flag |
| `wait_on_read` | `wait_on_bit(read_in_flight)` | no-op | spin wait flag |
| `wait_on_write` | `wait_on_bit(write_in_flight)` | no-op | spin wait flag |
| `node_read` | `bch2_btree_node_read` | 调用 load_btree_node | 调用 read_done 验证 |
| `read_done` | `bch2_btree_node_read_done` | 空壳 | **完整重写** |
| `validate_bset` | `bch2_validate_bset` | no-op | **bset header 验证** |
| `validate_bset_keys` | `bch2_validate_bset_keys` | 不存在 | **新增：key 排序验证** |
| `drop_keys_outside` | `bch2_btree_node_drop_keys_outside` | no-op | **范围裁切 + aux rebuild** |
| `node_write` | `__bch2_btree_node_write` | 直接序列化 | sort_iter → 排序后写入 |
| `post_write_cleanup` | `bch2_btree_post_write_cleanup` | compact + init_next | 微调 |
| `header_to_text` | `bch2_btree_node_header_to_text` | 不存在 | **新增** |
