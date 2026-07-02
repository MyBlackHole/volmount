# bcachefs `__bch2_btree_node_write` 分析

来源: `bcachefs-tools/fs/btree/write.c` 第 336-700 行

## 写入流水线

```
__bch2_btree_node_write(trans, b, flags)
│
├─ [0] 检查是否需要写入:
│   └─ 检查 BTREE_NODE_dirty / need_write / write_in_flight 标志
│
├─ [1] bch2_btree_node_sort(c, b, 0, b->nsets)    ← 合并所有 bset 到 set[0]
│   ├─ 这是写入前必须做的: 将多个增量 bset 合并为单个紧凑 bset
│   └─ 合并后只保留 set[0]，nsets=1
│
├─ [2] 分配 write_bio:
│   ├─ wbio = container_of(bio, struct btree_write_bio, wbio.bio)
│   ├─ wbio->wbio.first_btree_write = !b->written
│   └─ 填充 bio: 设置 io 向量指向 b->data
│
├─ [3] 计算 sectors:
│   └─ sectors = btree_sectors_written(b)           ← 根据写入内容计算扇区数
│
├─ [4] 设置 write 标志:
│   └─ set_btree_node_write_in_flight(b)            ← 防止并发写入
│
├─ [5] bch2_submit_wbio_replicas(...)               ← 提交复制写入
│
└─ [6] bio 完成后:
    └─ btree_node_write_endio(bio)
        └─ queue_work(c->btree.write_work_queue, &wbio->work)
            └─ btree_node_write_work(work)
                ├─ btree_node_write_update_key(...)  ← 更新 sectors_written
                ├─ __btree_node_write_done(trans, b) ← 完成回调
                │   ├─ bch2_journal_pin_drop(...)    ← 释放 journal pin
                │   ├─ 清除 write_in_flight 标志
                │   ├─ 检查是否需要重新写入
                │   │   └─ 如果 dirty+need_write → 递归 __bch2_btree_node_write
                │   └─ bch2_btree_node_write_done_clean(...)
                └─ bio_put(bio)
```

## 关键设计

### `btree_write_bio` 结构
```c
struct btree_write_bio {
    struct work_struct work;
    struct bch_write_bio wbio;     // 继承的写 bio
    struct bkey_i_btree_ptr_v2 key; // 写入后更新的节点 key
    struct btree *b;
    // ...
};
```

### write_done 的 re-arm 机制
写入完成后若节点仍然 dirty 且 need_write，`__btree_node_write_done` 会自动重新触发写入。这是 bcachefs COW 的关键设计：节点写入缓存后若又有新数据写入（dirty flag 被重新设置），写入完成时会立即再次写盘。

### journal pin 生命周期
- `bch2_journal_pin_add(seq, flush_cb)` — 写入前 pin
- `bch2_journal_pin_drop(&journal, &w->journal)` — 写入完成后 drop
- 这样 journal 知道"这个节点已经写到磁盘了，可以回收该 seq 前的 journal"

## volmount 当前同步写模型的简化

volmount 当前是 write-through（每次写入立即刷盘），所以很多机制不需要：
- 无需 re-arm（同步写入后节点就是 clean 的）
- 无需 write-bio workqueue（IO 在调用栈内完成）
- 无需 write_in_flight 标志（不存在并发写入）

但批量和后续的 write-back 模型会需要这些机制。
