# bcachefs `bch2_btree_node_read_done` 分析

来源: `bcachefs-tools/fs/btree/read.c` 第 574-913 行

## 验证流水线

```
bch2_btree_node_read_done(c, ca, b, failed, err_msg)
│
├─ [0] 初始化
│   ├─ iter = mempool_alloc(->c->btree.fill_iter)     ← sort_iter 用于收集所有 bset 的 key
│   ├─ sort_iter_init(iter, b)
│   └─ b->written = 0                                  ← 重置已写入指针
│
├─ [1] 动态故障注入检查
│   └─ bch2_meta_read_fault("btree") → btree_err(... btree_node_fault_injected)
│
├─ [2] Magic 验证
│   └─ b->data->magic != bset_magic(c) → btree_err(... btree_node_bad_magic)
│
├─ [3] 遍历所有 bset（while b->written < ptr_written）
│   │
│   ├─ [3a] 定位 bset:
│   │   ├─ first=true  → i = &b->data->keys          ← btree_node 自带的 bset
│   │   ├─ first=false → bne = write_block(b);       ← btree_node_entry 中的 bset
│   │   │                i = &bne->keys
│   │   │                if i->seq != b->data->keys.seq → break (超出节点边界)
│   │   │
│   │   ├─ [3b] 校验和类型验证:
│   │   │   └─ good_csum_type = bch2_checksum_type_valid(BSET_CSUM_TYPE(i))
│   │   │       └─ 不支持的类型 → btree_err(unknown_csum)
│   │   │
│   │   ├─ [3c] 如果 first:
│   │   │   ├─ sectors = vstruct_sectors(b->data)    ← btree_node 结构的扇区数
│   │   │   ├─ 检查 sectors 超出节点边界
│   │   │   ├─ 校验和验证: csum_vstruct → compare
│   │   │   ├─ bset_encrypt (解密)
│   │   │   ├─ 验证 btree_ptr_v2.seq == b->data->keys.seq
│   │   │   └─ 验证 NEW_EXTENT_OVERWRITE flag
│   │   │
│   │   ├─ [3d] 如果不是 first:
│   │   │   ├─ sectors = vstruct_sectors(bne)        ← btree_node_entry 的扇区数
│   │   │   ├─ 检查 sectors 超出节点边界
│   │   │   └─ 校验和验证 + 解密
│   │   │
│   │   ├─ [3e] b->version_ondisk = min(version)     ← 取最小的版本号
│   │   ├─ [3f] bch2_validate_bset(...)              ← bset header 格式验证
│   │   ├─ [3g] 如果是第一个 bset → btree_node_set_format(b, b->data->format)
│   │   ├─ [3h] bch2_validate_bset_keys(...)         ← bset 内 key 排序验证
│   │   ├─ [3i] SET_BSET_BIG_ENDIAN(i, CPU_BIG_ENDIAN)
│   │   ├─ [3j] blacklisted = bch2_journal_seq_is_blacklisted(i->journal_seq)
│   │   │   └─ 如果 blacklisted: 跳过此 bset
│   │   ├─ [3k] sort_iter_add(iter, i->start, i->end) ← 收集 key
│   │   └─ [3l] max_journal_seq = max(max_journal_seq, i->journal_seq)
│   │
│   └─ [3m] 后处理:
│       ├─ 检查 b->written < ptr_written → btree_node_data_missing
│       └─ 扫描未写入区域检查无残留 bset 签名
│
├─ [4] Key 验证:
│   ├─ sort_iter_sort(iter)                           ← 全局排序所有 key
│   ├─ btree_err_on(iter 中的 prev_key > next_key)    ← 检测降序
│   └─ btree_err_on(prev_key == next_key)             ← 检测重复 key
│
├─ [5] 范围验证:
│   ├─ 验证 min_key ≤ first key ≤ max_key
│   ├─ 验证 min_key ≤ last key ≤ max_key
│   └─ bch2_btree_node_drop_keys_outside_node(b)     ← 丢弃范围外 key
│
├─ [6] 排序合并:
│   ├─ sorted = sort_iter_free(iter)                  ← iter → sorted bset
│   ├─ memcpy(b->data, sorted, ...)                   ← 替换节点数据
│   └─ bch2_btree_build_aux_trees(b)                 ← 重建 aux 搜索树
│
├─ [7] 迭代器初始化:
│   └─ bch2_btree_node_iter_init_from_start(...)
│
├─ [8] 清理:
│   ├─ bch2_btree_set_heap(b, ...)                   ← 更新 cache 堆位置
│   ├─ clear_btree_node_read_in_flight(b)            ← 清除读取中标志
│   └─ bch2_time_stats_update(...)                   ← 更新延迟统计
│
└─ fsck_err:
    └─ 如果 ret != 0 → 返回错误码
```

## 核心验证函数

### `bch2_validate_bset()` (read.c ~420-550 行)
- 验证 bset header 字段的一致性
- 验证每个 key 的 format 与 header 声明一致
- 验证 bset u64s 与实际 layout 一致
- 返回错误码而非 bool

### `bch2_validate_bset_keys()` (read.c ~450-570 行)
- 遍历 bset 内所有 key
- 验证 `bkey_packed` 格式正确
- 调用 `bkey_invalid()` 检查 key 语义合法性
- 检查相邻 key 的 bpos 不降序
