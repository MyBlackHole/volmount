# Implement: bcachefs 核心模块完全对齐

## 批次划分

基于模块依赖关系（见 design.md），将 8 个模块分为 4 个批次执行：

### Batch 1: lock + volume（可完全并行）
- **lock**: ~10 个新增/修改函数
- **volume**: ~5 个 API 对齐

### Batch 2: btree（核心大模块，独立进行）
- 非常大，16 个子系统，需要子步骤

### Batch 3: alloc + journal（可并行，依赖 btree）
- **alloc**: 后台线程、discard、freelist 等
- **journal**: journal_reclaim、reservation 等

### Batch 4: snap + subvol + recovery（可部分并行）
- **snap**: skip_list、祖先缓存、清理
- **subvol**: 子卷快照流程完善
- **recovery**: pass 语义对齐、错误处理

## 各模块实施步骤

### Batch 1a: lock 对齐

1. 添加通用 `try_lock_ip` 方法（带 ip 参数）
2. 添加 `six_relock_ip`（带 seq 检查的重新加锁）
3. 添加 `six_trylock_convert`（read↔intent 通用转换）
4. 添加 `six_lock_increment`（重入计数）
5. 添加 `six_lock_wakeup_all`（调试用）
6. 添加 `six_lock_counts`（统计）
7. 添加 `six_lock_readers_add`（死锁恢复）
8. 对齐 `six_lock_ip_waiter` 接口（含 should_sleep_fn + waiter 参数）
9. 添加 `six_lock_contended`（跳过初始 trylock 的慢路径）
10. 宏生成类型化函数对齐命名
11. 更新 `lock/mod.rs` 导出

**验证**: `cargo test -p volmount-core --lib -- six` / `lock`

### Batch 1b: volume 对齐

1. 对齐 VolumeState 枚举值与 `BCH_FS_*` 标志位命名
2. 添加/对齐生命周期管理函数
3. 对齐 `Volume::start/stop/set_running/set_stopped/set_error` 的 CAS 语义
4. 检查 `write_extent` / `delete_extent` 流程

**验证**: `cargo test -p volmount-core --lib -- volume`

### Batch 2: btree 对齐（大模块，16 个子系统，分 5 个阶段）

#### 阶段 2a: bkey 核心对齐
1. 对齐 `Bpos` 字段名: `inode`/`offset`/`snapshot` (目前是 `vol_id`/`offset`/`snapshot`)
2. 添加 bpos 比较函数: `bpos_eq/lt/le/gt/ge/cmp` 对齐命名
3. 添加 `bpos_successor/predecessor`
4. 添加 `struct bkey_i` / `struct bkey_s_c` / `struct bkey_s` 等价类型
5. 添加 `bkey_unpack` / `bkey_pack`（替代 bincode 序列化）

#### 阶段 2b: bset + 节点操作对齐
1. 对齐 `BtreeNode` 内部布局与 bcachefs `struct btree`
2. 对齐 `bset_init_first/next`、`bset_insert/delete`
3. 对齐 `btree_node_iter` 操作
4. 添加 `bch2_btree_keys_init`

#### 阶段 2c: btree_iter + btree_trans 对齐
1. 对齐 `BtreeIter` 与 bcachefs `struct btree_iter`
2. 添加 `bch2_path_get/put`
3. 对齐 `bch2_btree_iter_peek/peek_slot/next/prev`
4. 添加事务重启机制 (`lockrestart_do` 等价)
5. 对齐 `btree_trans_commit` 流程
6. 添加 `for_each_btree_key` 宏等价

#### 阶段 2d: 内部节点操作对齐
1. 添加 `btree_split` / `btree_merge`
2. 添加 `btree_increase_depth`
3. 添加 `btree_node_rewrite`
4. 添加 `btree_set_root_for_read`

#### 阶段 2e: cache + I/O + write buffer + GC
1. 对齐 btree node cache 管理
2. 添加完整读路径 (`btree_node_read/root_read`)
3. 添加完整写路径 (`btree_node_write`)
4. 添加 journal overlay 完整迭代器
5. 添加 sort/compact 操作
6. 添加 key_cache 完整接口

**验证**: 每阶段后运行 `cargo test -p volmount-core --lib -- btree`

### Batch 3a: alloc 对齐

1. 对齐 alloc foreground 路径 (allocate_blocks/multi-bucket)
2. 添加 alloc background 线程 (bucket 优先级/GC)
3. 添加 discard 支持
4. 对齐 Freespace btree 操作
5. 对齐 OpenBucket 引用计数
6. 添加 bucket 迁移/老化

**验证**: `cargo test -p volmount-core --lib -- alloc`

### Batch 3b: journal 对齐

1. 对齐 journal_reservation 接口
2. 添加 journal_reclaim (bucket 回收)
3. 添加 journal_seq_blacklist
4. 对齐 journal_entry_pin 回调
5. 添加 discard 支持

**验证**: `cargo test -p volmount-core --lib -- journal`

### Batch 4a: snap 对齐

1. 修复 skip_list 实现 (test_skip_list_ordered 已知失败)
2. 添加祖先缓存优化
3. 添加死快照批量清理
4. 添加快照子树删除

**验证**: `cargo test -p volmount-core --lib -- snap`

### Batch 4b: subvol 对齐

1. 修复子卷创建流程 (test_create_multiple 已知失败)
2. 修复子卷快照创建 (test_create_snapshot_subvolume 已知失败)
3. 添加子卷统计/遍历

**验证**: `cargo test -p volmount-core --lib -- subvol`

### Batch 4c: recovery 对齐

1. 对齐 pass flags 语义 (`PASS_ALWAYS/UNCLEAN/SILENT`)
2. 对齐 `pass_done` 持久化到 superblock
3. 对齐 `rewound_to` / `bch_err_throw` 机制
4. 对齐 errors_to_text / 错误信息

**验证**: `cargo test -p volmount-core --lib -- recovery`

## 验证命令

```bash
# 每个模块完成后
cargo test -p volmount-core --lib
cargo clippy --all-targets
cargo fmt --check

# 全量验证
cargo build
cargo test -p volmount-core --lib
cargo clippy --all-targets
```

## 风险点与回滚

- **btree bkey 序列化格式更改**: `bkey_pack/unpack` 替代 bincode 可能影响磁盘兼容性 → 需要保持读写路径同步更新
- **Bpos 字段重命名**: `vol_id → inode` 影响所有模块 → 需全仓替换
- **3 个已知失败测试**: 在修复前不阻止 CI，修复后需确认不再回归

## 检查清单

- [ ] Batch 1a (lock): 3 个 - cargo test pass + clippy clean
- [ ] Batch 1b (volume): 3 个 - cargo test pass + clippy clean  
- [ ] Batch 2 (btree): 每阶段 2 个 - cargo test pass + clippy clean
- [ ] Batch 3a (alloc): 3 个 - cargo test pass + clippy clean
- [ ] Batch 3b (journal): 3 个 - cargo test pass + clippy clean
- [ ] Batch 4a (snap): 3 个 - cargo test pass + clippy clean
- [ ] Batch 4b (subvol): 3 个 - cargo test pass + clippy clean
- [ ] Batch 4c (recovery): 3 个 - cargo test pass + clippy clean
