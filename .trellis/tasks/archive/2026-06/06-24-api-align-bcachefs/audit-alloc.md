# Alloc API Gap Analysis: volmount-core vs bcachefs kernel

> 审计日期: 2026-06-24
> 审计范围: `volmount-core/src/alloc/` ↔ `bcachefs-tools/fs/alloc/` (C headers + .c sources)
> 参考文件: `types.h`, `foreground.h`, `foreground.c`, `background.h`, `format.h`, `buckets_types.h`, `buckets.h`, `accounting_format.h`

---

## 1. Type Definitions 对比

### 1.1 数据类型枚举

| Rust (volmount) | C (bcachefs) | 状态 | 差距 |
|---|---|---|---|
| `BchDataType` (4 变体: Free, User, NeedGcGens, Reserved) | `enum bch_data_type` (11 变体: free, sb, journal, btree, user, cached, parity, stripe, need_gc_gens, need_discard, unstriped) | ❌ **严重缺失** | volmount 缺少 sb(1), journal(2), btree(3), cached(5), parity(6), stripe(7), need_discard(9), unstriped(10)。`Reserved` 不在 bcachefs 的 data_type 枚举中。 |

**影响**: P1 — 缺少 data_type 导致无法区分元数据/数据/缓存/日志 bucket，影响分配策略、回收策略和 GC。

### 1.2 Bucket 结构

| Rust `Bucket` | C `struct bucket` (buckets_types.h:37-45) | 状态 | 差距 |
|---|---|---|---|
| `state: BchDataType` | `data_type:7` (bitfield) | 🟡 部分对齐 | bcachefs 用 7-bit + 1-bit lock，volmount 用完整枚举 |
| `group: u32` | (无对应字段) | ➕ volmount 额外 | volmount 记录 AG 归属，bcachefs 由 bucket index 隐含 dev |
| `version: u32` | `gen: u8` | ⚠️ **差异** | volmount 用 32-bit version，bcachefs 用 8-bit gen (wrapping) |
| `bucket_idx: u64` | (隐式由数组索引决定) | ➕ volmount 额外 | bcachefs 通过 `bucket_gens[]` 数组位置确定索引 |
| (缺失) | `lock: u8` (bit_spin_lock) | ❌ 缺失 | volmount 通过 Mutex 保护 AG，无 per-bucket 锁 |
| (缺失) | `gen_valid:1` | ❌ 缺失 | 用于 GC 阶段 gen 有效性标记 |
| (缺失) | `dirty_sectors: u32` | ❌ **P1 缺失** | 脏扇区计数，用于 alloc_data_type() 计算 |
| (缺失) | `cached_sectors: u32` | ❌ **P1 缺失** | 缓存扇区计数 |
| (缺失) | `stripe_sectors: u32` | ❌ **P2 缺失** | EC 条带扇区计数 |

### 1.3 Alloc btree 条目

| Rust `AllocEntry` | C `struct bch_alloc_v4` (format.h:82-99) | 状态 | 差距 |
|---|---|---|---|
| `state: BchDataType` (4 变体) | `data_type: u8` (11 变体) | ❌ **P1 缺失** | 数据类型空间不足 |
| `group: u32` | (无对应) | ➕ volmount 额外 | — |
| `version: u32` | `gen: u8` + `oldest_gen: u8` | ❌ **P2 缺失** | 缺少 oldest_gen，无法计算 GC gen |
| (缺失) | `journal_seq_nonempty: u64` | ❌ **P2 缺失** | 状态转换 journal 序列号追踪 |
| (缺失) | `flags: u32` (NEED_DISCARD, NEED_INC_GEN, BACKPOINTERS_START, NR_BACKPOINTERS) | ❌ **P2 缺失** | 丢弃/GC/后向指针位 |
| (缺失) | `dirty_sectors: u32` | ❌ **P1 缺失** | — |
| (缺失) | `cached_sectors: u32` | ❌ **P1 缺失** | — |
| (缺失) | `io_time[2]: u64` | ❌ **P3 缺失** | LRU 淘汰时间戳 (read/write) |
| (缺失) | `stripe_refcount: u32` | ❌ **P3 缺失** | EC 条带引用计数 |
| (缺失) | `nr_external_backpointers: u32` | ❌ **P3 缺失** | 后向指针计数 |
| (缺失) | `journal_seq_empty: u64` | ❌ **P3 缺失** | 清理后的 journal 序列 |
| (缺失) | `stripe_sectors: u32` | ❌ **P3 缺失** | EC 条带扇区 |

**影响**: AllocEntry 仅 3 字段 vs 14+ 字段的 bch_alloc_v4。volmount 缺少完整的扇区记账、journal 追踪、后向指针和 IO 时间。

### 1.4 Open Bucket 结构

| Rust `OpenBucket` | C `struct open_bucket` (types.h:65-91) | 状态 | 差距 |
|---|---|---|---|
| `pin: AtomicU32` | `pin: atomic_t` | ✅ 对齐 | — |
| `valid: AtomicBool` | `valid:1` (bitfield) | ✅ 对齐 | — |
| `group_id: AtomicU32` | `dev: u8` | ✅ 概念对齐 | 类型宽度不同，volmount 用 group_id 替代 dev |
| `bucket_bi: AtomicU32` | `bucket: u64` | ⚠️ **差异** | bcachefs 直接存完整 bucket number |
| `freelist: AtomicU16` | `freelist: open_bucket_idx_t` (u16) | ✅ 对齐 | — |
| `hash: AtomicU16` | `hash: open_bucket_idx_t` (u16) | ✅ 对齐 | — |
| (缺失) | `lock: spinlock_t` | ❌ 缺失 | volmount 使用 Mutex 保护池而非 per-ob 锁 |
| (缺失) | `ec_idx: u8` | ❌ **P3 缺失** | EC 条带索引 |
| (缺失) | `data_type:5` (bitfield) | ❌ **P2 缺失** | 写时数据类型标记 |
| (缺失) | `on_partial_list:1` | ❌ **P3 缺失** | 部分桶列表标记 |
| (缺失) | `do_discards_fast:1` | ❌ **P3 缺失** | 快速丢弃标记 |
| (缺失) | `gen: u8` | ❌ **P2 缺失** | 分配时的 bucket gen |
| (缺失) | `sectors_free: u32` | ❌ **P1 缺失** | 桶内可用扇区数 |
| (缺失) | `ec: *ec_stripe_new` | ❌ **P3 缺失** | EC 条带指针 |

### 1.5 Write Point 结构

| Rust `WritePoint` | C `struct write_point` (types.h:130-159) | 状态 | 差距 |
|---|---|---|---|
| `identity: Option<u64>` | `write_point: unsigned long` | ✅ 概念对齐 | — |
| `last_used: u64` | `last_used: u64` | ✅ 对齐 | — |
| `sectors_free: u64` | `sectors_free: unsigned` | ✅ 对齐 | — |
| `prev_sectors_free: u64` | `prev_sectors_free: unsigned` | ✅ 对齐 | — |
| `ptrs: Vec<OpenBucketIdx>` | `ptrs: struct open_buckets` (固定大小数组) | ✅ 对齐 | 存储方式不同但功能等价 |
| `hint: u64` | (无直接对应) | ➕ volmount 额外 | volmount 用 hint 做 AG 轮询 |
| (缺失) | `node: hlist_node` | ✅ 替代 | volmount 用 HashMap 替代 hlist |
| (缺失) | `lock: mutex` | ❌ **P1 缺失** | 无 per-writepoint 锁保护 |
| (缺失) | `data_type: enum bch_data_type` | ❌ **P2 缺失** | 写点绑定的数据类型 |
| (缺失) | `stripe: dev_stripe_state` | ❌ **P2 缺失** | 加权 round-robin 设备时钟 |
| (缺失) | `sectors_allocated: u64` | ❌ **P3 缺失** | 累计分配扇区统计 |
| (缺失) | `index_update_work` | ❌ **P3 缺失** | 异步索引更新 |
| (缺失) | `writes: list_head` | ❌ **P3 缺失** | 正在进行中的写入 |
| (缺失) | `writes_lock: spinlock_t` | ❌ **P3 缺失** | writes 链表锁 |
| (缺失) | `state: enum write_point_state` | ❌ **P3 缺失** | 写点状态机 (stopped/waiting_io/waiting_work/runnable/running) |

### 1.6 Allocator 顶层结构

| Rust `BlockAllocator` | C `struct bch_fs_allocator` (types.h:191-215) | 状态 | 差距 |
|---|---|---|---|
| `groups: Vec<Mutex<AllocGroup>>` | (内核按 per-device 组织，无显式 Group) | 🟡 概念不同 | volmount 使用 AG 实现并发，bcachefs 按设备 |
| `total_blocks: u64` | (隐含在 device config) | ✅ 功能等价 | — |
| `allocated: AtomicU64` | (无直接对应，使用 counters) | 🟡 简化 | bcachefs 通过 `bch_dev_usage` 精确计算 |
| `hint: AtomicU64` | (无直接对应) | ➕ volmount 额外 | — |
| `write_points: Option<Mutex<WritePointPool>>` | `write_points[WRITE_POINT_MAX]` + `write_points_hash[]` | 🟡 概念对齐 | volmount 用 HashMap+hint，bcachefs 用 hlist+spinlock |
| `open_buckets: OpenBucketPool` | `open_buckets[OPEN_BUCKETS_COUNT]` + `open_buckets_hash[]` | ✅ 对齐 | — |
| (缺失) | `rw_devs: bch_devs_mask[BCH_DATA_NR]` | ❌ **P1 缺失** | 每数据类型的可读写设备集 |
| (缺失) | `freelist_lock: spinlock_t` | ✅ 替代 | volmount 使用 Mutex |
| (缺失) | `freelist_wait: closure_waitlist` | ❌ **P2 缺失** | 空闲桶等待队列 |
| (缺失) | `open_buckets_freelist: open_bucket_idx_t` | ✅ 对齐 | — |
| (缺失) | `open_buckets_nr_free: open_bucket_idx_t` | ✅ 对齐 | — |
| (缺失) | `open_buckets_wait: closure_waitlist` | ❌ **P2 缺失** | open bucket 等待队列 |
| (缺失) | `open_buckets_partial[OPEN_BUCKETS_COUNT]` | ❌ **P3 缺失** | 部分填充桶列表 |
| (缺失) | `btree_write_point: write_point` | ❌ **P2 缺失** | 专用 btree 写点 |
| (缺失) | `reconcile_write_point: write_point` | ❌ **P3 缺失** | reconcile 写点 |

---

## 2. Function Signatures 对比

### 2.1 Bucket 分配

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `allocate_bucket(&self, engine, watermark, wp_id) -> Result<u64, StorageError>` | `bch2_bucket_alloc_trans(btree_trans*, alloc_request*) -> open_bucket*` | ❌ **P1 签名不匹配** | bcachefs 通过 `alloc_request` 传递 data_type, target, nr_replicas, flags 等完整上下文；volmount 仅传 watermar + wp_id |
| (无) | `bch2_bucket_alloc_set_trans(trans, req, stripe)` | ❌ **P1 缺失** | 多设备副本分配 — volmount 无 replicas 概念 |
| (无) | `bch2_alloc_sectors_req(trans, req, write_point, &wp)` | ❌ **P1 缺失** | **核心扇区分配入口** — 含 4 级分配策略: writepoint→partial→stripe→bucket_alloc |
| (无) | `bch2_alloc_sectors_start_trans(trans, ...)` | ❌ **P1 缺失** | alloc_request 的便捷初始化 + alloc_sectors_req |

### 2.2 Open Bucket 操作

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `OpenBucketPool::alloc(group_id, bucket_bi)` | `bch2_open_bucket_alloc(c)` | ✅ 概念对齐 | — |
| `OpenBucketPool::put(idx)` | `__bch2_open_bucket_put(c, ob)` | ✅ 概念对齐 | — |
| `OpenBucketPool::dec_pin_and_put(idx)` | `bch2_open_bucket_put(c, ob)` (atomic_dec_and_test) | ✅ 对齐 | — |
| `OpenBucketPool::inc_pin(idx)` | `atomic_inc(&ob->pin)` | ✅ 对齐 | — |
| `OpenBucketPool::lookup(group_id, bucket_bi)` | `bch2_bucket_is_open(c, dev, bucket)` | ✅ 对齐 | — |
| `OpenBucketPool::is_open(group_id, bucket_bi)` | `bch2_bucket_is_open_safe(c, dev, bucket)` | 🟡 部分对齐 | bcachefs 有 safe 版本 (加锁重新检查) |
| `open_bucket_put_for_addr(block_addr)` | (无直接对应) | ➕ volmount 额外 | — |

### 2.3 Write Point 操作

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `WritePointPool::resolve(id)` | `writepoint_find(trans, write_point)` | ✅ 概念对齐 | 均实现 LRU 查找/淘汰 |
| `WritePointPool::resolve_hint(id)` | (无直接对应) | ➕ volmount 额外 | — |
| (无) | `try_increase_writepoints(c)` | ❌ **P3 缺失** | 动态写点扩容 |
| (无) | `try_decrease_writepoints(trans, old_nr)` | ❌ **P3 缺失** | 动态写点缩容 |

### 2.4 Alloc btree 触发器

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `alloc_extent_trigger(engine, btree_type, key, old_val, new_val)` | `bch2_trigger_alloc(trans, trigger_op)` | 🟡 概念对齐 | 均实现 alloc btree ↔ extent btree 同步 |
| `alloc_freespace_trigger(engine, btree_type, key, old_val, new_val)` | (包含在 bch2_trigger_alloc 中) | 🟡 部分 | bcachefs 的 trigger 同时更新 freespace + LRU + bucket_gens + 容量计数 |
| `gc_trigger(engine, btree_type, key, old_val, new_val)` | `bch2_trigger_alloc` (同一入口) | ✅ 对齐 | — |
| (无) | `bch2_alloc_key_to_dev_counters(trans, ca, old, new, data_type)` | ❌ **P1 缺失** | alloc btree 更新时同步 dev 级别容量计数 |

### 2.5 容量/预留

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `Watermark::reserved_buckets(total)` | `bch2_dev_buckets_reserved(ca, watermark)` | 🟡 概念对齐 | 但 bcachefs 有 7 级 watermark (stripe/normal/copygc/btree/btree_copygc/reclaim/interior_updates)，volmount 只有 3 级 |
| (无) | `bch2_disk_reservation_add/get/put` | ❌ **P1 缺失** | 无扇区级预留机制 |
| (无) | `__dev_buckets_free(ca, usage, watermark)` | ❌ **P2 缺失** | 计算实际空闲桶 (减去 open_buckets 和预留) |

### 2.6 启动/停止

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `BlockAllocator::new(total_blocks, group_size, start_block)` | `bch2_fs_allocator_foreground_init(c)` | 🟡 概念对齐 | bcachefs 显式初始化 open_buckets freelist 和 write_points hash table |
| (无) | `bch2_fs_allocator_background_init(c)` | ❌ **P3 缺失** | 后台分配器初始化 (discard, capacity) |
| (无) | `bch2_fs_capacity_init(c)` | ❌ **P3 缺失** | 容量跟踪子系统初始化 |
| `BlockAllocator::load_from_btree(engine)` | `bch2_alloc_read(c)` | 🟡 概念对齐 | 均从 Alloc btree 恢复 bucket 状态 |

---

## 3. Calling Conventions 对比

### 3.1 事务参数

| 维度 | bcachefs | volmount | 差距 |
|---|---|---|---|
| 事务上下文 | `btree_trans *trans` 贯穿所有分配路径 | `BtreeEngine &engine` 在 allocate_bucket/free 时显式传入 | 🟡 概念等价 |
| 事务重启 | `bch2_trans_restart()` 通过 EAGAIN 错误码通知 | (无事务重启机制) | ❌ **P2 缺失** |
| 事务锁交互 | `bch2_trans_unlock()` + `mutex_lock()` 避免死锁 | (无事务锁升降级) | ❌ **P2 缺失** |

### 3.2 错误处理

| 维度 | bcachefs | volmount | 差距 |
|---|---|---|---|
| 错误模型 | ERR_PTR/PTR_ERR 模式 + 丰富错误码 (BCH_ERR_freelist_empty, BCH_ERR_open_buckets_empty, BCH_ERR_insufficient_devices 等) | `Result<_, StorageError>` 枚举 (AddressSpaceExhausted, Transaction 等) | 🟡 部分对齐。volmount 缺少具体分配错误码 |
| 分配阻塞 | `closure_wait()` + `freelist_wait` 等待队列，支持异步唤醒 | (无阻塞等待 — 立即返回错误) | ❌ **P1 缺失** |
| 重试逻辑 | `will_retry_target_devices` / `will_retry_all_devices` 标志控制重试策略 | (无重试机制) | ❌ **P2 缺失** |

### 3.3 设备管理

| 维度 | bcachefs | volmount | 差距 |
|---|---|---|---|
| 设备对象 | `struct bch_dev *ca` — 完整设备上下文 (名称、容量、bucket size、标注等) | `group_id: u32` — 仅数字标识 | ❌ **P1 缺失** |
| 多设备分配 | `bch2_bucket_alloc_set_trans()` 跨设备分配 | (单分配器无多设备概念) | ❌ **P1 缺失** |
| 设备热插拔 | `bch2_dev_tryget_noerror()` + `bch2_dev_put()` 引用计数 | (不支持) | ❌ **P3 缺失** |

---

## 4. Freespace BTree 操作对比

| Rust | C | 状态 | 差距 |
|---|---|---|---|
| `freespace_insert(engine, bucket_index, gen)` | (通过 alloc trigger 间接操作) | 🟡 概念对齐 | — |
| `freespace_delete(engine, bucket_index, gen)` | (通过 alloc trigger 间接操作) | 🟡 概念对齐 | — |
| `rebuild_freespace_from_alloc(engine)` | `bch2_bucket_do_freespace_index()` | 🟡 概念对齐 | — |
| 实际分配路径使用 free_list (per-AG Vec) | `bch2_bucket_alloc_freelist()` 扫描 Freespace btree | ❌ **P2 设计差异** | volmount 将 freespace btree 仅用于持久化，分配时使用内存 free_list。bcachefs 直接基于 btree 迭代器分配。 |
| (无) | `bch2_check_freespace_key_async(trans, iter, &gen, &journal_seq_empty)` | ❌ **P2 缺失** | 异步验证 freespace key 的有效性 (gen 匹配 + journal seq 检查) |
| (无) | Freespace btree 使用 `alloc_freespace_genbits()` 编码 gen bits | ❌ **P3 缺失** | volmount 的 freespace key 使用 gen 作为 snapshot 字段 |

---

## 5. 汇总差距表

| 编号 | 差距描述 | 严重性 | 涉及的 bcachefs 文件 |
|---|---|---|---|
| G1 | `BchDataType` 未覆盖 11 种类型 | **P1** | accounting_format.h |
| G2 | `struct bucket` 缺少 sector 级记账 | **P1** | buckets_types.h |
| G3 | `AllocEntry` 缺少完整 alloc_v4 字段 | **P1** | format.h |
| G4 | 无 `alloc_request` 分配请求上下文 | **P1** | foreground.h:42-100 |
| G5 | 无 `bch2_alloc_sectors_req()` 多策略分配入口 | **P1** | foreground.c:1466-1649 |
| G6 | 无 `disk_reservation` 系统 | **P1** | buckets.h:341-401 |
| G7 | 无 alloc→dev 容量计数器同步 | **P1** | background.h:331-333 |
| G8 | 无分配等待/阻塞机制 | **P1** | foreground.c:649-681 |
| G9 | `bch_alloc_v4` 的扇区/IO/journal 字段缺失 | **P2** | format.h:82-99 |
| G10 | 缺少 write_point 的 `data_type` 绑定 | **P2** | types.h:130-159 |
| G11 | 缺少 `dev_stripe_state` 加权 round-robin | **P2** | types.h:111-114 |
| G12 | 缺少 per-device 设备对象 | **P2** | foreground.h |
| G13 | 无 `alloc_data_type()` 状态推导 | **P2** | background.h:124-138 |
| G14 | 无 multi-replica 副本分配 | **P2** | foreground.c:915-982 |
| G15 | 缺少 `open_bucket` 的 `sectors_free` 字段 | **P2** | types.h:65-91 |
| G16 | 无 discard 基础设施 | **P2** | types.h:229-269 |
| G17 | write_point 无 per-wp mutex | **P2** | types.h:130-159 |
| G18 | 无 `bucket_gens` btree | **P3** | format.h:113-116 |
| G19 | 无 LRU btree | **P3** | background.h:147-152 |
| G20 | 无 partial bucket 列表 | **P3** | types.h:205-206 |
| G21 | 无 EC 条带分配支持 | **P3** | foreground.c:992-1027 |
| G22 | 无 `backpointer` 追踪 | **P3** | alloc_v4 扩展字段 |
| G23 | 无 write_point 状态机 | **P3** | types.h:116-128 |

**严重性说明**:
- **P1** — 对功能正确性有直接影响，阻碍 bcachefs 兼容性
- **P2** — 影响性能和资源管理精度，但非阻塞
- **P3** — 高级功能，在基础兼容性达成后可推迟

---

## 6. 已对齐的功能（亮点）

尽管存在较多差距，以下核心机制已合理对齐：

| 机制 | 状态 | 说明 |
|---|---|---|
| `open_bucket` 池 + freelist + hash 表 | ✅ 良好对齐 | `open_bucket.rs` 对 bcachefs 的忠实移植 |
| WritePoint LRU 查找/淘汰 | ✅ 良好对齐 | `write_point.rs` 的 `resolve()` ~ `writepoint_find()` |
| Alloc btree 触发器 (extent→alloc) | ✅ 基本对齐 | `alloc_extent_trigger()` 功能等价 |
| Freespace btree 同步 | ✅ 基本对齐 | `alloc_freespace_trigger()` + `rebuild_freespace_from_alloc()` |
| Bucket 级版本号 (防 ABA) | ✅ 对齐 | `Bucket::version` ~ `gen` |
| 分配后注册 open bucket (防 TOCTOU) | ✅ 对齐 | `allocate_bucket` 中 `self.open_buckets.alloc()` |
| Watermark 水位线预留 | 🟡 部分对齐 | 3 级 vs 7 级，但概念一致 |

---

## 7. 下一步行动建议

### P1 优先级 (必须对齐)

1. **扩展 `BchDataType`** 到覆盖所有 11 种类型
2. **实现 `alloc_request` 结构** 作为 allocate_bucket 的入口参数
3. **添加 `AllocEntry` 的扇区记账字段** (dirty_sectors, cached_sectors, stripe_sectors)
4. **实现 `bch2_alloc_sectors_req` 等价函数** — 4 级分配策略入口
5. **添加 `disk_reservation` 系统** 用于扇区级预留

### P2 优先级 (建议对齐)

1. **为 write_point 添加 `data_type` 和 `dev_stripe_state`**
2. **实现 `__dev_buckets_free()` 等价计算**
3. **添加 per-device 对象支持**
4. **实现 `alloc_data_type()` 状态推导函数**
5. **添加 discard 状态追踪**

### P3 优先级 (可选对齐)

1. **partial bucket 列表**
2. **EC 条带分配支持**
3. **LRU btree 同步**
4. **backpointer 追踪**
5. **write_point 状态机**
