# volmount-core btree API ↔ bcachefs 内核 API 差距分析报告

> **审计目标**: 逐项对比 volmount-core 的 Rust B-tree API 与 bcachefs 内核 C API 的差异
> **审计范围**: `volmount-core/src/btree/` 所有公共类型和函数，对齐 bcachefs `fs/bcachefs_format.h` + `fs/btree/*.h`
> **参考版本**: bcachefs-tools `v1.38.6-36-g499dbe7e0` (2026-06)，bcachefs 内核源码树
> **日期**: 2026-06-24
> **严重程度**: P1（正确性/性能关键）> P2（可维护性）> P3（缺失功能）

---

## 1. 摘要

| 维度 | 对齐（Aligned） | 微小偏差（Minor） | 重大差异（Major） | 缺失功能（Missing） |
|------|:-:|:-:|:-:|:-:|
| **类型定义** | 14 | 5 | 3 | 5 |
| **函数签名** | 18 | 8 | 4 | 15 |
| **合计** | 32 | 13 | 7 | 20 |

### 1.1 P1（必须修复）

| # | 项目 | 文件 | 影响 |
|---|------|------|------|
| P1-1 | `Bpos` 缺少 `bversion` 和 `size` 字段 | `key.rs` | 磁盘格式不兼容，extent 语义丢失 |
| P1-2 | `Bpos` 的 name `vol_id` 与 C `inode` 不一致 | `key.rs` | 导致 `from_key()` 丢失 inode 信息 |
| P1-3 | `BtreeIter` 缺少 per-btree 快照过滤 | `iter.rs` | 快照错误可见性 |
| P1-4 | `BtreeTrans` 缺少 `btree_insert_entry` 累积路径 | `transaction.rs` | 触发器和提交路径不正确 |
| P1-5 | `BtreeNode` 没有 `write_blocked` 链表 | `node.rs` | 异步分裂无法同步 |

### 1.2 P2（建议修复）

| # | 项目 | 文件 | 影响 |
|---|------|------|------|
| P2-1 | `BtreeKey` vs `bkey` 缺少 `bversion` | `key.rs` | 版本跟踪缺失 |
| P2-2 | `BtreeIter` 缺少 `path` 引用计数 | `iter.rs` | 多 iter 共享路径时错误 |
| P2-3 | `IterFlags` 过于简单 | `iter.rs` | 缺少 prefetch/cached/all_snapshots 等 |
| P2-4 | `BtreeCache` 缺少 shrinker 集成 | `cache.rs` | 内存压力下无法回收 |
| P2-5 | `SplitRoot` 的 `BtreeInteriorUpdate` 太简单 | `update.rs` | 缺少完整 btree_update 生命周期 |

### 1.3 P3（功能缺失）

| # | 项目 | 文件 | 影响 |
|---|------|------|------|
| P3-1 | 没有 key cache（`bkey_cached`） | — | 缓存缺失 |
| P3-2 | 没有节点重写（node rewrite） | — | 写放大 |
| P3-3 | 没有 overflow btree | — | 大 extent 不支持 |
| P3-4 | 没有 SRCU 保护 | — | RCU 安全 |
| P3-5 | 没有 `btree_node_iter` 多 bset 遍历 | `iter.rs` | 合并/压缩 |

---

## 2. 类型定义对比

### 2.1 磁盘格式类型

| Rust 类型 | C 类型 | 状态 | 差距描述 |
|-----------|--------|:----:|----------|
| `Bpos` | `struct bpos` | **⚠ 偏差** | 1. Rust 使用 `vol_id(u64)+offset(u64)+snapshot(u32)`，对齐 C `inode(u64)+offset(u64)+snapshot(u32)`，字段名不同（`vol_id` vs `inode`）。2. C 的 `bpos` 使用 `__packed __aligned(4)` 且 LE下字段顺序为 `snapshot,offset,inode` 以便 memcmp 视作大整数。Rust 仅用自然序（`vol_id,offset,snapshot`），**排序语义相同但序列化格式不兼容**（P1-1）。 |
| `BkeyPacked` | `struct bkey_packed` | **✓ 对齐** | `u64s(u8)+format_whiteout(u8)+type_(u8)+key_start([])` 与 C 的 3 字节 header + `pad[sizeof(bkey)-3]` 完全兼容。 |
| `BkeyFormat` | `struct bkey_format` | **✓ 对齐** | `key_u64s(u8)+nr_fields(u8)+bits_per_field[6]+field_offset[6]` 完全一致。 |
| — | `struct bkey` | **⚠ 偏差** | Rust 没有直接的 `bkey` 等价类型。`BtreeKey`（vaddr+snapshot_id+key_type）是简化替代，**缺少 `bversion(hi,lo)`(16B)、`size(u32)`、`pad[1]`**（P1-1/P2-1）。C `bkey` = `u64s(1)+format/needs_whiteout(1)+type(1)+pad(1)+bversion(12)+size(4)+bpos(20)` = 40 字节。Rust `BtreeKey` = `vaddr(8)+snapshot_id(4)+key_type(1)` = 13 字节。 |
| — | `struct bkey_i` | **⚠ 偏差** | Rust 无此类型。`BtreeEntry(pos+key_type+value)` 是逻辑替代但**缺少 `bkey` 的位域字段和 `bversion`**。 |
| — | `struct bversion` | **❌ 缺失** | C 有 `bversion{lo(u64),hi(u32)}` 表示版本号。Rust 完全缺失此类型（P3-1）。 |
| `BtreeNodeHeader` | `struct btree_node` (disk) | **⚠ 偏差** | 1. C 的 `btree_node` 包含完整的 bset 头信息（`crc, btree_id, level, ...`），Rust `BtreeNodeHeader` 结构不同——多了 `min_key/max_key(Bpos)`、少了 C 的 `format, unpack` 等字段。2. 序列化格式不兼容。 |
| — | `struct btree_node_entry` | **❌ 缺失** | Rust 的 `BtreeNodeDiskEntry` 概念对齐但字段不同。C 中 `btree_node_entry` 用于 log-structured append 写入第二个及之后的 bset。 |
| `KeyType` | `bch_bkey_type` (bkey_types.h) | **✓ 对齐** | 对齐 `KEY_TYPE_deleted=0, KEY_TYPE_...` 等。Rust 定义了 `Normal=0, Deleted=1, Whiteout=2`。 |
| `BtreeOp` | (内联 enum in bcachefs) | **✓ 对齐** | `Insert/Delete/Whiteout` 对应 bcachefs 的 `BTREE_UPDATE_*`。 |

### 2.2 内存类型

| Rust 类型 | C 类型 | 状态 | 差距描述 |
|-----------|--------|:----:|----------|
| `BtreeNode` | `struct btree` | **⚠ 偏差** | 1. Rust 的 `data: Vec<u8>`、`sets: [BsetTree; 3]` 概念对齐，但缺少 `nsets, bset_tree` 等多个辅助字段。2. **缺少 `write_blocked` 链表**（P1-5）——异步分裂/合并无法阻塞父节点写入。3. **缺少 `ob`（open_buckets）和 `list`（LRU 链表）**——缓存管理不兼容。4. C 的 `btree_bkey_cached_common c` 提供 `six_lock+level+btree_id+cached`——Rust 将 lock 直接嵌入 `BtreeNode`。 |
| `BtreeNodeLockedType` | (six_lock_type 枚举) | **✓ 对齐** | `None=0/Read=1/Intent=2/Write=3` 与 C 一致。 |
| `BtreePathLevel` | `struct btree_path_level` | **✓ 对齐** | 包含 `node(b)/lock_state/lock_seq`。Rust 多了 `offset/child_idx`（跟踪位置）。 |
| `BtreeRoot` | `struct btree_root` | **⚠ 偏差** | Rust 简化为 `node(Arc<BtreeNode>)+depth(u8)`。C 的 `btree_root` 包含 `b(key, level, alive, error)` 和 `__BKEY_PADDED(key, ...)` 用于磁盘根指针。 |
| `BtreeIter` | `struct btree_iter` | **⚠ 偏差** | 1. Rust 的 `path: Vec<BtreePathLevel>` 是简化版——C 使用 `btree_path` 引用（`path: btree_path_idx_t`）并通过 `update_path/key_cache_path` 支持路径复用。2. **缺少 `btree_id、snapshot、k、flags`**——需要 per-btree 过滤（P1-3）和路径复用。3. C 有 `min_depth/journal_idx`。 |
| — | `struct btree_path` | **❌ 缺失** | C 有完整的 `btree_path`（`pos, btree_id, ref, intent_ref, level, locks_want, nodes_locked, l[BTREE_MAX_DEPTH]`）。Rust 将路径信息分散在 `BtreeIter` 和 `BtreePathLevel` 中，**没有引用计数**——多 iter 复用路径时可能出错（P2-2）。 |
| `BtreeTrans` | `struct btree_trans` | **⚠ 偏差** | 1. Rust 的 `iters: Vec<BtreeIter>` 对应 C 的 `paths[]` 数组。2. Rust 有 `journal` 向量追踪修改——C 使用 `struct btree_insert_entry *updates` + `nr_updates`。3. **缺少 `updates` 数组**——bcachefs 的事务提交路径将所有 pending 更新收集到 `updates` 数组，通过排序+锁顺序+触发器+写回。Rust 的 `journal` 更简单，缺少 `old_k, old_v, path ref` 信息（P1-4）。4. Rust 有 `needs_restart/restart_count`——C 有 `restarted/restart_count`，已经对齐。 |
| `BtreePtrV2` | `struct bch_btree_ptr_v2` | **✓ 对齐** | 概念对齐——包含 `block_addr/level/key_count/node_size`。 |
| `BtreeCache` | `struct bch_fs_btree_cache` | **⚠ 偏差** | 1. Rust 用了 `Mutex<HashMap>`——C 使用 `rhashtable`（无锁读）+ `clean/dirty` 链表。2. Rust `MAX_CLEAN=1024, MAX_DIRTY=256` 是固定值——C 支持 shrinker + 动态调整。3. **没有 shrinker 集成**（P2-4）。 |
| `NodeCache` | (没有直接对应) | **⚠ 偏差** | Rust 在 `BtreeCache` 上包装了 `NodeCache`，添加了 `next_block: AtomicU32` 用于模拟地址分配。C 中地址分配在 bucket allocator 中。 |
| — | `struct btree_insert_entry` | **❌ 缺失** | C 的数据结构，包含 `flags, sort_order, bkey_type, btree_id, level, cached, path, old_k, old_v, k` 等。Rust 通过 `BtreeTrans::journal: Vec<(BtreeId, BtreeKey, BchVal, BtreeOp)>` 简化实现——丢失了触发器状态（`insert_trigger_run/overwrite_trigger_run`）和旧值引用（P1-4）。 |
| — | `struct btree_update` | **❌ 缺失** | C 中 btree 分裂/合并的完整状态机，包含 `closure, disks_res, btree_id, mode, node_start/end, nodes_written, b, write_blocked_list` 等。Rust 的 `BtreeInteriorUpdate` 是简化版本——缺少磁盘预留、异步写完成回调等（P2-5）。 |
| — | `struct btree_node_iter` | **❌ 缺失** | C 中用于多 bset 遍历的结构。Rust 将遍历逻辑嵌入 `BtreeNode` 的 `read_entry/scan_entry` 等方法中。缺少 `btree_node_iter_set` 数组的跨 bset 合并遍历（P3-5）。 |

---

## 3. 函数签名对比

### 3.1 btree 初始化/生命周期

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `Btree::new()` | `bch2_fs_btree_init()` + `bch2_btree_cache_init()` | **✓ 对齐** | 概念一致，简化。 |
| `Btree::load_root()` | `bch2_btree_root_read()` | **✓ 对齐** | 从后端加载根节点。 |
| `Btree::from_root()` | 无直接对应 | **⚠ 偏差** | Rust 特有（test helper）。 |
| `BtreeEngine::new()` | `bch2_fs_btree_init_early()` | **✓ 对齐** | 初始化所有 btree 类型实例。 |

### 3.2 btree 读操作

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `Btree::get(&self, target: &BtreeKey) -> Option<(BtreeKey, BchVal)>` | `bch2_btree_iter_peek()` | **⚠ 偏差** | 1. Rust 封装了 BtreeIter + pos 匹配逻辑；C 通过 `bch2_btree_iter_peek()` 返回 `struct bkey_s_c`（key+value 分离）。2. Rust 返回 `Option<(BtreeKey, BchVal)>`，C 返回 `bkey_s_c`——C 版本提供 key/value 指针（不拷贝），Rust 拷贝整个 key/value（P2-2）。 |
| `Btree::get_entry(&self, pos: Bpos) -> Option<BtreeEntry>` | `bch2_btree_iter_peek()` + 走查 | **✓ 对齐** | 逻辑一致，但缺少 `bversion`。 |
| `Btree::search(&self, target: &BtreeKey) -> Option<(BtreeKey, BchVal)>` | `bch2_btree_iter_peek()` + `bch2_btree_iter_peek_with_holes()` | **⚠ 偏差** | Rust 使用 `find_leaf_node` + `node.search` 两阶段——C 使用 `btree_path_traverse_one` + `path_peek_slot`。 |
| `Btree::for_each()` | `bch2_btree_iter_peek()` 循环 | **✓ 对齐** | 概念一致。 |
| `Btree::for_each_entry()` | 同上 | **✓ 对齐** | 概念一致，支持 Raw value 更好。 |

### 3.3 btree 写操作

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `Btree::insert(&mut self, key, value, journal_seq) -> bool` | `bch2_btree_insert()` + `bch2_trans_update()` + `bch2_trans_commit()` | **⚠ 偏差** | 1. Rust 直接操作 `Arc::get_mut` 获取节点独家引用——C 通过事务 + 锁 + path 路径获取节点。2. C 的插入经过 `btree_node_iter_fix` 修正内部迭代器位置。3. C 的插入通过 `btree_node_bset_insert_key` 完成——Rust 通过 `node.insert()`。Rust 简化了写路径，但缺少 `needs_whiteout` 位和 `bversion` 更新（P1-1/P2-1）。 |
| `Btree::insert_entry()` | 同上 | **✓ 对齐** | 同上但支持 `KeyValue::Raw`。 |
| `Btree::delete(&mut self, key, journal_seq) -> bool` | `bch2_btree_delete_at()` + `bch2_trans_update()` | **⚠ 偏差** | Rust 通过 `node.delete_key()`——C 插入一个 `KEY_TYPE_deleted` 墓碑记录。 |
| `Btree::insert_with_transaction()` | `bch2_trans_update()` | **⚠ 偏差** | 尝试集成了事务重启通知，但缺少 `struct btree_insert_entry` 的完整信息。 |
| `Btree::split_root()` | `__bch2_btree_node_split()` + `bch2_btree_update_start()` | **⚠ 偏差** | 1. Rust 版本只做了最基本的分裂（split leaf/internal + 新建根），缺少 C 的复杂度（多节点分裂、扇区预留、write_blocked 链表、异步写完成回调）。2. C 的 `btree_update` 支持递归 root→interior 更新（Rust 只处理了 root）。3. C 通过 `bch2_btree_update_start/end` 管理生命周期——Rust 通过 `BtreeInteriorUpdate` 状态机。 |
| `BtreeEngine::insert_guarded()` | `bch2_btree_iter_peek()` + `journal_keys` overlay | **✓ 对齐** | 概念对齐——journal 重放期间将写入缓冲到 overlay 中。 |

### 3.4 事务 API

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `BtreeTrans::new()` | `bch2_trans_get()` | **✓ 对齐** | 简化实现。 |
| `BtreeTrans::begin()` | `bch2_trans_begin()` | **✓ 对齐** | 重置 iter 和状态。 |
| `BtreeTrans::commit()` | `bch2_trans_commit()` | **⚠ 偏差** | 1. Rust 版本实现了重新加锁（relock）+ 重启循环，但**缺少完整的 `btree_insert_entry` -> 触发器 -> do_bch2_trans_commit 路径**。2. C 的 commit 包含：检查是否有更新 → 运行 transactional 触发器 → 获取所有必要的锁 → 排序更新 → 运行 atomic 触发器 → 预留 journal 空间 → 写 journal → 应用 btree 更新 → 清理。Rust 的 commit 只做了 `begin()/iter_relock() → 检查 restart → 检查 committed`，实际写入委托给调用者。3. **Journal pin 集成不完整**（P1-4）。 |
| `BtreeTrans::drain_journal()` | 无直接对应 | **⚠ 偏差** | Rust 特有——将积累的修改作为 journal entries 取出。C 的修改通过 `struct btree_insert_entry` 直接在 `bch2_trans_commit()` 中应用。 |

### 3.5 迭代器 API

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `BtreeIter::init()` | `bch2_trans_iter_init()` + `bch2_btree_iter_traverse()` | **⚠ 偏差** | 1. Rust 初始化时同时完成下降（从 root 到 leaf 的完整遍历 + 加锁）。C 分两步：初始化（`bch2_trans_iter_init`）设置 pos/btree_id/flags，然后可选地 traverse（`bch2_btree_iter_traverse`）。2. Rust 的加锁策略固定（`try_lock_read` + 可选保留读锁）——C 使用 `btree_path` 的 `locks_want` 和 `nodes_locked` 位图。3. C 支持 `btree_path` 复用/共享——Rust 每次 init 创建新的 path。 |
| `BtreeIter::peek()` | `bch2_btree_iter_peek()` | **✓ 对齐** | 返回 (key, value) 对。 |
| `BtreeIter::peek_entry()` | 同上 | **✓ 对齐** | 返回 BtreeEntry。 |
| `BtreeIter::advance()` | `bch2_btree_iter_next()` / `bch2_btree_iter_next_slot()` | **⚠ 偏差** | Rust 的 advance 只在一个 leaf 节点内移动，叶子节点用完后停止。C 的 `bch2_btree_iter_next_slot()` 通过 `btree_path` 自动跨节点跳转（寻找到 sibling 节点）。 |
| — | `bch2_btree_iter_peek_prev()` | **❌ 缺失** | Rust 不支持反向遍历（P3-3）。 |
| — | `bch2_btree_iter_peek_slot()` | **❌ 缺失** | C 的 slot 迭代专门用于精确位置查找（P3-3）。 |
| `BtreeIter::find_child_node()` | `btree_path_down()` / `bch2_btree_node_child_access()` | **✓ 对齐** | 在 internal 节点中查找子节点地址。 |
| — | `bch2_btree_path_traverse_one()` | **❌ 缺失** | C 的完整路径遍历，包含 `traverse_all` 机制——Rust 每次 init 独立遍历（P3-3）。 |

### 3.6 缓存相关

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `BtreeCache::get_or_load()` | `bch2_btree_node_get()` | **⚠ 偏差** | 1. Rust 使用 `HashMap` + `Mutex`——C 使用 `rhashtable`（无锁读）+ `clean/dirty` 链表。2. Rust 的驱逐策略偏好 leaf 节点——C 使用 LRU + shrinker + 两级扫描（`accessed` bit）。 |
| `BtreeCache::mark_dirty()` | `bch2_btree_node_set_dirty()` | **✓ 对齐** | 概念一致。 |
| `BtreeCache::flush_dirty()` | `bch2_btree_node_flush0/flush1()` | **⚠ 偏差** | Rust 一次性取出所有脏节点——C 通过 journal pin 回调逐节点刷新。 |
| — | `bch2_btree_cache_cannibalize_lock/unlock()` | **❌ 缺失** | C 支持内存压力时的节点"同类相食"机制（P3-3）。 |
| — | `bch2_btree_evicted_size_record/lookup()` | **❌ 缺失** | C 跟踪被驱逐节点的预期大小用于优化（P3-3）。 |

### 3.7 锁操作

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `BtreeNode.lock::try_lock_read/unlock_read/try_lock_write` | `six_lock_read()` / `six_lock_write()` 等 | **✓ 对齐** | volmount 的 `SixLock` 是 bcachefs `six_lock` 的 Rust 移植。 |
| — | `bch2_btree_lock_init()` | **❌ 缺失** | Rust 的 SixLock 在 `BtreeNode::new()` 中初始化（简化），不需要单独初始化函数。 |
| — | `bch2_trans_unlock_write()` | **❌ 缺失** | C 有事务级别的写锁批量释放——Rust 通过 iter path 管理（P3-3）。 |
| — | `bch2_btree_node_upgrade()` | **❌ 缺失** | C 有专门的锁升级函数——Rust 通过 `try_lock_write()` 直接尝试（P3-3，但可用现有 SixLock API）。 |
| — | `bch2_btree_path_downgrade()` | **❌ 缺失** | C 有专门的锁降级函数——Rust 无（P3-3）。 |
| — | `bch2_trans_relock()` / `bch2_trans_unlock()` | **❌ 缺失** | C 有事务级别 relock/unlock——Rust 通过 `BtreeTrans::commit()` 中的 relock 逻辑间接实现（P3-3）。 |

### 3.8 内部节点操作

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `BtreeInteriorUpdate::new()` | `bch2_btree_update_start()` | **⚠ 偏差** | Rust 版本简单——C 版本需要 `disk_reservation`、`btree_id`、`start/end pos` 等。 |
| — | `bch2_btree_update_start()` 完整签名 | **❌ 缺失** | C: `struct btree_update *bch2_btree_update_start(struct btree_trans *, enum btree_id, unsigned, unsigned, unsigned, enum btree_node_rewrite_reason, gfp_t)` |
| — | `bch2_btree_node_check_topology()` | **❌ 缺失** | 拓扑一致性检查。 |
| — | `bch2_btree_node_prep_for_write()` | **❌ 缺失** | 写前准备（bset 合并 + 格式更新）。 |
| — | `bch2_btree_bset_insert_key()` | **❌ 缺失** | C 的 bset 键插入函数——Rust 通过 `BtreeNode::insert()` 实现。 |

### 3.9 触发器系统

| Rust fn | C fn | 状态 | 差距描述 |
|---------|------|:----:|----------|
| `TriggerRegistry::run_triggers()` | `bch2_trans_commit_run_triggers()` | **✓ 对齐** | 三阶段设计（Transactional → Atomic → Gc）对应 C 的 `run_one_mem_trigger/trans_trigger/gc_trigger`。 |
| `TriggerFn` 签名 | `btree_key_cache_trigger` 等 | **⚠ 偏差** | Rust 签名 `(engine, btree_type, key_bytes, old_val_bytes, new_val_bytes)` 更通用但也更底层（所有值序列化为字节切片）。C 使用 `struct bkey_s_c` 和 `struct bkey_s`（强类型 key/value）。 |
| `TriggerRegistry::register()` | 通过 `bkey_ops` 表 | **✓ 对齐** | 概念对齐——按 `(BtreeId, KeyType)` 注册。 |

### 3.10 子模块 API 对比

| Rust 模块 | C 对应 | 状态 | 差距描述 |
|-----------|--------|:----:|----------|
| `snapshot.rs` → `SnapshotTree` | `fs/snapshots/snapshot.h` | **✓ 对齐** | `SnapshotNode`（对应 `snapshot_t`）包含 `id, parent_id, children, subtree_max, depth, skip[3]`——和 C 一致。 |
| `key.rs` → pack/unpack | `fs/btree/bkey.c` + `bset.c` | **✓ 对齐** | `bkey_pack_pos/bkey_unpack_pos` 对应 C 的 `bch2_bkey_pack_pos/__bkey_unpack_pos`。位流编解码使用 LE 高→低位打包，与 bcachefs 一致。 |
| `key.rs` → `BKEY_NR_FIELDS = 5` | `BKEY_NR_FIELDS = 6` | **⚠ 偏差** | Rust 定义了 5 个 field（INODE, OFFSET, SNAPSHOT, PADDR, VER）。C 定义了 6 个（加 SIZE=3, VERSION_HI=4, VERSION_LO=5）。Rust 合并了 VERSION_HI/VERSION_LO 为 16bit 的 VER 字段，并且用 PADDR(48bit) 替换了 SIZE 字段。**格式不兼容**（P1-2）。 |

---

## 4. 命名惯例对比

| 概念 | bcachefs C | volmount Rust | 状态 |
|------|-----------|---------------|:----:|
| 搜索位置 | `bpos` (inode, offset, snapshot) | `Bpos` (vol_id, offset, snapshot) | ⚠ 字段名不同 |
| 磁盘 key | `bkey` / `bkey_packed` | `BkeyPacked` | ✓ |
| 键格式 | `bkey_format` | `BkeyFormat` | ✓ |
| 内存 key | `bkey_i` / `bkey_s_c` | `BtreeKey` / `BtreeEntry` | ⚠ 概念不同 |
| B-tree 节点 | `struct btree` | `BtreeNode` | ✓ |
| 迭代器 | `struct btree_iter` / `struct btree_path` | `BtreeIter` | ⚠ 合并了 iter + path |
| 事务 | `struct btree_trans` | `BtreeTrans` | ✓ |
| 节点缓存 | `struct bch_fs_btree_cache` | `BtreeCache` / `NodeCache` | ⚠ 拆分 |
| 内部更新 | `struct btree_update` | `BtreeInteriorUpdate` | ✓ 但简化 |
| 常量前缀 | `BTREE_*`, `BKEY_*` | `BTREE_*`, `BKEY_*` | ✓ 一致 |
| 函数前缀 | `bch2_btree_*` / `bch2_trans_*` | `Btree::*` / `BtreeTrans::*` | ⚠ 使用 method 而非前缀 |

---

## 5. 架构差异详细分析

### 5.1 磁盘格式不兼容（P1-1）

**核心问题**: Rust 的 `BkeyPacked` + `BkeyFormat` 打包字段与 C 不兼容。

| Field index | bcachefs C | Rust | 影响 |
|:-----------:|-----------|------|------|
| 0 | `INODE` (64bit) | `BKEY_FIELD_INODE` (64bit) | ✓ 相同 |
| 1 | `OFFSET` (64bit) | `BKEY_FIELD_OFFSET` (64bit) | ✓ 相同 |
| 2 | `SNAPSHOT` (32bit) | `BKEY_FIELD_SNAPSHOT` (32bit) | ✓ 相同 |
| 3 | `SIZE` (32bit) | `BKEY_FIELD_PADDR` (48bit) | ✗ 不同 |
| 4 | `VERSION_HI` (32bit) | `BKEY_FIELD_VER` (16bit) | ✗ 不同 |
| 5 | `VERSION_LO` (64bit) | — (缺失) | ✗ 缺失 |

C 的 `BKEY_FORMAT_CURRENT` = 6 个 field，共 264bit/33B 的 key 数据 + 1B pad + 3B header = 40B = 5 u64s。
Rust 的 `BKEY_FORMAT_CURRENT` = 5 个 field，共 224bit/28B 的 key 数据 + 3B header = 31B = 4 u64s（BKEY_U64S=3）。

**这意味着用 Rust 格式打包的数据无法被 bcachefs 内核读取，反之亦然。**

### 5.2 `Bpos` 的 inode vs vol_id（P1-2）

Rust 将 `Bpos::vol_id` 在 `.from_key()` 中始终设为 0，仅在内部使用。这意味着 `Bpos` 的 `vol_id` 字段未被正确使用——它是 `inode` 的别名，应该从 `BtreeKey::vaddr`（这是 offset）以外的来源获取。

C 的 `bpos.inode` = 文件 inode 号。
Rust 的 `Bpos.vol_id` = 卷 ID，与 inode 不同。

### 5.3 `Btree` 结构设计差异（多个系统）

bcachefs 的 B-tree 系统采用 `bch_fs`（全局文件系统结构）-> `btree_root[]`（每 btree 类型根）-> `btree`（节点）-> `bset_tree`（多 bset）-> `bkey_packed`（keys）的 5 层架构。

Rust 的 B-tree 系统采用 `BtreeEngine` -> `[Btree; 6]` -> `BtreeRoot{node, depth}` -> `BtreeNode` -> `[BsetTree; 3]` -> `BkeyPacked` 的 5 层架构。

**架构层次一致**，但因 C 和 Rust 的内存模型不同，底层实现有本质差异：
- bcachefs 大量使用指针/引用/位操作
- Rust 使用 `Arc` + `Vec<u8>` + 原子操作

### 5.4 路径 vs 迭代器（P2-2/P3-3）

bcachefs 的关键设计之一是 **iter 和 path 分离**：
- `btree_iter`（高级 API）——持有 `btree_path_idx_t` 引用
- `btree_path`（低级路径）——负责加锁 + 路径维护
- 多个 iter 可以共享同一个 path（通过 refcount）
- `btree_path` 支持 `should_be_locked` 标志——事务重启后自动 relock

Rust 的 `BtreeIter` 既是 iter 又是 path：
- `path: Vec<BtreePathLevel>` 直接持有一组 `Arc<BtreeNode>` 引用
- 没有 refcount——每个 iter 独占 path
- 没有 `should_be_locked` 标志——重启后需要重新 init 遍历
- 没有 `locks_want` 和 `nodes_locked` 位图——锁状态分散在 `BtreePathLevel::lock_state`

### 5.5 事务提交路径（P1-4）

bcachefs 的 `bch2_trans_commit()` 执行（按顺序）：
1. `bch2_trans_commit_run_triggers(trans, BTREE_TRIGGER_transactional)` — 运行 transactional 触发器
2. `bch2_trans_commit_get_locks(trans)` — 获取所有必要的锁（可能重启）
3. `bch2_trans_commit_run_triggers(trans, BTREE_TRIGGER_atomic)` — 运行 atomic 触发器
4. `bch2_trans_commit_write_locked(trans)` — 预留 journal 空间 + 写入 journal
5. `bch2_trans_commit_apply(trans)` — 将更新应用到 btree 节点
6. 写完成后运行 GC 触发器

Rust 的 `BtreeTrans::commit()` 执行：
1. `begin()` 重置状态
2. relock 所有 iter 的路径
3. 检查 `needs_restart`，重启循环
4. 实际修改通过 `Btree::insert/delete` 直接操作
5. 修改记录在 `journal` 列表中，供调用者 drain

**Rust 缺失**: 触发器执行、old_v 跟踪、排序更新、journal 预留和写确认。

### 5.6 Key Cache（P3-1）

bcachefs 有完整的 key cache（`struct bkey_cached`），位于 `btree_key_cache_types.h` 和 `btree/key_cache.h` 中：
- 按 `(btree_id, pos)` 哈希
- 支持 dirty/clean 状态
- 通过 journal pin 管理持久性
- 支持 `BTREE_ITER_cached` 标志——通过 key cache 而非 btree 遍历读取

Rust 的 `KeyCache`（在 `key_cache.rs` 中）是一个简单的 `HashMap<Bpos, Option<(BtreeKey, BchVal)>>` 包装：
- 单层缓存（没有区分 dirty/clean）
- 没有 journal pin 集成
- 仅用于最热的 key（无逐出策略）

### 5.7 Write path 简化

Rust 的写路径使用 `Arc::get_mut` 获取唯一引用：
```rust
let node = match Arc::get_mut(&mut self.root.node) {
    Some(n) => n,
    None => return false,  // 有其他引用持有者
};
```
这在当前单线程测试中有效，但在并发写场景下 `get_mut` 会失败（Arc 有其他 clone）。C 使用 SIX 锁（`six_lock_write`）允许多个写线程并发获取不同节点的写锁。

### 5.8 Split/Merge 阈值

| 常量 | bcachefs C | volmount Rust | 状态 |
|------|-----------|---------------|:----:|
| 分裂阈值 | `BTREE_SPLIT_THRESHOLD` (cache.h:189) | `SPLIT_THRESHOLD_NUM/DEN = 3/4` | ✓ |
| 合并阈值 | `BTREE_FOREGROUND_MERGE_THRESHOLD` (interior.h:195) | `MERGE_THRESHOLD_NUM/DEN = 1/3` | ✓ |
| 合并高水位 | `MERGE_HIGHER` (interior.h:197, 3/5) | `MERGE_HIGHER_NUM/DEN = 3/5` | ✓ |
| 合并滞后 | `MERGE_HYSTERESIS` (interior.h:199, 5/12) | `MERGE_HYSTERESIS_NUM/DEN = 5/12` | ✓ |
| 平衡分裂 | `find_balanced_split` (3/5 target) | `BALANCE_TARGET_NUM/DEN = 3/5` | ✓ |

**阈值常量完全对齐**。

---

## 6. 详细差异清单

### 6.1 P1（必须修复）

| ID | 文件 | 行号 | 描述 |
|----|------|------|------|
| P1-1 | `key.rs` | 全局 | 磁盘 key 格式与 bcachefs 不兼容（缺少 SIZE/VERSION_HI/VERSION_LO 字段，使用 PADDR/VER 替代）。导致 volmount 序列化的 btree 数据无法被 bcachefs 读取。 |
| P1-2 | `key.rs` | 130-135 | `Bpos` 字段名为 `vol_id`（代替 C 的 `inode`）。`Bpos::from_key()` 将 vol_id 硬编码为 0，丢失了真实的 inode/卷信息。 |
| P1-3 | `iter.rs` | 46-59 | `BtreeIter` 没有 `btree_id` 字段，无法按 btree 类型做快照过滤。C 的 `btree_iter` 在 `bch2_btree_iter_flags()` 中根据 btree_id 自动配置 `filter_snapshots/all_snapshots`。 |
| P1-4 | `transaction.rs` | 100-129 | `BtreeTrans` 缺少 `btree_insert_entry` 数组——事务提交时无法跟踪 old_v、触发器状态、路径引用。journal 列表过于简单。 |
| P1-5 | `node.rs` | 98-118 | `BtreeNode` 缺少 `write_blocked` 链表和 `list` LRU 节点——无法支持 bcachefs 的异步分裂/合并等待机制和 LRU 缓存。 |

### 6.2 P2（建议修复）

| ID | 文件 | 行号 | 描述 |
|----|------|------|------|
| P2-1 | `key.rs` | 763-767 | `BtreeKey` 没有 `bversion`——丢失版本跟踪和 extent size。C 的 `bkey` 包含 12 字节 `bversion` 和 4 字节 `size`。 |
| P2-2 | `iter.rs` | 46-59 | `BtreeIter` 没有 `path` 引用计数和 `intent_ref`——多 iter 共享路径时可能导致双重解锁或数据竞争。 |
| P2-3 | `iter.rs` | 26-31 | `IterFlags` 只有 `intent` 和 `forward`——缺少 C 的 `prefetch, cached, all_snapshots, filter_snapshots, with_journal, nofill` 等 15+ 标志位。 |
| P2-4 | `cache.rs` | 22-48 | `BtreeCache` 使用 `Mutex<HashMap>` + 固定大小阈值（1024/256），没有 shrinker 集成。C 使用 `rhashtable` + 链表 + shrinker + 两级访问保护。 |
| P2-5 | `update.rs` | 52-65 | `BtreeInteriorUpdate` 缺少磁盘预留、异步写完成回调、write_blocked_list——与 C 的 `struct btree_update` 差距大。 |
| P2-6 | `transaction.rs` | 85-89 | `BtreePath::sort_key()` 比较逻辑简化——C 的 `__btree_path_cmp` 更复杂（处理 `cached` 路径等）。 |
| P2-7 | `btree.rs` | 288-352 | `split_root()` 只处理了 root 分裂，没有处理深层次的分裂传播。C 的 `btree_split` 支持递归层次分裂。 |
| P2-8 | `btree.rs` | 359-368 | `with_transaction()` 每次都新建 `BtreeTrans`——每次新建成本高。C 复用 `bch2_trans_get()` 的线程本地事务池。 |

### 6.3 P3（功能缺失）

| ID | 描述 |
|----|------|
| P3-1 | **Key cache**: `struct bkey_cached` 和 `btree_key_cache_*` API 完全缺失。Rust 的 `KeyCache` 仅是一个 HashMap 兜底。 |
| P3-2 | **节点重写**: `bch2_btree_node_rewrite()` 和 `NEED_REWRITE` 标志缺失。 |
| P3-3 | **溢出页/Snapshot btree**: C 有 `BTREE_ID_snapshots` btree 用于快照元数据。Rust 有 `BtreeId::Snapshots` 但内部通过 `SnapshotTree`（HashMap）独立管理，而非 btree 存储。 |
| P3-4 | **Journal replay 集成**: C 在 `bch2_btree_iter_peek()` 中通过 `BTREE_ITER_with_journal` 遍历 journal 中的未持久化 key。Rust 的 `BtreeTrans` 有 `journal` 列表但读路径不检查 journal。 |
| P3-5 | **btree_node_iter 多 bset 合并遍历**: C 的 `btree_node_iter` 对 3 个 bset 做合并（merge）遍历。Rust 通过 `BtreeNode::scan_entry()` 做线性扫描——缺少合并遍历导致删除/白化的条目不能被跳过。 |
| P3-6 | **SRCU 保护**: C 的 `bch2_trans_begin()` 获取 SRCU 读锁，在 long wait 时释放。Rust 无此机制。 |
| P3-7 | **`shard_cpu` 和 per-CPU 映射**: C 通过 pin 到 CPU 保持缓存热度。Rust 无此机制。 |
| P3-8 | **IO 完成回调和 closure 机制**: C 使用 `closure` 管理异步 IO。Rust 使用 `async/await`——架构不同，但 bcachefs 的回调无法直接映射。 |
| P3-9 | **`BtreePath::update_path` 和 `key_cache_path`**: C 的 iter 通过额外的 path 跟踪更新位置和 key cache 位置。Rust 无此功能。 |
| P3-10 | **`should_be_locked` 路径复用**: C 的标记允许事务重启后自动 relock 已遍历路径。Rust 的 `BtreeIter` 需要在事务重启后完全重新创建。 |

---

## 7. 建议行动计划

### Phase A：高优先级修复（P1）

1. **修复 Bpos 字段名**（P1-2）：`vol_id` → `inode`，修正 `from_key()` 使其传递真实 inode
2. **对齐磁盘 key 格式**（P1-1）：在 `BkeyFormat` 中添加 `SIZE/VERSION_HI/VERSION_LO` 字段，使 `BKEY_NR_FIELDS` 从 5 变为 6，对齐 C 的 `enum bch_bkey_fields`
3. **添加 `btree_id` 到 `BtreeIter`**（P1-3）：支持 per-btree 快照过滤
4. **完善 `BtreeTrans` 提交路径**（P1-4）：添加 `btree_insert_entry` 数组，支持 old_v 跟踪和触发器集成
5. **添加 `write_blocked` 链表**（P1-5）：支持异步分裂/合并等待

### Phase B：中期修复（P2）

1. Path 引用计数：实现 `BtreePath` 的 `ref/intent_ref`
2. `IterFlags` 扩展：添加 `prefetch, cached, all_snapshots` 等标志位
3. 缓存 shrinker：添加内存压力下自动清理机制
4. `BtreeInteriorUpdate` 增强：添加磁盘预留、异步写完成回调

### Phase C：长期工作（P3）

1. Key cache 完整实现
2. 溢出页支持
3. Journal replay 读穿透
4. 多 bset 合并遍历
5. SRCU 保护机制

---

## 附录 A：文件映射

| bcachefs C 头文件 | volmount Rust 文件 | 对齐度 |
|-------------------|-------------------|:------:|
| `bcachefs_format.h` (bpos, bkey, etc.) | `key.rs` | ⚠ 偏差 (field mismatch) |
| `btree/types.h` (btree struct, iter, trans) | `node.rs`, `types.rs`, `iter.rs`, `transaction.rs` | ⚠ 偏差 |
| `btree/bkey.h` | `key.rs` | ✓ 对齐 |
| `btree/bkey_types.h` | `key.rs` (KeyType) | ✓ 对齐 |
| `btree/bkey_cmp.h` | `key.rs` (Ord impl) | ✓ 对齐 |
| `btree/bkey_methods.h` | `key.rs` | ⚠ 偏差 (缺少 val 方法) |
| `btree/bset.h` | `node.rs`, `key.rs` | ✓ 对齐 |
| `btree/iter.h` | `iter.rs` | ⚠ 偏差 (缺少 path) |
| `btree/update.h` | `btree.rs`, `update.rs` | ⚠ 偏差 (简化) |
| `btree/interior.h` | `update.rs` | ⚠ 偏差 (简化) |
| `btree/cache.h` | `cache.rs` | ⚠ 偏差 |
| `btree/locking.h` / `locking_types.h` | `lock/six.rs` (外部) | ✓ 对齐 |
| `btree/key_cache.h` / `key_cache_types.h` | `key_cache.rs` | ⚠ 偏差 (简化) |
| `snapshots/snapshot.h` | `snapshot.rs` | ✓ 对齐 |

---

## 附录 B：公共 API 覆盖率

| volmount 公共 API | 有 C 对应 | 状态 |
|--------------------|:---------:|:----:|
| `Bpos` | ✓ `struct bpos` | ⚠ 字段不兼容 |
| `BkeyPacked` | ✓ `struct bkey_packed` | ✓ |
| `BkeyFormat` | ✓ `struct bkey_format` | ✓ |
| `BtreeKey` | ⚠ `struct bkey` | 缺失 bversion/size |
| `BchVal` / `Addr48` | ⚠ `struct bch_val` | 简化（`Addr48` 为 Rust 特有 48-bit 地址包装类型） |
| `BtreeEntry` | ⚠ `struct bkey_i` | 简化 |
| `BtreeNode` | ✓ `struct btree` | 缺失 write_blocked |
| `BtreePathLevel` | ✓ `struct btree_path_level` | ✓ |
| `BtreeRoot` | ✓ `struct btree_root` | ✅ |
| `NodeCache` | ⚠ `struct bch_fs_btree_cache` | 简化 |
| `BtreePtrV2` | ✓ `struct bch_btree_ptr_v2` | ✓ |
| `BtreeIter` | ⚠ `struct btree_iter` | 缺失 path |
| `BtreeTrans` | ✓ `struct btree_trans` | 缺失 updates |
| `BtreeInteriorUpdate` | ⚠ `struct btree_update` | 简化 |
| `BtreeCache` | ✓ `struct bch_fs_btree_cache` | ⚠ 无 rhashtable |
| `SnapshotTree` | ✓ 快照树概念 | ✓ |
| `TriggerRegistry` / `TriggerPhase` | ✓ `bkey_ops` 三阶段 | ✓ |
| `BtreeEngine` | ⚠ `bch_fs.btree_roots[]` | 简化 |
| `KeyCache` | ⚠ `struct bkey_cached` | 严重简化 |
| `Btree` (主 struct) | ⚠ 概念对应 `bch_fs` | 简化 |

---

*审计完成日期：2026-06-24 | 审计人：volmount-agent | 参考版本：bcachefs-tools v1.38.6-36-g499dbe7e0*
