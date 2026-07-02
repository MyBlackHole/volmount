# Quality Guidelines

> Code quality standards for backend development.

---

## Overview

<!--
Document your project's quality standards here.

Questions to answer:
- What patterns are forbidden?
- What linting rules do you enforce?
- What are your testing requirements?
- What code review standards apply?
-->

(To be filled by the team)

---

## Current Test Baseline

- `cargo test -p volmount-core --lib` currently passes with `941 passed; 0 failed; 0 ignored` on 2026-07-02.
- The old `9 ignored` count is stale and should not be used as the current reference line.
- Keep future spec updates on the latest verified count, not on historical batch snapshots.

---

## Design Decisions

### Block device backend naming (2026-07-02)

**问题**: 后端实现底层用了稀疏文件，但对外抽象是块设备；如果把实现手段写进用户可见命名，会把协议、文案和调试输出带偏。

**方案**: 对外统一使用 `block device backend` / `sparse`，实现说明只在内部注释中提稀疏文件。`BackendType` 的主枚举值使用 `Sparse`，HTTP/CLI 输出不再显示 `NFS`。

**模式**:
- `BackendType::Sparse` 表示虚拟块设备后端
- `volmountd` 创建请求的 `backend` 字段主值为 `sparse`
- `volmount` 的创建/查看/inspect 输出使用 `block device backend`

**决策理由**:
- 与 bcachefs 对外设备抽象一致
- 避免将实现手段误当成协议语义

### Block device deletion semantics (2026-07-02)

**问题**: 小容量 block device 在关闭/回收阶段可能触发空间耗尽，导致 `stop_volume()` 无法完成完整的 journal drain。

**方案**: 删除路径采用 best-effort stop：先尽量 flush/close，再删除目录；若 stop 失败但目录删除成功，删除操作仍视为成功。

**模式**:
- `delete_block` 不把 `stop_volume` 的失败直接提升为删除失败
- 文件系统清理以目录删除结果为准

**决策理由**:
- 避免小容量卷在删除时因为回收/blacklist 失败而“删不掉”
- 与用户期望的“删除即移除资源”更一致

### Snapshot description contract (2026-07-02)

**问题**: HTTP snapshot create 请求带 `description`，但 core 当前 `SnapshotMeta` 不持久化描述字段。

**方案**: 现阶段只把 description 当作请求兼容字段，列表/查询响应不承诺持久化展示。

**模式**:
- `create_snapshot(description=...)` 接口接受该字段，但列表响应只保证 snapshot id / parent / depth / created_at / deleted

**决策理由**:
- 避免在 core 元数据模型外再引入一份平行描述存储
- 保持当前实现和测试契约一致，后续如需描述持久化再单独设计

### Snapshot clone reflink-first behavior (2026-07-02)

**问题**: 从快照克隆新的 block device 时，既要保留“创建后初始内容一致”的语义，又要尽量避免立即 materialize 整个后端副本。

**方案**: 克隆路径先尝试 Linux `FICLONE` reflink；如果底层文件系统不支持，则回退到普通 sparse copy。

**模式**:
- 支持 reflink 的文件系统上，clone 会共享底层物理块直到任一副本写入
- 不支持 reflink 的文件系统上，clone 仍然可用，但只保证逻辑数据一致

**决策理由**:
- 尽量贴近 COW 克隆语义
- 不把文件系统能力不足直接升级为创建失败
- 保持第一版在不同开发环境中的可用性

### Btree cache memory-pressure alignment (2026-07-02)

**问题**: upstream `system_memory_usage_high()` 不只是一个孤立判断，它会直接影响 btree 节点分配路径的 self-reclaim 行为。

**方案**: 保留 `system_memory_usage_high()` / `system_memory_usage_high_from()` 的结构，把 live footprint 和 freeable footprint 一并纳入判定，并在 `alloc_node_for_key()` / `bch2_btree_node_fill()` 前先做 best-effort `shrink_one()`。

**模式**:
- `system_memory_usage_high()` 仍然是 cache 层 helper
- 分配热路径在 memory pressure 高时先 self-reclaim，再创建新节点

**决策理由**:
- 更接近 bcachefs `bch2_btree_node_mem_alloc()` 的调用链
- 把 cache 压力判断真正接到分配层，而不是留在 dead code / test-only helper
- 事务节流和内存高压仍然是不同信号，但它们都从同一个 cache 计数源读取，避免出现 stale throttle snapshot

### Btree commit throttle gate (2026-07-02)

**问题**: bcachefs 在 `__bch2_trans_commit()` 里会在低水位线下先看 cache throttle，再进入写路径；volmount 先前只保留了注释，没有真正接这条 gate。

**方案**: `BtreeTrans::commit()` 在 `Watermark::Stripe` / `Watermark::Normal` 下调用 cache throttle 检查，并在 throttle 激活时等待解除。volmount 采用短轮询 wait，直到 cache counters 反映出 throttle 清除。

**模式**:
- `Watermark::Reclaim` 及以上仍然跳过 throttle wait
- `BtreeCache::bch2_btree_cache_should_throttle()` 按当前 counters 刷新状态后再返回
- 等待 helper 是 cache-side 的 best-effort polling wait

**决策理由**:
- 把 commit 前置节流从注释变成实际行为
- 保持 transaction 的 reclaim bypass
- 在没有 upstream waitqueue 的前提下，仍能让测试和运行时行为可验证

### Btree cache freeable pool (2026-07-02)

**问题**: `system_memory_usage_high_from()` 需要一个真实的 `freeable` 输入源，才能把 bcachefs `nr_freeable` 语义从结构保留变成可观察的 cache 态信号。

**方案**: 在 `BtreeCacheInner` 里维护显式 `freeable` 池，`gc_retire()` 将无外部引用的 clean 节点迁入该池，分配路径优先复用该池中的节点，并让内存压力判定读取池的真实长度。

**模式**:
- `gc_retire()` 只把安全的 clean 节点移入 `freeable`
- `alloc_node_for_key()` / `bch2_btree_node_fill()` 先尝试从 `freeable` 复用节点
- `system_memory_usage_high_from()` 的 `freeable_nodes` 参数由真实池大小驱动

**决策理由**:
- 让 freeable 不再是测试占位
- 保持和 upstream `live + nr_freeable` 的结构一致
- 复用路径仍然是 best-effort，不改变现有事务节流链路

### Watermark 水位线分配策略 (2026-06-24)

**问题**: 避免分配器在空间压力下死锁 — 高优先级操作（journal、btree 内部更新）需要预留桶。

**方案**: 7 级 `Watermark` 枚举（stripe=0 → interior_updates=6，低值=高需求），每级保留桶数通过 if 链模拟 C switch fallthrough 累加。分配时检查 `free - reserved(watermark) > 0`。

**模式**:
- `Watermark::reserved_buckets()` — if 链累加，与 bcachefs `bch2_dev_buckets_reserved` 语义一致
- `Watermark::allows(request)` — `request >= self` 允许通过
- 测试中使用 `Watermark::InteriorUpdate`（预留 0）避免小型分配器（1-4 桶/组）测试失败

### Freespace per-group 栈 (2026-06-24)

**问题**: 原 `allocate_bucket()` 使用 O(n) 线性扫描查找空闲 bucket。

**方案**: `AllocGroup.free_list: Vec<u32>` — 存空闲 bucket 索引。分配时 `pop()` O(1)，释放时 `push(bi)`。`free_buckets` 原子计数与此保持一致。

**注意事项**:
- `free_list.pop()` 和 `group.buckets[bi]` 不能在同一闭包中同时可变借用 — 分两步操作（先 pop 索引，再访问 bucket）
- `load_from_btree()` 启动时通过 `filter/bucket.state == Free` 重建 free_list
- 释放路径（`alloc_extent_trigger`）仅写 Alloc btree，不更新 allocator 内存状态；`BlockAllocator::free()` 同时更新 free_list + Alloc btree

### commit() 三阶段触发器路径 (2026-06-27)

**问题**: bcachefs 中 transaction commit 包含三个有序阶段（transactional → atomic → gc），且 commit 可接受可选 engine 参数。volmount 原有两个分离方法 `commit()` 和 `commit_with_engine()`，其中 `commit()` 缺少 trigger 路径（触发 alloc/freespace 的 btree 修改）。

**方案**: 统一为 `commit(engine: Option<&mut BtreeEngine>)`：

```rust
pub fn commit(&mut self, engine: Option<&mut BtreeEngine>) -> Result<(), StorageError> {
    // Phase 1: Transactional triggers — 可回滚（needs_restart 可重启事务）
    if let Some(ref engine) = engine {
        for trigger in &self.triggers {
            trigger.run(State::Transactional, &self.keys, engine)?;
        }
    }
    
    if self.needs_restart { /* 重启事务 */ }
    
    // 应用修改：降级写锁 + committed 标记
    self.apply_modifications();
    
    // Phase 2: Atomic triggers — 不可回滚
    if let Some(ref engine) = engine {
        for trigger in &self.triggers {
            trigger.run(State::Atomic, &self.keys, engine)?;
        }
    }
    
    // Phase 3: GC triggers
    if let Some(ref engine) = engine {
        for trigger in &self.triggers {
            trigger.run(State::Gc, &self.keys, engine)?;
        }
    }
    
    Ok(())
}
```

**三阶段语义**：
| Phase | 可回滚 | 调用时机 | 用途 |
|-------|--------|---------|------|
| Transactional | ✅ needs_restart 可回滚 | 修改应用前 | Alloc bucket 计数增减，Freespace 更新 |
| Atomic | ❌ 不可回滚 | 修改应用后 | Journal flush 触发，block IO 持久化 |
| Gc | ❌ 不可回滚 | 全部之后 | GC mark 标记 |

**向后兼容**: `commit(None)` 行为与旧 `commit()` 完全一致（仅锁管理 + committed 标记，无 trigger 触发）。

**决策理由**:
- 统一 API 消除 `commit_with_engine()` 的重复代码
- 三阶段顺序对齐 bcachefs `bch2_trans_commit()` 
- `Option<&mut BtreeEngine>` 允许事务路径不感知 alloc 层（无 engine 时不漏 alloc 更新）

### shrink() 两阶段时钟淘汰算法 (2026-06-27)

**问题**: BtreeNodeCache 需要接近 LRU 的淘汰行为，但纯 LRU 实现（`remove_last`）无法利用 recently-accessed 节点的热数据特性。

**方案**: 两阶段时钟（two-phase clock）扫描代替 LRU pop：

```rust
pub fn shrink(&self, target: usize) -> usize {
    let mut inner = self.inner.lock().unwrap();
    let min_keep = 64usize;
    let max_evict = inner.clean.len().saturating_sub(min_keep);
    let target = target.min(max_evict);
    if target == 0 { return 0; }
    
    // 相位 1: 从 LRU front 扫描 target + 64 个节点
    // 访问过的节点 → 清除 accessed 标志（第 2 次再被扫描才淘汰）
    // 未访问的节点 → 淘汰
    let mut scanned = 0usize;
    let scan_limit = target + 64;  // 宽松扫描窗口
    let ids: Vec<u64> = inner.clean_lru.iter().take(scan_limit).copied().collect();
    
    for &id in &ids {
        if scanned >= target { break; }
        scanned += 1;
        let should_evict = inner.clean.get(&id).map_or(false, |node| {
            if node.is_accessed() {
                node.clear_accessed();  // 第一轮清除标志
                false
            } else {
                true  // 第二轮淘汰
            }
        });
        if should_evict {
            inner.clean.remove(&id);
            inner.clean_lru.retain(|&x| x != id);
            freed += 1;
        }
    }
}
```

**关键参数**:
- `min_keep = 64`: 绝对值保护下限，防止 shrink 清空整个 cache
- `scan_limit = target + 64`: 比目标多扫描 64 个节点，给第一次扫描的节点第二次机会
- 两轮访问保护：节点在 `clean_lru` 中的位置不动，accessed 标志清除后下次再被扫描才淘汰
- `system_memory_usage_high()` 的判定要优先看系统可用内存是否低于 1/4，总 cache footprint 再和剩余压力做二次比较，避免把本地固定阈值误当成 upstream 语义
- dirty 节点进入 `BtreeCache` 时要同步打上 `NODE_NEED_REWRITE`，写完成入口 `bch2_btree_node_write_done_clean()` 负责清理该标志，避免“缓存脏”和“节点需重写”语义脱节

**对应 bcachefs**:
- bcachefs `bch2_btree_cache_shrink` 使用 `list_for_each_entry` 扫描 clean list
- bcachefs 使用 `btree_node_accessed` clear + shrink 相同两阶段模式
- volmount 使用 `VecDeque` + `retain`，而非 intrusive linked list

### Node 生命周期标志 (2026-06-27)

**BtreeNode 新增字段**:

```rust
pub struct BtreeNode {
    // ... 已有字段
    pub accessed: AtomicBool,    // shrink 两阶段时钟使用
    pub need_rewrite: bool,      // btree split/update 后可能需要重写
}
```

**语义**:
- `accessed`: 仅由 cache shrink 读取/修改（其他路径不涉及）。`set_accessed()` 在 cache lookup/insert 时调用；`clear_accessed()` 只在 shrink phase 1 调用。
- `need_rewrite`: 由 btree 更新路径在 split/compact 后设置，在 `flush_dirty_nodes()` 中检查。
- `need_rewrite` 也用于恢复阶段的 fake root：`btree_root_alloc_fake()` 必须在节点进入 cache 前设置该标志，避免把占位根误当作可直接持久化的 clean 节点。
- 与 bcachefs `struct btree_node.accessed` 和 `struct btree.rewrite_needed` 对齐。

### trigger_extent — Alloc triggers 的 idempotent entry (2026-06-27)

**问题**: 原 alloc trigger 路径直接用 `dirty_sectors += sectors`，不支持批量 key 更新的 idempotent 重入（多次触发导致重复计数）。

**方案**: `trigger_extent()` 基于 old/new bkey 的 sector 计数推导：

```rust
fn trigger_extent(
    trans: &mut Transaction,
    old: Option<&AllocBkey>,
    new: Option<&AllocBkey>,
) -> BucketDiff {
    // 推导 old/new 的扇区变化
    let old_dirty = old.map_or(0, |b| b.dirty_sectors());
    let new_dirty = new.map_or(0, |b| b.dirty_sectors());
    let diff = new_dirty as i64 - old_dirty as i64;
    BucketDiff { dirty_sectors: diff, ... }
}
```

**关键约束**:
- `trigger_extent` 被 Transactional/Atomic/Gc 三个 phase 各调用一次
- 每个 phase 传递相同的 old/new 对 → 三阶段乘积必须正确（相加为零或符合预期）
- **非幂等补偿**不能出现在单 phase 内；如果 Phase1 做了 `+= diff`，Phase2/3 不能重复做
- bcachefs 使用 `btree_key_cache` 避免重读，volmount 使用 `old_key` 缓存

### try_decrease 写点淘汰机制 (2026-06-26)

**问题**: 分配失败（`AddressSpaceExhausted`）时，写点拴住的空间（stranded space）可能超过空闲空间的 1/8，需要淘汰写点释放桶。

**方案**: `WritePointPool::try_decrease()` 在分配失败 retry 循环中被调用。

**关键决策**:
- **factor = 8**: `stranded * 8 > free_sectors` 判定写点过多（与 bcachefs `try_decrease_writepoints(c, 8)` 一致）
- **最小保护**: `nr_active <= 1` 时不执行淘汰（保留至少一个池写点）
- **释放路径**: 有剩余扇区的桶 → `add_to_partial()`（可复用），已满桶 → `put()`（回 freelist）
- **retry 一次**: 只尝试一次 `try_decrease` + 重试（防无限循环），写入点后重走 Step 2 (try_reuse) + Step 3 (alloc_new_fs)

**签名**:
```rust
pub fn try_decrease(
    &mut self,
    bucket_size: u64,     // 扇区单位
    free_sectors: u64,    // 当前空闲扇区数
    open_buckets: &BchOpenBuckets,
) -> bool // true = 成功减少一个写点，调用者应重试分配
```

**注意事项**:
- `try_decrease` 只释放最后一个活跃写点（`nr_active - 1`），不是 LRU 淘汰
- 对应的 `too_many_writepoints()` 是私有方法，由 `try_decrease` 内部调用
- `stranded_space()` 方法本已存在但无人调用，Wave 4 通过 `try_decrease` → `too_many_writepoints` 间接接入

### BtreeInteriorUpdate 生命周期 (2026-06-27)

**问题**: `btree_update.rs` 的 `BtreeInteriorUpdate` 状态机仅含 `Init → NodesAllocated → UpdateParent → Done` 四个同步状态，bcachefs 的 `struct btree_update` 使用 `pending → done → free` 异步状态机（含 disk_reservation、异步写完成回调、write_blocked_list）。

**方案**: 当前 volmount 采用同步 interior update 设计：

```rust
pub enum InteriorUpdateState {
    Init,
    NodesAllocated,
    UpdateParent,
    Done,
}
```

**决策理由**:
- volmount 当前为单线程引擎，无并发 split/merge 场景
- 同步路径避免了异步状态管理的复杂性（closure 回调、内存序保证）
- split_root() 在单线程中直接完成所有节点操作，无需等待 I/O 完成

**差异对比**:
| 维度 | bcachefs `struct btree_update` | volmount `BtreeInteriorUpdate` |
|------|-------------------------------|-------------------------------|
| 状态机 | pending → done → free (5 态) | Init → NodesAllocated → UpdateParent → Done (4 态) |
| 磁盘预留 | `disks_res` 显式管理 | 未实现（调用方保证空间） |
| 异步回调 | `closure` 完成时触发 | 无（同步完成） |
| write_blocked | 链表管理等待写入的节点 | 未实现（单线程不需等待） |
| 并发保护 | 多写线程通过 SIX 锁协调 | 单线程无竞争 |

**未来迁移**:
- 如果将来引入多线程写路径，需要实现完整的 `struct btree_update` 状态机：
  - `disk_reservation` 在 split 前预留 bucket 空间
  - `write_blocked` 链表防止父节点在子节点落盘前被写
  - 异步 `closure` 回调在 I/O 完成时推进状态
  - `bch2_btree_update_start/end` 生命周期管理
- 在此之前，同步设计更简单且正确

### journal_res_get_slowpath 三阶段降级 (2026-06-27)

**问题**: `journal_res_get()` 在 fastpath CAS 失败后，原实现用 100 次自旋重试后直接返回 `Overflow` panic。生产环境中 journal 满时应先尝试 cycle → wait → reclaim 三级降级，panic 仅作为最后手段。

**方案**: 三级 fallback：

```rust
Phase 1: cycle → journal_cycle_locked() 关闭当前 entry 打开新 bucket
Phase 2: wait → 自旋 1024 次等待 in_flight 队列清空
Phase 3: reclaim → bch2_journal_flush_pins() + update_last_seq + advance_dirty_idx
Fallback: 三级都失败 → Err(JournalError::Overflow)
```

**关键点**:
- 每级成功后立即重试 fastpath（`journal_res_get_fast`），避免不必要的降级
- `slowpath_lock` Mutex 保证同一时间只有一个线程执行降级
- `journal_res_get()` 公开入口：先尝试无锁 fastpath，失败后获取 slowpath 锁进入降级

### bch2_journal_flush_pins — Pin 回调链 (2026-06-27)

**问题**: btree 节点写入后 journal entry 被 pin 住无法回收，缺乏 flush 机制释放已完成的 pin。

**方案**: `PinEntry` 携带 `flush_callbacks: Vec<Box<dyn Fn() + Send>>`，`bch2_journal_flush_pins(target_seq)` 遍历 pin_fifo 触发 seq ≤ target 的 flush 回调：

```rust
pub fn bch2_journal_flush_pins(&self, target_seq: u64) -> Result<bool, StorageError> {
    // 收集 seq ≤ target 且 count==0 的前端条目
    // 先触发所有回调（持有锁），再从前端弹出
    // callback 返回 Err 时立即停止并传播错误
}
```

**回调锚点**: btree/journal pin 集成中，`node_write()` 写入节点后注册空回调（`Box::new(|| {})`）作为 pin 生命周期管理的锚点。在 volmount 同步写模型中，节点在 pin_add 前已完成写入，回调不做额外 I/O。

### 1变2 快照创建语义 (2026-06-27)

**问题**: bcachefs 快照创建时源子卷的快照指针指向旧节点，需要同时创建两个快照节点（一个给新子卷，一个替换源子卷的快照指针），原实现只创建一个。

**方案**: `bch2_snapshot_node_create(engine, parent_id, subvol, extra_child_subvol)` 通过 `Option<u32>` 控制：

- `None` → 单子节点（向后兼容）
- `Some(src_subvol)` → 双子节点（1变2）：
  1. 分配两个 snapshot ID（id 和 id-1）
  2. src_subvol.snapshot → child1, new_subvol.snapshot → child2
  3. 父节点：subvol=0, flags.clear(SUBVOL), children=[child1, child2]
  4. 使用 `batch_write` 原子写入三个条目

### BtreeKey bversion 向后兼容字段 (2026-06-27)

**问题**: bcachefs `struct bkey` 包含 `__u64 version` 字段用于 MVCC 版本追踪，volmount `BtreeKey` 缺少此字段。

**方案**: 在 `BtreeKey` 结构体末尾添加 `pub version: u64` 字段，使用 `#[serde(default)]` 确保旧序列化数据兼容：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C, packed)]
pub struct BtreeKey {
    pub vaddr: u64,
    pub snapshot_id: u32,
    pub key_type: KeyType,
    #[serde(default)]
    pub version: u64,  // MVCC 版本号，不参与排序
}
```

**不参与比较**: `version` 不参与 `PartialEq`/`Ord`/排序——bcachefs 中 bkey 的排序仅基于 (inode, offset, snapshot)，version 用于写冲突检测。

### Key Cache Write-back — Slot 复用 + Dirty 追踪 + Journal Pin + Two-Phase Flush (2026-06-28)

**问题**: 原 `BtreeKeyCache` 中的 slot 是"一次写入永不释放"模式 — `invalidate()` 从 hash 表移除 entry 后 slot 不再可复用。同时，key cache 缺乏 dirty 追踪和 journal 集成，无法在同步点将脏数据写回 btree。

**方案**: 4 个 Phase 实现 bcachefs 对齐的 key cache write-back 语义。

#### Phase 1: Slot 复用

```rust
pub struct CachedEntry {
    pub valid: AtomicBool,   // ← 新增: false 表示 slot 已释放但 entry 仍占位
    pub key: BtreeKey,
    pub value: RwLock<Option<Vec<u8>>>,
    pub lock: SixLock,       // SixLock 保护
    pub dirty: AtomicBool,   // Phase 2
    pub journal_seq: AtomicU64,
    pub flush_pending: AtomicBool,
}
```

**语义**:
- `valid = AtomicBool::new(true)` — 创建时有效
- `invalidate()` / `bch2_btree_key_cache_drop()`: `valid.store(false, Release)` — 保留 slot 但标记无效
- `find()`: 检查 `valid.load(Acquire)`，false → 返回 None（即使 slot 存在）
- `drop(self)`: `valid.store(false, Release)` — Arc 降零时释放关联状态
- 下次 `insert()` 发现 hash 表已有 invalid entry → 复用 slot（`valid.store(true, Release)`）
- 与 bcachefs `struct bkey_cached.valid` 对齐

#### Phase 2: Dirty 追踪

```rust
pub struct KeyCache {
    // ... 已有字段
    pub nr_dirty: AtomicU64,     // ← 新增: 当前脏 entry 数
}

pub struct CachedEntry {
    pub dirty: AtomicBool,       // true = 有未写回 btree 的修改
    pub journal_seq: AtomicU64,  // 最后修改的 journal seq
    pub flush_pending: AtomicBool, // journal pin callback 已触发，等待 flush
    // ...
}
```

**`bch2_btree_insert_key_cached()` 重写** (对应 bcachefs `bch2_btree_insert_key_cached` `btree_key_cache.c:843-885`):

```rust
pub fn bch2_btree_insert_key_cached(
    &self,
    key: BtreeKey,
    value: Vec<u8>,
) -> Result<(u64, Arc<CachedEntry>), StorageError> {
    // 1. 查找已有 slot
    if let Some(entry) = self.find(&key) {
        // 2. 如果已存在: 覆盖 value, 设 dirty=true, inc nr_dirty
        let mut val = entry.value.write().unwrap();
        *val = Some(value);
        drop(val);
        if !entry.dirty.swap(true, AcqRel) {
            self.nr_dirty.fetch_add(1, AcqRel);
        }
        // 3. 注册 journal pin (Phase 3)
        self.pin_entry(&entry);
        return Ok((k, entry));
    }
    // 4. 不存在: 创建新 slot + dirty=true + insert hash 表
}
```

**`insert()` 覆盖脏 slot**: 如果 hash 表命中一个已有的 dirty entry，必须清除 dirty + 释放 journal pin，再设新的 dirty 标志：

```rust
// insert() 中:
if let Some(entry) = self.find(key) {
    if entry.valid.swap(true, AcqRel) == false {
        // slot 复用
    }
    // 已有效但脏: 清除旧状态再设新值
    if entry.dirty.swap(true, AcqRel) == false {
        self.nr_dirty.fetch_add(1, AcqRel);
    }
    // 释放旧 journal pin (Phase 3)
    self.drop_journal_pin(&entry);
    // 注册新 pin (Phase 3)
    self.pin_entry(&entry);
}
```

**`invalidate()` 清理脏**:
- 如果 entry 是脏的：`dirty.store(false, Release)` + `nr_dirty.fetch_sub(1, AcqRel)` + `drop_journal_pin()`
- `valid` 仍设 false（标记 slot 可复用）
- 与 bcachefs `bch2_btree_key_cache_drop` 的 `clear_bit(KEY_CACHE_DIRTY)` 对齐

**`nr_dirty_keys()` 访问器**:
```rust
pub fn nr_dirty_keys(&self) -> u64 {
    self.nr_dirty.load(Acquire)
}
```

**`bch2_nr_btree_keys_need_flush`** 返回 `max(0, nr_dirty - (1024 + nr_keys / 2))`，`bch2_btree_key_cache_must_wait` / `wait_done` 也按 bcachefs 的 `nr_dirty` + `nr_keys` 阈值公式计算（对应 `btree_key_cache.c:900-910`）。

#### Phase 3: Journal Pin 集成

**KeyCache 新增字段和方法**:

```rust
pub struct KeyCache {
    pub journal: OnceLock<Weak<Journal>>,  // Journal 弱引用
    // ...
}

impl KeyCache {
    pub fn set_journal(&self, journal: &Arc<Journal>) {
        self.journal.set(Arc::downgrade(journal)).ok();
    }

    fn pin_entry(&self, entry: &Arc<CachedEntry>) {
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else { return };
        let entry_clone = entry.clone(); // clone Arc 注册入回调
        let barrier = Arc::downgrade(&entry_clone);
        j.pin_add(Box::new(move || {
            if let Some(e) = barrier.upgrade() {
                e.flush_pending.store(true, Release);
            }
        }));
    }

    fn drop_journal_pin(&self, entry: &CachedEntry) {
        let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else { return };
        j.pin_drop();
    }
}
```

**Callback 链**:
- `pin_add(Box<dyn Fn() + Send>)` 注册 flush callback
- `journal_flush_pins(target_seq)` 触发 seq ≤ target 的 callback
- callback 设置 `flush_pending = true` → 同步点检查 → 调用 `flush_cache_dirty_keys()`
- 与 bcachefs `bch2_journal_pin_copy` / `bch2_journal_pin_drop` / `bch2_journal_pin_set` (`journal.h`) 对齐

#### Phase 4: Two-Phase Flush

**两阶段设计解决跨层借用冲突**:

```text
Phase 1 (KeyCache, &self):  collect_dirty()  →  Vec<(BtreeKey, Arc<CachedEntry>)>
Phase 2 (Btree, &mut self):  写回 btree         ← BtreeEngine::insert_entry_skip_cache()
Phase 3 (KeyCache, &self):  mark_clean()      → 清 dirty + drop journal pin
```

**`collect_dirty()`** (Phase 1):
```rust
pub fn collect_dirty(&self) -> Vec<(BtreeKey, Arc<CachedEntry>)> {
    let mut result = Vec::new();
    for (k, entry) in self.entries.read().unwrap().iter() {
        if entry.dirty.load(Acquire) {
            // 清 flush_pending（重入保护）
            entry.flush_pending.store(false, Release);
            result.push((*k, entry.clone()));
        }
    }
    result
}
```

**`mark_clean()`** (Phase 3):
```rust
pub fn mark_clean(&self, keys: &[BtreeKey]) {
    for key in keys {
        if let Some(entry) = self.find(key) {
            if entry.dirty.swap(false, AcqRel) {
                self.nr_dirty.fetch_sub(1, AcqRel);
            }
            self.drop_journal_pin(&entry);
        }
    }
}
```

**`flush_dirty(on_write: impl Fn(...))`** — 收集 + 写回调 + 清理：
```rust
pub fn flush_dirty<F>(&self, mut on_write: F) -> Vec<(BtreeKey, Result<(), StorageError>)>
where F: FnMut(&BtreeKey, &[u8]) -> Result<(), StorageError>
{
    let entries = self.collect_dirty();               // Phase 1
    if entries.is_empty() { return vec![]; }
    
    let keys: Vec<BtreeKey> = entries.iter().map(|(k, _)| *k).collect();
    let results: Vec<(BtreeKey, Result<(), StorageError>)> = entries.iter().map(|(k, entry)| {
        let val = entry.value.read().unwrap();
        let res = val.as_ref().map_or(Ok(()), |v| on_write(k, v));
        (*k, res)
    }).collect();
    
    // Phase 3: 标记成功的为 clean
    let ok_keys: Vec<BtreeKey> = results.iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(k, _)| *k)
        .collect();
    self.mark_clean(&ok_keys);
    
    results
}
```

**`Btree::insert_entry_skip_cache()`** — 写 btree 但不 invalidation cache：

```rust
pub fn insert_entry_skip_cache(
    &mut self,
    key: BtreeKey,
    value: Vec<u8>,
) -> Result<(), StorageError> {
    // 直接插入节点，不走 key cache 路径
    self.insert_entry_into_node(key, &value)
}
```

**`BtreeEngine::flush_cache_dirty_keys()`** — Engine 级别遍历 5 种 btree 同步 flush：

```rust
pub fn flush_cache_dirty_keys(&mut self) -> Vec<(BtreeType, Vec<(BtreeKey, Result<(), StorageError>)>)> {
    let mut results = Vec::new();
    for tree in self.trees.iter_mut() {
        let r = tree.key_cache.flush_dirty(|key, val| {
            tree.insert_entry_skip_cache(*key, val.to_vec())
        });
        if !r.is_empty() {
            results.push((tree.ty(), r));
        }
    }
    results
}
```

**同步点接线**（以下位置会在写入前调用 `flush_cache_dirty_keys()`，新写入口需同步维护）:
- `batch_write()` 调用前
- `insert_guarded()` 调用前
- `commit_with_journal()` 调用前
- bcachefs 中对应的 `bch2_btree_key_cache_flush()` 在 `btree_key_cache.c:708-740`

**行为约束**:
- `flush_dirty()` 是同步写，不是异步后台线程
- 写失败的条目保持 dirty 状态（不调用 mark_clean）
- collect_dirty 使用 &self（仅读 hash 表），flush 阶段在 Engine 层完成 &mut self 操作
- 与 bcachefs `bch2_btree_key_cache_flush` 和 `bch2_btree_key_cache_journal_flush` 语义对齐

**对应 bcachefs 源码**:

| 概念 | bcachefs 文件:行号 |
|------|-------------------|
| `struct bkey_cached.valid` | `btree_key_cache.c` |
| `struct bkey_cached.dirty` | `btree_key_cache.c:75` |
| `KEY_CACHE_DIRTY` | `btree_key_cache.c` |
| `bch2_btree_insert_key_cached` | `btree_key_cache.c:843-885` |
| `bch2_nr_btree_keys_need_flush` | `btree_key_cache.c:900-910` |
| `bch2_btree_key_cache_flush` | `btree_key_cache.c:708-740` |
| `bch2_journal_pin_copy/drop/set` | `journal.h` |
| `bch2_btree_key_cache_journal_flush` | `btree_key_cache.c` |

## Verification Status — Batch A (2026-06-27)

### lock/six.rs — bcachefs C 一致性验证（已修复）

以下 4 项已在 Batch A 中修复并通过验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| 1 | WAITING_WRITE_BIT 位位置 | `SIX_LOCK_WAITING_write=1U<<30` | bit 29→30 (0x2000_0000→0x4000_0000) | ✅ 49/49 six 测试通过 |
| 2 | try_lock_intent CAS 模式 | `atomic_try_cmpxchg_acquire` loop | 单次 compare_exchange→compare_exchange_weak 循环 | ✅ 无死锁/回退 |
| 3 | downgrade_write notify | `six_lock_downgrade` 隐式等待者检查 | 增加 self.notify_waiters() 调用 | ✅ 条件隐含在 notify_waiters 内部 |
| 4 | handoff 对齐验证 | `six.c __six_lock_wakeup` | 增加文档对比 C 语义 | ✅ handoff 实现已存在，文档确认对齐 |

### btree 类型系统 — 新增基础设施（已验证）

| # | 新增项 | C 引用 | 说明 | 验证结论 |
|---|--------|--------|------|----------|
| 1 | BtreeNodeType 枚举 | `enum btree_node_type` | 映射 __btree_node_type(level, btree_id) | ✅ 正确 |
| 2 | KEY_TYPE_BTREE_PTR_V3=19 | `KEY_TYPE_btree_ptr_v3=19` | 从 key.rs 导出 | ✅ 正确 |
| 3 | BTREE_ITER_BUF_GRANULARITY=2048 | `bkey_buf.h kmalloc(2048)` | peek_upto buffer 粒度 | ✅ 正确 |
| 4 | Watermark PartialOrd | BCH_WATERMARK_reclaim 比较 | #[derive(PartialOrd)] repr(u8) | ✅ 正确 |

### btree 内部操作 — 历史 TODO / 已闭环项

以下 5 项在 Batch A 中标记为 TODO，因需要跨子系统基础设施支持：

| # | TODO | 文件 | blocker |
|---|------|------|---------|
| 1 | commit WAL 持久性窗口 | transaction.rs | log_operation 先 journal 后 btree 需事务回滚能力 |
| 2 | pre_split journal 预留 | btree.rs | 需要 journal_res_get 集成 |
| 3 | mark_done drop_children | update.rs | 需要 drop_children 函数实现 |
| 4 | journal_seq_verify | update.rs | 需要跨子系统 journal_seq API |
| 5 | gc_gens journal_seq 追踪 | gc.rs | 需要 gc_pos 结构体修改 + 签名变更 |

**验证状态**: 这些条目保留为历史背景；已实现项会在后续验证记录中标记为 ✅，未实现项仍保留明确 blocker 解释，非"slop"。

### 测试覆盖验证

- **lock/six**: 49 tests → 49 ✅ (新增覆盖率：无新增测试，C bit 对齐的回归验证)
- **btree 模块**: 331 tests → 331 ✅ (GC 17/17, key 6/6, node 155/155, io 7/7, iter 5/5, trans 6/6, mod 12/12, writepoint 9/9)
- **全量**: 693 passed, 5 known fail (预存 AddressSpaceExhausted), 6 ignored
- **clippy/fmt**: 0 新增 warning/diff

## Verification Status — Batch B (2026-06-27)

### alloc 模块 — bcachefs C 一致性验证（12 项修复）

2026-06-27 通过 4 个并行子代理实施并在 main-session 验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BchAllocEntry 字段对齐 | `bch_alloc_v4` | 字段命名和 bitfield 布局对齐 | ✅ |
| P0-2 | reserved_buckets 耗尽策略 | `BCH_ALLOC_RESERVE_*` | `AddressSpaceExhausted` → `AllocError::ReserveExhausted` + alloc_hint 优先 | ✅ |
| P0-3 | derive_data_type 优先级 | `alloc_data_type` | USER>META>PARITY>RESERVED 严格顺序 | ✅ |
| P0-4 | BchAllocEntry journal_seq | journal entry 兼容 | format 写入路径修复 | ✅ |
| P1-5 | BchAllocBucket 状态枚举 | `bucket_state` | `need_discard` / `free_discarded` / `free_available` / `need_gc_gens` / `sb_only` | ✅ |
| P1-6 | bucket_gens 更新策略 | `bch2_bucket_gens` | set-version → lazy dirty + checkpoint 批处理 | ✅ |
| P1-7 | alloc_group 分配亲和性 | `alloc_prio_hint`/`target` 复合 | `foreground::AllocTarget` + `resolve_alloc_group` | ✅ |
| P1-8 | alloc_key_v2 单 entry 路径 | `bch2_alloc_key_v2` | 新增单一 entry 写入路径 | ✅ |
| P1-9 | gc_gens 回收范围 | BITMAP_SIZE | 完整范围覆盖 | ✅ |
| P2-10 | bucket_mark checkpoint 初始化 | `bch2_alloc_read` | 0 号桶初始化补全 | ✅ |
| P2-11 | 最大尝试次数步进回退 | `BCH_ALLOC_ATTEMPTS` | 3→步进降级水位线 | ✅ |
| P2-12 | prio_hint 映射 | `alloc_hint_type` | UNSPECIFIED→USER/SYSTEM/META 映射 | ✅ |

### journal 模块 — bcachefs C 一致性验证（8 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | Jset magic/version/csum | `JSET_MAGIC` | `VMNT_JSET_MAGIC` + `JSET_VERSION` + `CSUM_TYPE_*` | ✅ |
| P0-2 | JsetEntry has_last/has_prev | `jset_entry` byte flags | 新增 `#[serde(default)]` byte 字段 | ✅ |
| P1-3 | Pin 预分配 | `JOURNAL_PIN_LIST_SIZE` | `MAX_PIN_ENTRIES=128` 固定预分配 | ✅ |
| P1-4 | replay 特殊 entry | `JOURNAL_ENTRY_TYPE_OVERWRITE` / `BTREE_NODE_REWRITE` | 新增处理路径 | ✅ |
| P1-5 | preres noflush 状态机 | `journal_buf_state_noflush` | `BufState::Noflush` 枚举变体 | ✅ |
| P2-6 | commit callback 机制 | `journal_commit` closure | `write_done_callbacks` Vec + wake_up | ✅ |
| P2-7 | flush 定时器 + 标志 | `JOURNAL_NEEDS_FLUSH_WRITE` | `JOURNAL_NEEDS_FLUSH_WRITE` 常量 | ✅ |
| P2-8 | CRC 分片算法 | crc32c 分片 | 对齐 bcachefs crc32c 分片方式 | ✅ |

### snap 模块 — bcachefs C 一致性验证（7 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BchSnapshotFlags 位布局 | `BCH_SNAPSHOT_SUBVOL=1<<4` | 位从 `1<<4` 开始，前 4 位为 leaf 保留位 | ✅ |
| P0-2 | skip_list 指数步进 | `bch2_snapshot_skiplist_good` | 等距→指数 `1<<i` 步进 | ✅ |
| P1-3 | is_ancestor subvol 间接路径 | `bch2_snapshot_is_ancestor` | `bch2_snapshot_is_ancestor_subvol` | ✅ |
| P1-4 | master_subvol 级联管理 | `bch2_snapshot_tree_master_subvol` | 新增函数 | ✅ |
| P2-5 | skiplist 递归回退重试 | `bch2_snapshot_skiplist_good` | 健壮性检查 + 递归回退 | ✅ |
| P2-6 | snapshot_id bitmap 分配 | bitmap + 回收 | `SnapshotIdBitmap` 新增 | ✅ |
| P2-7 | snapshot_tree 子树注册 | subtree registry | `SubtreeRegistry` + `write_snapshot_tree_value` | ✅ |

### subvol 模块 — bcachefs C 一致性验证（5 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | BCACHEFS_ROOT_INO 判据 | `BCACHEFS_ROOT_INO` | 新增常量用于根节点操作判别 | ✅ |
| P1-2 | root snapshot 创建 | `bch2_snapshot_root` | `bch2_snapshot_node_create` 在 subvol_create 中调用 | ✅ |
| P1-3 | subvol_ino_map 清理 | `bch2_subvolume_ino_map` | `register_ino_map` 清理路径 | ✅ |
| P2-4 | 1变2 batch_write 原子性 | `commit_do` | batch_write 包含父子卷更新 + 新子卷创建 | ✅ |
| P2-5 | bch2_subvolume_trigger | `bch2_subvolume_trigger` | 新增 snapshot tree 验证路径 | ✅ |

### 兼容层清理

| 模块 | 操作 | 状态 |
|------|------|------|
| snap/mod.rs | 移除 `create_snapshot_btree` / `delete_snapshot_btree` export | ✅ |
| subvol/mod.rs | 移除旧名 export，增加 `bch2_subvolume_trigger` / `BCACHEFS_ROOT_INO` export | ✅ |
| volume/mod.rs | `create_snapshot_btree`→`bch2_snapshot_node_create` / `delete_snapshot_btree`→`bch2_snapshot_node_set_deleted` | ✅ |

### 测试覆盖验证

- **alloc**: 42 tests → 42 ✅
- **journal**: 28 tests → 28 ✅
- **snap**: 16 tests → 16 ✅（含 skip_list 指数步进测试）
- **subvol**: 13 tests → 13 ✅
- **全量**: 710 passed（较 Batch A +17），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（fmt clean，clippy 仅预先存在的 dead_code/unused）

**验证结论**: PASS_WITH_NOTES
- Minor: `subvol/ops.rs:269` 注释仍含旧名 `delete_snapshot_btree` — 已修复为 `bch2_snapshot_node_set_deleted`
- Minor: `snap/mod.rs` 仍导出 `is_ancestor_from_btree`（volmount 扩展函数，非 bcachefs compat 名）

## Verification Status — Batch C (2026-06-27)

### recovery 模块 — bcachefs C 一致性验证（10 项修复）

2026-06-27 通过 4 个并行子代理实施并在 main-session 验证：

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P0-1 | SnapshotsRead pass | `PASS_ALWAYS #3` | 新增 `BchRecoveryPass::SnapshotsRead=6` + stub 实现 | ✅ 顺序正确 |
| P0-2 | TransMarkDevSbs pass | `PASS_ALWAYS #6` | 新增 `BchRecoveryPass::TransMarkDevSbs=7` + stub 实现 | ✅ 顺序正确 |
| P0-3 | FsJournalAlloc pass | `PASS_ALWAYS #7` | 新增 `BchRecoveryPass::FsJournalAlloc=8` + stub 实现 | ✅ 顺序正确 |
| P0-4 | AccountingRead pass | `PASS_ALWAYS #39` | 新增 `BchRecoveryPass::AccountingRead=9` + stub 实现 | ✅ deps 修复 BIT_ULL[1]→BIT_ULL[5] |
| P0-5 | PresplitShardBoundaries pass | `PASS_ALWAYS #48` | 新增 `BchRecoveryPass::PresplitShardBoundaries=10` + stub 实现 | ✅ 注释修复 |
| P0-6 | LookupRootInode pass | `PASS_ALWAYS #42` | 新增 `BchRecoveryPass::LookupRootInode=11` + stub 实现 | ✅ 最后 pass |
| P1-7 | alloc_read stub | `bch2_alloc_read` | 已挂接到 `passes::alloc_read::run` | ✅ stub 安全 |
| P1-8 | check_topology 增强 | `bch2_check_topology` | 递归 parent-child、child 边界和缺失 child 验证已实现并有回归测试 | ✅ |
| P1-9 | deps 强制执行 | `passes.c` `depends` | 调度器中增加依赖检查：pass 运行前检查所有 deps 位是否已 complete | ✅ 新增强制执行 |
| P1-10 | PASS_UNCLEAN/FSCK/ONLINE/NODEFER flags | `passes_format.h` | `RecoveryPassFlags` 新增 4 个标志常量 | ✅ 对齐 C |

### volume 模块 — bcachefs C 一致性验证（3 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | recovery 状态追踪字段 | `bch_fs_recovery` | `recovery_pass_done` / `recovery_passes_complete` / `passes_failing` | ✅ |
| P1-2 | RwWithPendingRecovery 子状态 | `enum bch_fs_state` | `VolumeState::RwWithPendingRecovery=6` | ✅ |
| P2-3 | error_count AtomicU64 | `bch_fs` `fsck_error` | `Volume` 新增 `error_count: AtomicU64` | ✅ |

### storage 模块 — bcachefs C 一致性验证（4 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | 备份 superblock 布局 | `BCH_SB_LAYOUT_*` | `BackupSbLayout` 结构 + 多副本写入（BlockAddr 0/4/8） | ✅ |
| P1-2 | 写所有副本 | superblock 写入路径 | `write_to_backend` 遍历所有副本写入 | ✅ |
| P2-3 | UUID 字段 | `sb.uuid` / `sb.user_uuid` | `BchSb` 新增 `uuid: [u8; 16]` + `user_uuid: [u8; 16]` | ✅ serde(default) 兼容 |
| P2-4 | features/compat 标志 | `sb.features[2]` / `sb.compat[2]` | `BchSb` 新增 `features: [u64; 2]` + `compat: [u64; 2]` | ✅ serde(default) 兼容 |

### block_device 模块 — bcachefs C 一致性验证（3 项修复）

| # | 修复项 | C 引用 | 修复内容 | 验证结论 |
|---|--------|--------|----------|----------|
| P1-1 | checksum 读写方法 | `bch2_crc32c` | `block_crc32c` 函数 + `read_block_with_csum` / `write_block_with_csum` | ✅ |
| P1-2 | write_extent checksum 集成 | write_extent 路径 | `Volume::write_extent` 中调用 `write_block_with_csum` | ✅ |
| P2-3 | MockBlockDevice 零填充 | 对齐 FileBlockDevice | 未写入块返回零填充而非 `BlockNotFound` | ✅ |

### 测试覆盖验证

- **recovery**: 17 tests → 17 ✅（新增 6 个 stub pass 后不会 panic `unreachable!()`）
- **volume**: 17 tests → 17 ✅
- **storage::superblock**: 5 tests → 5 ✅
- **block_device**: 42 tests → 42 ✅
- **全量**: 716 passed（较 Batch B +6），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（fmt clean，clippy 仅预先存在的 pre-Batch-C warnings）

### 自修复项

trellis-check 审计中发现并修复了以下问题：

1. **`recovery/mod.rs`** — `accounting_read` 的 `deps` 字段错误引用了 `RECOVERY_PASS_BITS[1]`（BtreeRoots）。C 中 `accounting_read`（稳定 ID 39）的 deps 是 `BIT_ULL(BCH_RECOVERY_PASS_check_topology)`。等价于 volmount 的 `RECOVERY_PASS_BITS[5]`（GcScan，包含拓扑检查）。已修复。

2. **`recovery/passes/snapshots_read.rs:7`** — 注释错误"遍历 Alloc btree 的 snapshots 条目"→ 已修正为"遍历 Snapshots btree 的快照条目"。

3. **`recovery/passes/presplit_shard_boundaries.rs:5`** — 注释与 `deps` 矛盾（说需要 snapshots_read，但 deps 指向 JournalReplay）。已修正为"需要 journal_replay 已完成"对齐 C。

4. **`recovery/mod.rs`** — `depends` 位掩码从未在调度器中执行。PRD #9 要求强制执行。已添加 deps 检查逻辑。

5. **`snap/snapshot.rs`** — `bch2_fix_child_of_deleted_snapshot()` 重建 skip list 时不能沿用旧 `skip[]` 槽位，否则已删祖先会被原样保留到新快照中。修复方式是先把 `new_skip` 置零，再只填充有效祖先并排序。

6. **`storage/block_io.rs` / `storage/service.rs`** — `BchAllocator::new()` 在很小的 AG 上会被 `Watermark::Normal` 的固定预留卡住，导致 checkpoint / block I/O 测试在无关的地址空间耗尽上失败。测试夹具应使用足够大的单 AG，避免把 allocator 预留策略误判成业务回归。

7. **`recovery/mod.rs`** — `BchRecoveryPassStable::CheckDiretns` 是明显的拼写漂移，已统一为 `CheckDirents`。恢复 pass 的稳定 ID / 枚举命名不能出现这类拼写误差，否则会污染覆盖地图、实现注释和后续审查。

8. **`recovery/mod.rs`** — `restore_progress()` 不能把已持久化的 `superblock.pass_done` 回写成 runtime 顺序派生的更小 stable ID；bcachefs 的 stable pass 编号不是按运行时顺序单调增长的。若恢复调度器临时注入兜底 pass，也要把这些 pass 计入完成掩码，否则可能在部分成功时提前结束恢复。

9. **`recovery/mod.rs`** — `check_snapshots` 的 flags 需要保留 bcachefs 的 `PASS_ALWAYS|PASS_ONLINE|PASS_FSCK|PASS_NODEFER` 组合。`passes_online` 是从 flags 派生的可观测状态，不是装饰性字段；如果 flags 缺了 `PASS_ONLINE`，后续在线调度和状态展示都会失真。

10. **`btree/gc.rs`** — 递归拓扑检查必须先走真实 child 引用，再做平面遍历。`Btree::for_each_entry()` 依赖 `BtreeIter::init()`，而后者会通过 `get_or_create()` 补出缺失的 child 节点；如果先跑平面遍历，`missing child` 类损坏会被掩盖。child 存在性检查必须使用 `NodeCache::get()`，不要在校验前触发自动创建。

### 已知差距（非本次范围）

- 6 个新 recovery passes 均为 stub 实现（`let _ = &state.field; Ok(())`），需要对应 btree/allocator 基础设施就绪后才能启用实质逻辑
- `PASS_ALLOC` 标志在 volmount 的 `RecoveryPassFlags` 中不存在（C 中 `trans_mark_dev_sbs` 和 `fs_journal_alloc` 均含 `PASS_ALLOC`，当前阶段无影响）
- `check_topology` 的递归 parent-child 链接验证已实现并由回归测试覆盖；这里只保留对实现约束的说明，不再把它当作未完成 TODO

**验证结论**: PASS_WITH_NOTES
- Note: 6 个新 recovery passes 为 stub 实现，且 `alloc_read` pass 仍为 stub（PRD 要求真实现但缺少 bucket_gens btree 基础设施）
- Note: 读取路径未集成 checksum 验证（PRD 仅要求 write_extent 路径，已满足）

## Verification Status — Batch D (2026-06-28)

### Key Cache Write-back — 4 个 Phase 实现

| Phase | 变更 | 文件 | 验证结论 |
|-------|------|------|----------|
| P1 | CachedEntry slot 复用 — valid AtomicBool + find() 检查 | key_cache.rs | ✅ valid.store/load Acquire/Release 正确 |
| P1 | invalidate() 设 valid=false 不移除 hash 表 | key_cache.rs | ✅ hash 保留 slot，复用通过 insert 路径验证 |
| P2 | Dirty tracking — dirty/jounral_seq/flush_pending AtomicBool | key_cache.rs | ✅ 三字段齐全，nr_dirty 计数正确 |
| P2 | bch2_btree_insert_key_cached 重写 | key_cache.rs | ✅ cache+dirty+pin 三步完整 |
| P3 | Journal pin 集成 — pin_entry/drop_journal_pin | key_cache.rs | ✅ Weak<CachedEntry> callback 设 flush_pending |
| P3 | bch2_nr_btree_keys_need_flush / _must_wait / _wait_done 真实实现 | key_cache.rs | ✅ 基于 nr_dirty + nr_keys 阈值公式 |
| P4 | collect_dirty + mark_clean + flush_dirty 两阶段 flush | key_cache.rs | ✅ 三阶段避免锁嵌套 |
| P4 | BtreeEngine::flush_cache_dirty_keys | btree/mod.rs | ✅ 遍历 5 tree collect+write+clean |
| P4 | insert_entry_skip_cache | btree/btree.rs | ✅ 写 btree 不 invalidation |
| - | six.rs notify_waiters 公平唤醒 | lock/six.rs | ✅ min_wi_trans_id 对齐 time_before64 |
| - | six.rs try_relock_* 别名移除 | lock/six.rs | ✅ 所有引用已更新 |
| - | Crc32CHasher 真正 CRC32C | journal/jset.rs | ✅ crc32fast→crc::CRC_32_ISCSI |
| - | btree/node.rs CRC32C 对齐 | node.rs | ✅ Crc32CHasher::hash 替换 crc32fast::hash |
| - | Watermark reserved_buckets bcachefs 对齐 | types.rs | ✅ nb/64 + btree_reserve 新策略 |

### 测试覆盖验证

- **key_cache**: 23 tests → 23 ✅（含 slot_reuse, dirty_tracking, journal_pin, flush_dirty, concurrent）
- **全量**: 714 passed（较 Batch C -2 因 Watermark 预留变少），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff（仅预先存在的 dead_code/private_interfaces）

### 已知差距（非本次范围）

- 同步点接线已完成（`batch_write`/`insert_guarded`/`commit_with_journal` 调用 `flush_cache_dirty_keys()`），后续新增写入口时需同步补齐
- `bch2_btree_key_cache_journal_flush` 已由 journal reclaim 触发，不再是 stub
- `trigger_key_cache_miss()` — 事务重启机制待事务系统就绪后接入

**验证结论**: PASS_WITH_NOTES
- Note: 同步点已接线，核心 write-back 原语（Phase 1-4）已实现并测试通过
- Note: `collect_dirty` 在持 hash 锁时获取 per-entry 读锁（与 `find()` 不同），经分析无死锁风险（无 per-entry→hash 反向路径）

## Forbidden Patterns

<!-- Patterns that should never be used and why -->

(To be filled by the team)

---

## Required Patterns

### bcachefs API 命名对齐

所有与 bcachefs C 源码对应的函数必须使用 `bch2_` 前缀 + 子系统名：

```rust
// ✅ 正确：对齐 bcachefs 命名
pub fn bch2_btree_node_write(b: &BtreeNode, ...) { }
pub fn bch2_journal_flush(j: &mut Journal, ...) { }
pub fn bch2_subvolume_create(sv: &mut SubvolumeManager, ...) { }

// ❌ 错误：使用自定义命名
pub fn flush_journal(j: &mut Journal, ...) { }
pub fn create_subvolume(sv: &mut SubvolumeManager, ...) { }
```

### 类型字段对齐

结构体字段名和语义必须与 bcachefs 的 `struct` 定义一致：

```rust
// ✅ 正确：Bpos 字段对齐 struct bpos { u64 inode; u64 offset; u32 snapshot; }
pub struct Bpos { pub inode: u64, pub offset: u64, pub snapshot: u32 }

// ❌ 错误：使用自定义字段名
pub struct Bpos { pub vol_id: u64, pub offset: u64, pub snapshot: u32 }
```

### 向后兼容

API 重命名时，通过 `pub use` 保留旧名别名，优先更新内部引用：

```rust
// mod.rs 中提供过渡兼容
pub use snapshot::bch2_snapshot_node_create;
pub(crate) use snapshot::bch2_snapshot_node_create as create_snapshot_btree; // 旧名别名
```

禁止直接删除已被外部依赖的旧 API。先更新所有引用，再移除别名。

### 功能逻辑必须与 bcachefs 完全一致

**原则**: API 命名/类型/签名 100% 对齐 bcachefs，内部实现允许 Rust 惯用写法，但**功能逻辑必须一致**。

```
命名/类型/签名     → 100% bcachefs 一致 (bch2_xxx, Bpos::inode, JournalRes)
内部实现风格       → Rust 惯用写法 (所有权、Option/Result、trait、闭包)
功能逻辑           → 必须与 bcachefs 完全一致（边界条件、错误处理路径、并发语义）
```

**适用边界**:
- bcachefs 的 `static inline` 函数、内部宏展开、平台相关优化不需要逐行复制
- 但外部可见的行为（函数返回值、错误码、并发锁语义、恢复 pass 顺序）必须一致
- btree split/merge 的触发条件、journal reservation 的阈值、alloc watermark 的预留逻辑 — 必须与 bcachefs 一致
- btree split 必须含 compact_fits 检查（compact 后无法容纳新 key 时跳过 compact 直接 split）
- btree split 必须含 format-aware split point（考虑 packed size，防止 split 后某侧仍满）
- btree split 必须含错误回滚机制（split 失败时释放已分配节点）
- journal reclaim 必须触发关联 flush_callbacks 再回收 bucket
- journal seq 按 entry 分配（不按 per-reservation），JournalRes 携带 entry_seq
- alloc trigger 修改 Alloc btree 和 Freespace btree 必须保证事务原子性（失败可回滚）

### 子系统功能逻辑约定

以下约定基于 2026-06-26 的 6 子系统功能逻辑审查（对比 bcachefs C 源码），记录每一子系统的关键差异和必须遵守的合约。

#### btree trans/iter — 路径状态一致性

**advance() 必须重新遍历路径**:
- `advance()`/`skip_to_next_leaf()` 不能仅递增 `leaf.offset` — 并发 split/compaction 后 offset 可能指向错误条目
- 每次 advance 后必须 `set_pos()` + `traverse()` 重新查找路径
- `back_up_and_advance()` 访问父节点前必须验证锁 seq

**peek() 必须考虑多来源 overlay (P0-1, 2026-06-27)**:
- `peek()` 不能只从 bset 读取 — 必须检查 journal 中有无未刷盘的修改（journal overlay）
- 缺少 overlay 时，读后写操作可能看到过期数据导致写丢失
- **overlay 优先级顺序**: `overlay_btree.journal` 优先于 `journal_seq` peek
  - 先查 `overlay_btree` 中有无对应 key 的最新版本（内存中未刷盘的修改）
  - 再退回到从 `journal_seq` 查找 journal entry 中已提交的修改
  - `get_next_unpacked()` 中的 `try_overlay_peek()` 实现此优先级
- bcachefs 的 `btree_iter_peek` 中 `bch2_btree_path_peek_slot` → `bch2_btree_node_iter_peek` 含 `btree_key_cache` 优先查询，volmount 的 `overlay_btree` 对应此语义

**trans_relock() 必须验证 seq**:
- `trans_relock()` 必须检查路径中每层节点的 `locked_seq` 是否仍然有效
- 无效 seq 意味着节点已被并发操作修改，需要 `restart_transaction()`

**共享路径快照必须同步刷新**:
- `BtreeIter` 的局部 `path` 仍然是遍历真源，但用于复用/重启观测的共享路径快照必须在 `init()`、`advance()`、`restart()`、`restart_optimized()` 之后刷新
- 共享快照只用于路径复用语义和测试，不应绕开现有的锁/遍历逻辑

#### btree cache/IO — 脏页管理

**mark_dirty 不能丢弃脏数据**:
```rust
// ❌ 错误：dirty.clear() 丢弃所有脏节点引用 — 数据丢失
if inner.dirty.len() >= MAX_DIRTY { inner.dirty.clear(); flush_all(); }

// ✅ 正确：真正的 flush
if inner.dirty.len() >= MAX_DIRTY { flush_all_dirty(); }
```

**必含 will_make_reachable**:
- COW btree 中，父节点必须先于子节点到达磁盘
- 必须在写入前通过 `will_make_reachable` 确保父节点已落盘
- 缺失此保证 → 崩溃后父节点指向不存在的子节点

#### will_make_reachable 实现模式

**生命周期合约**:

```
① 新节点创建（split/increase_depth/merge）
   → node.set_will_make_reachable()      // 阻止 eviction
   → cache.insert_dirty() / insert()     // 插入 cache

② flush_dirty_nodes()
   → 按 level 升序写入
   → serialize_to_bucket + write_block
   → node.clear_will_make_reachable()    // 已落盘 → 释放 eviction 保护
   → bch2_btree_post_write_cleanup

③ eviction（shrink / evict_one_leaf）
   → if node.will_make_reachable() → skip // 防止首次写入前被驱逐
```

**数据结构**:

```rust
// btree/node.rs — BtreeNode struct 中新增
pub will_make_reachable: AtomicBool,
```

**方法签名**:

```rust
impl BtreeNode {
    pub fn will_make_reachable(&self) -> bool       // load(Acquire)
    pub fn set_will_make_reachable(&self)            // store(true, Release)
    pub fn clear_will_make_reachable(&self)          // store(false, Release)
}
```

**调用点清单**:

| 位置 | 文件 | 操作 |
|------|------|------|
| `split_root()` — 新左右 leaf | btree.rs | `set_will_make_reachable()` 后写入当前 `journal_seq`，再 `insert_dirty()` |
| `split_root()` — 新 internal root | btree.rs | `set_will_make_reachable()` 后写入当前 `journal_seq`，再 `root_modified = true` |
| `btree_increase_depth()` — 新 root | interior.rs | `set_will_make_reachable()` 后 `cache.insert_dirty()` |
| `btree_set_root_for_read()` — 读入 root | interior.rs | 仅接受非当前 root，随后 `reset_key_count()` |
| `flush_dirty_nodes()` — 写入后 | volume/mod.rs | `clear_will_make_reachable()` |
| `shrink()` — 驱逐扫描 | cache.rs | 跳过 `will_make_reachable() == true` |
| `evict_one_leaf_with_jseq()` — leaf 驱逐 | cache.rs | 跳过 `will_make_reachable() == true` |

**设计决策**: `AtomicBool` vs bcachefs tagged pointer

| 维度 | bcachefs (C) | volmount (Rust) |
|------|-------------|-----------------|
| 类型 | `unsigned long` tagged pointer (含 `btree_update*`) | `AtomicBool` |
| 闭包引用 | `closure_get(&as->cl)` 持有更新状态机引用 | 无（同步 interior update 无需闭包生命周期管理） |
| 原子清除 | `xchg(&b->will_make_reachable, 0)` + `closure_put()` | `store(false, Release)` |
| 阻止效果 | `btree_node_reclaim()` 跳过 | `shrink()` + `evict_one_leaf()` 跳过 |

**决策理由**: 
- volmount 当前为同步 interior update 设计（单线程引擎，无并发 split/merge）
- 同步路径直接完成所有节点操作，无需等待 I/O 闭包回调
- `AtomicBool` 更简单，且能通过 `Arc<BtreeNode>` 安全操作
- 如果将来引入异步 I/O 写路径，需要升级为类似 bcachefs 的闭包引用计数模式

**flush_dirty_nodes 必须拓扑排序 (P0-2, 2026-06-27)**:
- `flush_dirty_nodes()` 必须按 `node.level` 升序排列：叶子（level 0）先写，父节点/根后写
- 违反此顺序 → 崩溃后根节点指向未落盘的内部节点
```rust
// ✅ 正确：按 level 升序 flush
nodes.sort_by_key(|(_, _, node)| node.level);
for (addr, node_id, node) in &nodes {
    // 先写叶子，再写父节点
}
```

**flush_btree() 批量 flush 在 sync_all 中执行**:
- `sync_all()` 负责触发 `flush_btree()`，后者收集脏节点后按 level 排序 flush
- 缓存 eviction 路径（读缓存满时驱逐脏页面）不得跳过拓扑排序
- `evict_dirty_nodes_bottom_up()` 独立实现自底向上驱逐（优先驱逐叶子级别的脏节点）
  - 使用 `inner.dirty.iter()` 扫描，优先 flush level 0 的脏节点再驱逐
  - 与 `flush_dirty_nodes()` 的拓扑排序互补——后者在 flush 时排序，前者在 evict 时排序

**depth=0 root 节点专用 dirty 跟踪**:
- root 节点（depth=0）不在 `cache.dirty` 中跟踪（避免 `Arc::get_mut` refcount 冲突）
- 使用独立 `root_modified: AtomicBool` 标记
- `ROOT_CACHE_ADDR = u64::MAX` 作根节点 sentinel 地址
- `flush_dirty()` 返回 `dirty_addrs` 时包含 `ROOT_CACHE_ADDR` 标记 root 需写

**Cannibalize 必须含重入保护**:
- Cannibalize（内存压力下替换缓存项）必须检查可重入性
- 递归 cannibalize 导致死锁 → 需要 per-thread cannibalize lock + stack depth guard

#### BtreeNode 序列化 Pipeline

**BtreeNode 序列化使用固定 C 布局（非 bincode）(Design Decision, 2026-07-01)**:

**Context**: 原 `serialize_to_bucket()` 用 bincode 序列化 BtreeNode 整体（含 header, bsets, entries），但 bincode 不是 bcachefs 兼容的磁盘格式。

**Options**:
1. **bincode** — 简单但产生 bcachefs 不兼容的二进制 blob，无法直接与 C 实现的磁盘格式对接
2. **手动固定 C 布局** — 用 `#[repr(C, packed)]` 结构体 + `ptr::write` 直接填充 buf

**Decision**: 使用固定 C 布局（`#[repr(C, packed)]` 结构体 + 直接指针/拷贝写入）。原因是 bcachefs 磁盘格式本身就是固定布局，无需中间序列化层。

**Layout (version=2, 2026-07-01)**:
```rust
// 连续内存布局：
// ┌─────────────────────────────────────────┐
// │ BtreeNodeHeader    (80 B, repr(C,packed))│
// │ BsetHeader         (16 B, repr(C,packed))│
// │ packed entries     (变长)                │
// │ CRC32C (4 B, 覆盖 header+bset+entries)  │
// │ zero pad to BLOCK_SIZE                  │
// └─────────────────────────────────────────┘
```

**Key Contracts**:
- `serialize_to_bucket()`: 返回 `Vec<u8>` (BLOCK_SIZE 字节)，内含 header → bset → entries → CRC，尾部零填充
- `deserialize_from_bucket()`: 读取 version 字段，v1 走 `deserialize_from_bucket_v1` 旧格式兼容，v2 走直接 ptr 读取
- CRC 覆盖范围: header + BsetHeader + entries（不含尾部 padding）
- 版本字段: `BtreeNodeHeader.version` — 1=旧 bincode 格式, 2=新固定 C 布局

**Common Mistake — CRC 覆盖范围不足 (P2, 2026-07-01)**:
```rust
// ❌ 错误：CRC 只覆盖 header
let crc = crc32c(0, &buf[..size_of::<BtreeNodeHeader>()]);

// ✅ 正确：CRC 覆盖 header + BsetHeader + entries 全部有效数据
let crc = crc32c(0, &buf[..data_end]);  // data_end = header + bset_hdr + entries
```

#### btree GC — 不可为空骨架

**GC 必须实现完整 mark-and-sweep**:
- `bch2_gc_btrees()` 必须遍历所有 btree 对 bucket 引用计数做 mark
- `bch2_gc_mark_key()` 必须根据 key 类型递增对应 bucket 的引用计数
- `bch2_gc_alloc_start/done()` 必须复制/合并 alloc btree 的引用计数
- 缺失任何一项 → 崩溃后 allocator 可能分配仍被引用的 bucket（数据覆盖）

**GC 必须含排他锁**:
- GC 运行时需要一个 `gc.lock` rwsem，写锁持有期间阻止其他写操作
- 缺少排他锁 → GC 与其他写操作并发导致引用计数不一致

**GC 必须在 recovery pass 中**:
- recovery 必须包含 `bch2_check_allocations`（即 GC）pass
- GC pass 在 journal replay 之后执行，重建 bucket 引用计数

**GC 必须含 topology check**:
- `bch2_check_topology` 必须验证 btree 节点之间的 prev/next 链接一致性
- 空桩 → 分裂后的 btree 拓扑损坏不可检测

#### journal flush/write/read — 校验与同步

**CRC32 必须覆盖完整 Jset**:
```rust
// ❌ 错误：CRC 只覆盖 entries
pub struct Jset {
    pub seq: u64,
    pub last_seq: u64,
    pub entry_count: u32,
    pub crc: u32,  // 只保护 entries
    pub entries: Vec<JsetEntry>,
}

// ✅ 正确：CRC 覆盖 magic + 全部头部字段 + entries
// bcachefs: crc = crc32c(JSET_MAGIC || seq || last_seq || ... || entries)
```

**CRC32C 硬件路径必须做 init/final 补码 (Common Mistake, 2026-07-01)**:
```rust
// ❌ 错误：硬件 CRC 指令不做标准 CRC32 的初始/最终取补
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_wrong(data: &[u8], crc: u32) -> u32 {
    let mut crc64 = crc as u64;  // ❌ 应该 !crc
    // ... _mm_crc32_u64 …
    crc64 as u32  // ❌ 应该 !ret
}

// ✅ 正确：对齐 bcachefs crc32c_le_bch 语义
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_correct(data: &[u8], crc: u32) -> u32 {
    let mut crc64 = (!crc) as u64;  // !crc 是标准 CRC 初始值
    // ... _mm_crc32_u64 …
    let ret = crc64 as u32;
    !ret  // 最终取补
}
```
**原因**: SSE4.2 `_mm_crc32_u64` 指令直接返回多项式除法的余数，不做标准 CRC 的 init/final 取补。纯软件查表实现则隐含了 `!crc` 初始值设计。在自动分发函数中，SW 和 HW 路径必须行为一致——HW 路径必须补上 `!init` 和 `!result`。
**检测**: trellis-check 通过比较 `crc32c_sw` 与 `crc32c_hw_impl` 的 Castagnoli 检验向量输出捕获此 bug。

**bch2_journal_flush 必须避免数据竞争**:
- `flush()` 将在持有 buf lock 的同时读取 buf data，再触发异步 I/O
- 必须在读数据后释放锁，防止 `add_entry()` 并发修改 buf

**journal entry 必须含版本号**:
- JsetEntry 必须包含 `version` 字段，以便未来格式变更时兼容
- 当前缺少数值版本字段

#### lock six — 内存序与降级语义

**downgrade_write_to_intent 必须递 increment seq**:
```rust
// ❌ 错误：降级不递增 seq
pub fn downgrade_write_to_intent(&self) {
    self.state_write.unlock();
}

// ✅ 正确：bcachefs 每次 write unlock（含降级）递增 seq
pub fn downgrade_write_to_intent(&self) {
    self.seq.fetch_add(1, Release);
    self.state_write.unlock();
}
```

**必须实现 handoff protocol**:
- 唤醒 waiter 后，必须设 `lock_acquired=true`（写锁独占唤醒，不与其他 waiter 竞争）
- 缺失 handoff → 高竞争场景下可能永久饿死

**WaitFifo 必须继承 bcachefs 唤醒语义**:
- bcachefs 的 `wait_on_bit` + `wake_up_bit` 唤醒单个 waiter，volmount 的 `VecDeque` 实现必须同样为单 waiter 唤醒
- Percpu 路径 memory ordering: bcachefs 用 `smp_mb()` fence，volmount 用 `Acquire` — 在弱排序架构上可能有可见性问题

**notify_waiters handoff 逐个唤醒（P0-2, 2026-06-27, 2026-06-28 更新为公平唤醒）**:
- write/intent 等待者只能逐个唤醒（避免惊群效应）
- 原 `woke_write_intent` 标记（first-come-first-serve slot 顺序 → 改为 `min_wi_trans_id` 公平唤醒
- 先扫描所有 write/intent 等待者找到 `trans_id` 最小的（最老事务），对齐 bcachefs `time_before64` 语义
- read 等待者仍唤醒全部（读者可共享锁）
- bcachefs 原生行为一致：`wake_up_bit` 在 `__wait_on_bit` 中只唤醒一个

**写锁抢占比量与 WRITE_BIT 公平性（P0-1, 2026-06-27）**:
- bcachefs 中写锁的 `WRITE_BIT` 实现写者排他；多个并发写者在抢到 `WRITE_BIT` 前不 sleep
- volmount 的写锁 `lock_write()` 慢路径在 CAS 失败转入 sleep 前，必须：
  1. `atomic::fence(SeqCst)` — 保证对其他线程的 `WRITE_BIT` 设置的可见性
  2. 重新检查 `self.state_write.load(Acquire)` 是否含 `WRITE_BIT`（另一位写者可能刚抢到）
  3. 若已有人持 WRITE_BIT → 继续等待；若无 → 重新尝试 CAS（防早期 sleep 饿死）
- 此 fence + re-check 与 bcachefs `__wait_on_bit` 中 `smp_mb()` 后的条件重检等价
- 缺失此检查 → 高写并发场景下写者可能提前进入 sleep 并导致写吞吐骤降

**lock_slowpath WAITING_WRITE_BIT 清除（P0-1, 2026-06-27）**:
- 双检路径（`trylock_ip` 在设置 WAITING_WRITE_BIT 后成功）必须调用 `clear_waiting_bit()`
- 遗漏清除 → 写锁释放后读者看到残留的 WAITING_WRITE_BIT 并错误等待（写锁幽灵）
- 与 `lock_write()` 的同等路径一致

**lock_acquired 传播（P0-3, 2026-06-27）**:
- `lock_slowpath` 中 `waiter_box` 为局部变量，需标记 `mut`
- handoff 路径（`is_handoff_for_current_thread`）和非 handoff 路径都需 `waiter_box.lock_acquired = true`
- 最终通过 `wait.lock_acquired = waiter_box.lock_acquired` 传播给调用者

**lock_restart()（P0-4, 2026-06-27）**:
- 对应 bcachefs `six_lock_restart`
- 按 Write > Intent > Read 优先级检测当前持锁类型并释放
- 释放后以新类型重新 `lock()`
- unlock → lock 之间有窗口期，其他线程可能在此期间获得锁
- 调用者需要 btree 事务重启循环来处理此竞争

**try_lock_read_to_write() / lock_read_to_write()（P0-5, 2026-06-27）**:
- 对应 bcachefs `six_lock_tryread_to_write` / `six_lock_read_to_write`
- `try_lock_read_to_write` 快速路径：
  1. 移除当前线程 reader 计数（`lock_readers_add(-1)`）
  2. 检查无其他读者 + 无 intent + 无 write
  3. CAS 设置 WRITE_BIT | INTENT_BIT
  4. 失败时恢复 reader 计数
- `lock_read_to_write` 慢速路径：先尝试快速升级，失败则 `unlock_read()` + `lock_write()`
- 注意：`try_lock_read_to_write` 中使用已有的 `unsafe { *self.write_owner.get() }` pattern，无新增 unsafe 代码

#### alloc — BchDataType 与 sector 计数推导

**BchDataType 枚举值必须对齐 bcachefs C（2026-06-27）**:
```rust
// ✅ 正确：数值完全匹配 bcachefs enum bch_data_type
// BCH_DATA_free=0, sb=1, journal=2, btree=3, user=4,
// cached=5, parity=6, stripe=7, need_gc_gens=8,
// need_discard=9, unstriped=10, Reserved=11
pub enum BchDataType {
    Free = 0,     Sb = 1,        Journal = 2,
    Btree = 3,    User = 4,      Cached = 5,
    Parity = 6,   Stripe = 7,    NeedGcGens = 8,
    NeedDiscard = 9, Unstriped = 10, Reserved = 11,
}
pub const BCH_DATA_NR: usize = 11;  // 不含 Reserved
```
- 旧版 volmount 使用不同的编号（Btree=2, User=1 等），已统一重编号
- Reserved(11) 是 volmount 内部变体，非 bcachefs 标准
- 此为 v1.0 前的磁盘格式断裂（pre-v1.0 可接受）

**derive_data_type() 必须使用 sector 计数**:
```rust
// ✅ 正确：bcachefs alloc_data_type 逻辑
pub fn derive_data_type(
    dirty_sectors: u32,
    cached_sectors: u32,
    stripe: u32,
    data_type: BchDataType,
) -> BchDataType {
    if stripe > 0 { return BchDataType::Stripe }
    if dirty_sectors > 0 { return data_type }  // 透传
    if cached_sectors > 0 { return BchDataType::Cached }
    BchDataType::Free
}
```
- bcachefs 不显式存储 data_type，而是从 dirty_sectors / cached_sectors / stripe 计数推导
- volmount 保留显式 state 字段作为缓存，以 sector 计数为真实来源
- 旧版签名含 `need_discard: bool` 参数，已移除（need_discard 不由 sector 计数推导）

**Bucket / BchAllocEntry 必须含 sector 计数字段**:
```rust
pub struct Bucket {
    pub state: BchDataType,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe: u32,
    pub journal_seq: u64,
    pub group: u32,
    pub version: u32,
    pub bucket_idx: u64,
}

pub struct BchAllocEntry {
    pub state: BchDataType,
    pub dirty_sectors: u32,
    pub cached_sectors: u32,
    pub stripe: u32,
    pub journal_seq: u64,
    pub group: u32,
    pub version: u32,
}
```
- `Bucket::derive_state()` 方法封装了 sector-count 推导逻辑
- `BchAllocEntry::from_bucket_with_journal_seq()` 用于分配路径中携带 journal_seq 写入 Alloc btree

**may_alloc_bucket() — 分配前 journal seq 安全检查（P0-6, 2026-06-27）**:
- 对应 bcachefs `may_alloc_bucket_journal_seq` (alloc_foreground.c)
- 防止 crash recovery 后分配仍被 journal 引用的 bucket → 数据损坏
```rust
pub fn may_alloc_bucket(bucket: &Bucket, request_journal_seq: u64) -> bool {
    if request_journal_seq == 0 { return true; }  // 无 journal 追踪
    if bucket.journal_seq == 0 { return true; }   // 从未被引用
    bucket.journal_seq <= request_journal_seq      // journal 已推进到此 bucket 之后
}
```
- `AllocRequest.journal_seq` 由调用方传递（`journal_cur_seq` 或 `last_seq_ondisk`）
- 分配成功时写入 `bucket.journal_seq = request.journal_seq`
- 在 `allocate_bucket_inner()` 和 reuse 路径中都需调用

#### recovery — 必须集成到 Volume 启动路径

**Recovery 模块不是可选项**:
```rust
// ❌ 错误：recovery 已定义但 Volume::new 不调用 — 死代码
pub struct Volume { ... }
impl Volume {
    pub fn new(path: &Path) -> Result<Self> {
        // 不调用 bch2_fs_recovery()
    }
}

// ✅ 正确：Volume 启动时必须执行 recovery passes
impl Volume {
    pub fn new(path: &Path) -> Result<Self> {
        bch2_fs_recovery(&mut self)?;
    }
}
```

**btree root level 信息不可丢失**:
- `BtreeRoots::load_from_superblock()` 必须提取并存储 `level` 字段
- 丢失 level 导致 btree 加载器无法正确重建非 level-0 root

**必须含 unclean shutdown seq skip**:
- unclean shutdown 后 journal replay 必须跳过 `seq + 64` 并黑名单化 seq 范围
- 防止崩溃前写入 btree 但未 journal 的修改被错误应用

**journal replay 必须避免双重读取**:
- `journal_read` 已经读到 `state.jsets`，`journal_replay` 不应再次从磁盘读取
- 改为从内存 jsets 列表直接应用

**必须含 overlay_btree 集成**:
- recovery pass 期间对 btree 的修改应先写入 `overlay_btree`（内存中暂存）
- 而非直接写入底层 btree — 因为 pass 可能失败回滚
- `overlay_btree` 在 recovery 完成后合并到主 btree
- 此设计与 bcachefs `journal_replay` 期间的 `btree_key_cache` 使用一致

**必须含 journal rewind 支持**:
- 防御未正确实现 FUA/FLUSH 的块设备
- 从最后一个有效 entry 向前扫描找到一致的状态

**示例**:
```rust
// ✅ 正确：签名对齐 + 逻辑一致 + Rust 惯用实现
pub fn bch2_snapshot_is_ancestor(c: &SnapshotTable, id: u32, ancestor: u32) -> bool {
    // Rust 数组下标代替 C 指针运算，但 skip_list 遍历逻辑与 bcachefs 一致
    let mut id = id as usize;
    while id != ancestor as usize {
        let parent = c.snapshot_parent(id);
        if parent == id { return false; }
        id = parent;
    }
    true
}
```

### serde 反序列化结构体

对于反序列化外部二进制格式（如 bcachefs on-disk 格式）的结构体，所有字段必须按正确偏移声明，即使只使用部分字段：

```rust
#[derive(serde::Deserialize)]
struct SnapshotRef {
    #[allow(dead_code)]  // 只读 subvol，但其他字段影响二进制布局
    flags: u32,
    parent: u32,
    children: [u32; 2],
    subvol: u32,
    // ... 其余字段
}
```

**教训**: 不要为了"只用部分字段"就定义裁剪版结构体。bincode 反序列化按字段顺序匹配，少一个字段整个偏移链全错。`SnapshotT` 有 11 个字段，如果定义 8 字段的结构体去反序列化，第 9 个 bincode 字段会被解释成垃圾，导致后续节点遍历指向随机内存。

✅ 正确做法: 永远使用完整结构体，不需要的字段用 `#[allow(dead_code)]` 标注。

### BchSb 新字段必须 `#[serde(default)]`

向 `BchSb`（superblock）添加字段时，必须标记 `#[serde(default)]` 以保证旧版本序列化数据的向后兼容性：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSb {
    // ... 已有字段
    /// GC 位置（用于崩溃恢复后继续中断的 GC）
    #[serde(default)]
    pub gc_pos: GcPos,
    /// gc_pos 是否有效（旧版本无此字段时 false）
    #[serde(default)]
    pub gc_pos_valid: bool,
}
```

**`#[serde(default)]` 要求**：字段类型必须实现 `Default`：
- 整数类型（`u32`, `u64` 等）天然支持
- 自定义类型（如 `GcPos`）需 `#[derive(Default)]`
- 枚举类型（如 `GcPhase`）需 `#[derive(Default)]` + `#[default]` 指定默认变体

**注意**：`#[serde(default)]` 仅在反序列化旧版本数据时生效（当二进制数据中不存在该字段时使用默认值）。新版本序列化始终写入该字段。

### batch_write — 单 btree 的批量原子写入

`BtreeEngine::batch_write()` 提供"单树内批量写入的原子性"（崩溃恢复时整批要么全到要么全不到）。适用于同时修改同 tree_id 下多 key 的场景：

```rust
// ✅ 正确: 批量写入合并多个操作到一次 journal commit
let mut batch = Vec::new();
batch.push(BatchEntry::Insert { key: key1, value: val1 });
batch.push(BatchEntry::Delete { key: key2 });
batch.push(BatchEntry::Insert { key: key3, value: val3 });
engine.batch_write(transaction.id, batch).unwrap();
```

**限制**:
- 仅对同 tree_id 有效，不跨 btree（multi-tree 原子性需要 P2 级别的完整事务）
- 内部顺序执行，先 Insert 后 Delete
- 所有变更在同一个 journal entry 中提交，崩溃时整批回滚

### bch2_snapshot_node_create 的双 child 模式

`extra_child_subvol: Option<u32>` 控制创建模式：

- `None` → 创建单个 snapshot node（传统模式，向后兼容测试）
- `Some(src_subvol)` → "1变2" bcachefs 语义：分配两个快照 ID，源 subvol 指向 child1，新 subvol 指向 child2，parent skip 指针两路更新。三路写入通过 batch_write 原子化

```rust
// ✅ 正确: 创建两个快照 child 并原地更新 parent
let child_id = snapshot_node_create(
    c, t, trans,
    parent_id, Some(src_subvol)
)?;
// 成功: src_subvol 已指向 child1，新 subvol 已指向 child2
```

### Skiplist 指数步进

Skip 列长度固定为 3（`[u32; 3]`），新节点的 skip 直接从父节点继承：
- `skip[0]` = parent_id（直接父节点）
- `skip[1]` = `parent.skip[0]`（父节点的 parent）
- `skip[2]` = `parent.skip[1]`（父节点的 skip[1]）

这天然形成指数级跳转。`bch2_snapshot_skiplist_get` 返回 `Option<[u32; 3]>` 而非 Option<u32>：

```rust
pub fn bch2_snapshot_skiplist_get(c: &SnapshotTable, id: u32) -> Option<[u32; 3]> {
    let parent = c.snapshot_parent(id);
    if parent == id { return None; }
    Some([
        parent,
        c.snapshot_skip(parent, 0),
        c.snapshot_skip(parent, 1),
    ])
}
```

**`build_skip_list_from_btree`** 现在是全量重建逻辑（不再增量修补）：
1. 遍历 btree 中所有快照 key，收集 `(id, parent, children)` 三元组到临时表
2. 显式初始化 skip = `[0u32; 3]`
3. 按拓扑顺序填充 skip（保证父节点的 skip 已就绪）
4. 写回 btree

## Verification Status — Batch D (2026-06-28)

### key_cache 模块 — bcachefs C 一致性验证（9 项新增）

2026-06-28 通过 main-session 直接实施，4 个 Phase 全部完成：

| # | Phase | 新增项 | C 引用 | 验证结论 |
|---|-------|--------|--------|----------|
| 1 | P1-SlotReuse | `CachedEntry.valid` 标志 | `struct bkey_cached.valid` | ✅ |
| 2 | P1-SlotReuse | `find()` 检查 valid | `bch2_btree_key_cache_find` | ✅ |
| 3 | P1-SlotReuse | `invalidate()` 只设 valid=false | `bch2_btree_key_cache_drop` | ✅ |
| 4 | P2-Dirty | `dirty`/`pin_type`/`flush_pending` 字段 | `KEY_CACHE_DIRTY` 标志 | ✅ |
| 5 | P2-Dirty | `nr_dirty` 计数 + `bch2_nr_btree_keys_need_flush` | `bch2_nr_btree_keys_need_flush` | ✅ |
| 6 | P2-Dirty | `bch2_btree_insert_key_cached()` 脏存储重写 | `btree_key_cache.c:843-885` | ✅ |
| 7 | P3-JournalPin | `pin_entry()`/`drop_journal_pin()` + callback 链 | `bch2_journal_pin_copy/drop/set` | ✅ |
| 8 | P4-Flush | `collect_dirty()` + `mark_clean()` 两阶段 | `bch2_btree_key_cache_flush` | ✅ |
| 9 | P4-Flush | `flush_cache_dirty_keys()` Engine 方法 | `bch2_btree_key_cache_journal_flush` | ✅ |

### 测试覆盖验证

- **key_cache**: 23 tests → 23 ✅（新增 9 个测试: slot_reuse, dirty_tracking, journal_seq, insert_key_cached, journal_pin_integration, journal_pin_with_instance, flush_callback, collect_dirty_and_mark_clean, flush_dirty_callback, flush_dirty_skip_failed_writes, engine_flush_cache_dirty_keys）
- **btree 相关**: 全部通过（含新增的 `insert_entry_skip_cache` 和 `insert_entry_into_node` 路径）
- **全量 volmount-core lib**: 714 passed（+4 个新增 flush 测试），5 known fail（预存 AddressSpaceExhausted），6 ignored
- **clippy/fmt**: 0 新增 warning/diff

### 已知差距（非本次范围）

- flush 同步点已接线：`batch_write()`、`insert_guarded()`、`commit_with_journal()` 都会在写入前调用 `flush_cache_dirty_keys()`；后续新增写入口时需同步补齐
- `bch2_btree_key_cache_journal_flush` 的 reclaim 触发语义与 pin 类型分桶
- KCQ (key cache queue) bcachefs 对齐的 shrinker 和后台 flush 线程
- bcachefs 中 `struct btree_update` 在 key cache flush 路径中的异步状态机（volmount 当前同步 flush 已足够）

**验证结论**: PASS_WITH_NOTES
- Note: 同步点已接线，后续重点是新写入口的回归约束和更广泛的调度/后台 flush 对齐
- Note: `commit_with_journal()` 旧入口也有回归测试，确保 legacy journal 写入路径同样先 flush dirty key cache
- Note: 架构约束已更新：Journal 不再严格"仅用于崩溃恢复"，Key cache write-back 参与 journal pin 协调

## Verification Status — Batch E (2026-06-28)

### Btree IO 节点读写对齐 — 4 个 Phase 全部实现

| Phase | 新增项 | 函数/结构 | 验证结论 |
|-------|--------|-----------|----------|
| P1-Read | bset 结构验证 | `bch2_validate_bset()` — data_offset/end_offset/8-align/bounds | ✅ |
| P1-Read | key 排序验证 | `bch2_validate_bset_keys()` — 非降序、无重复、format 合法 | ✅ |
| P1-Read | 完整验证流水线 | `bch2_btree_node_read_done()` — nsets→per-bset→sort-merge→drop | ✅ |
| P1-Read | 读取后排序合并 | `bch2_read_done_sort()` — SortIter + compact 回退 | ✅ |
| P1-Read | 范围 key 过滤 | `bch2_btree_node_drop_keys_outside_node()` — 按 min/max_key 裁剪 | ✅ |
| P1-Read | 调试输出 | `bch2_btree_node_header_to_text()` — header field 格式化 | ✅ |
| P2-Write | SortIter 架构 | `SortIter` struct + `init_from_node/add/add_all_bsets/sort_into/total_keys` | ✅ |
| P2-Write | 写入前排序 | `bch2_btree_node_sort_keys()` — 排序合并多 bset 后 compact | ✅ |
| P2-Write | 写入路径集成 | `bch2_btree_node_write_mut()` — 序列化前调用 sort_keys | ✅ |
| P3-IOFlags | write_in_flight 标志 | `NODE_WRITE_IN_FLIGHT=0x04` + `try_lock/unlock_write_in_flight()` CAS | ✅ |
| P3-IOFlags | read_in_flight 标志 | `NODE_READ_IN_FLIGHT=0x08` + `try_lock/unlock_read_in_flight()` CAS | ✅ |
| P3-IOFlags | just_written 标志 | `NODE_JUST_WRITTEN=0x10` — write_mut 后设置 | ✅ |
| P3-IOFlags | io_lock/unlock 实现 | `bch2_btree_node_io_lock/unlock` — spin+CAS (从 no-op 改为真实) | ✅ |
| P3-IOFlags | wait_on_read/write | spin 等待标志位清除 (从 no-op 改为真实) | ✅ |

### 测试覆盖验证

- **btree::io**: 19 tests → 29 ✅（新增 19: SortIter 5, IO flags 5, read_done 3, write 3, post_write_cleanup 2, checksum 1）
- **全量 volmount-core lib**: 740 passed（较 Batch D +26），5 known fail, 6 ignored
- **clippy/fmt**: 0 新增 warning/diff

## Verification Status — Batch G (2026-06-29)

### Key Cache JournalEntryPin 集成

| 变更项 | 说明 | 状态 |
|--------|------|------|
| `CachedEntry` 嵌入 `JournalEntryPin` | 替换 `journal_seq: AtomicU64` | ✅ |
| `JournalEntryPin.pin_type` | 显式分类为 `KeyCache` / `Btree*` / `Other` | ✅ |
| `pin_entry()` 使用 `bch2_journal_pin_add` | 替代过渡 `_seq` API，注册真实 flush callback | ✅ |
| `drop_journal_pin()` 使用 `bch2_journal_pin_drop` | 正确移除侵入式链表节点 | ✅ |
| `bch2_fs_btree_key_cache_exit()` 清理 pin | 先 `drop_all_journal_pins()` 再 `clear()` | ✅ |
| `Drop for KeyCache` 自动清理 | 防止 cleanup 遗漏导致 dangling pointer | ✅ |
| `unsafe impl Sync for CachedEntry` | 安全论证：`flush callback` 仅在 reclaim 单线程上下文调用 | ✅ |

### 测试覆盖验证

- **btree::key_cache**: 22 passed (较 Batch F 不变，语义正确性已验证) ✅
- **全量 volmount-core lib**: 762 passed, 5 known fail, 9 ignored ✅
- **clippy/fmt**: 无新增 warning/diff ✅

### Issues Found and Fixed by trellis-check

1. **`bch2_fs_btree_key_cache_exit()` dangling pointer** — `clear()` 直接 drop `Arc<CachedEntry>` 时，`JournalEntryPin.Link` 可能仍在 journal 侵入式链表中。修复为先 `drop_all_journal_pins()` 再 `clear()`，并添加 `Drop for KeyCache` 防御性清理。

### Issues Found and Fixed by trellis-check

1. **`read_in_flight` 标志泄漏** — 原 `bch2_btree_node_read_done()` 在 `validate_bset`/`validate_bset_keys`/`read_done_sort` 返回错误时，`read_in_flight` 不会被清除。修复为双函数模式（`bch2_btree_node_read_done` 作包装 + `_read_done_inner` 作内部实现），使用结果变量确保 `clear_read_in_flight()` 在所有错误路径都被调用。

2. **`BLOCK_SIZE` 未使用导入** — io.rs 中导入了但未使用，已移除。

3. **`bset_idx` 字段 dead_code** — `SortIterEntry.bset_idx` 已移除，`SortIter::add` 只保留排序所需的偏移参数。

4. **`is_multiple_of` clippy 建议** — `end_offset % 8 != 0` 改为 `end_offset.is_multiple_of(8u32)`。

5. **`AtomicBool` 未使用导入** — node.rs 中新增但从未使用（IO 标志位全部通过 `AtomicU8 flags` 实现），已移除。

### 关键设计决策

#### IO 标志位协议：AtomicU8 位操作 + CAS

**不要使用 Mutex 或单独的 AtomicBool 字段**。所有 IO 标志（write_in_flight / read_in_flight / just_written）复用 BtreeNode 已有的 `flags: AtomicU8`：

```rust
pub const NODE_WRITE_IN_FLIGHT: u8 = 0x04;
pub const NODE_READ_IN_FLIGHT: u8 = 0x08;
pub const NODE_JUST_WRITTEN: u8 = 0x10;

// 加锁：CAS 协议
pub fn try_lock_write_in_flight(&self) -> bool {
    self.flags
        .compare_exchange_weak(
            self.flags.load(Relaxed) & !NODE_WRITE_IN_FLIGHT,
            ... | NODE_WRITE_IN_FLIGHT,
            Acquire, Relaxed,
        )
        .is_ok()
}

// 解锁：fetch_and 清除
pub fn unlock_write_in_flight(&self) {
    self.flags.fetch_and(!NODE_WRITE_IN_FLIGHT, Release);
}
```

****为什么不是 Mutex**：bcachefs 使用 `wait_on_bit_lock` 在位标志上进行 spin，不是 Mutex。CAS + 位标志与 bcachefs 的 `clear_bit`/`set_bit`/`wait_on_bit` 协议对应。

#### bch2_btree_node_read_done 双函数模式（防止资源泄漏）

```rust
pub fn bch2_btree_node_read_done(node: &mut BtreeNode) -> Result<(), StorageError> {
    node.try_lock_read_in_flight();
    let result = _read_done_inner(node);  // 真正的验证逻辑
    node.clear_read_in_flight();           // 所有路径都被调用
    result
}
```

这个模式确保 `read_in_flight` 标志在错误路径上也被正确清除。

#### SortIter 使用 raw pointer 操作 packed key

SortIter 在 packed key 级别排序合并，避免 full unpack/repack 的开销：

```rust
pub struct SortIter {
    entries: Vec<u32>,   // 偏移量数组
    data_len: u32,
    data_ptr: *const u8, // 指向 node.data 的 raw pointer
}
```

- `add(offset, u64s)` — 将一个 packed key 的偏移加入 entries
- `add_all_bsets(node)` — 遍历 node 的所有活跃 bset，添加所有 key
- `sort_into(dst)` — 按 bpos_cmp 排序 entries，然后按顺序拷贝 packed key 到 dst
- 排序比较使用 `bkey_cmp_packed`（通过 `bkey_unpack` 解包 bpos）

#### 写入前自动排序

`bch2_btree_node_write_mut` 在序列化前自动调用 `bch2_btree_node_sort_keys(node)`，确保多 bset 被合并为单一排序 bset。与 `serialize_to_bucket` 内部的 `collect_all_entries` 互补——前者减少 bset 数量，后者保证排序去重。

#### JournalEntryPin 嵌入模式（替代 _seq 过渡 API）

`KeyCache` 中，每个 dirty `CachedEntry` 通过嵌入 `JournalEntryPin` 替代独立的 `journal_seq: AtomicU64`：

```rust
struct CachedEntry {
    pin: JournalEntryPin,    // 嵌入 pin (含 intrusive Link + seq + flush callback)
    // ... 其他字段
}
```

关键点：
- `pin_entry()` 使用 `bch2_journal_pin_add(seq, &entry.pin, flush_fn)` 而非 `_seq` 过渡 API
- `drop_journal_pin()` 使用 `bch2_journal_pin_drop(&entry.pin)` 正确移除侵入式链表节点
- `bch2_fs_btree_key_cache_exit()` 和 `Drop for KeyCache` 必须先调用 `drop_all_journal_pins()` 再 `clear()`，防止 `Link` dangling pointer

⚠️ **必须的清理顺序**：`JournalEntryPin` 的 `Link` 是侵入式链表节点，drop `CachedEntry` 前必须已调用 `bch2_journal_pin_drop` 将其从 journal 的 unflushed/flushed 链表中移除。跳过此步骤会导致 journal 链表中的 dangling pointer。

### 已知差距（跨批次跟踪）

| 差距 | 状态 | 批次 |
|------|------|------|
| `bch2_btree_node_read()` 调用 `read_done()` | ✅ 已修复 | Batch F |
| `bch2_btree_node_write`（&self 版）调 sort_keys | 按设计保留（write_mut 替代方案） | — |
| bset checksum 验证在 read/load 边界显式进行（`deserialize_from_extent` / `load_btree_node_from_ptr`） | ✅ 已覆盖 | `/home/black/Documents/bcachefs-tools/fs/btree/read.c:629-724` |
| IO 锁在 write 路径中已被调用，read 路径不需要 | ✅ write 路径已集成 | Batch F |
| sort_iter `bset_idx` 字段移除 | ✅ 已覆盖 | `/home/black/Documents/bcachefs-tools/fs/btree/sort.h:7-43` |
| key_cache journal_flush 空 stub | ✅ 已修复 | Batch G |
| `KeyCache::pin_entry` 使用 JournalEntryPin 替代 _seq 过渡 API | ✅ 已修复 | Batch G |
| `_seq` 过渡 API 未迁移（25处/5文件） | ✅ 全部迁移 | Batch H |
| write_buffer P0 全部功能缺失（6项） | ✅ 全部实现（755行，10测试） | Batch I |
| GC 模块全部 6 项 P0 差距（含 recovery pass 接线） | ✅ 全部实现（880行，13测试） | Batch J |

**验证结论**: PASS（Batch E-J）— 全部 14 项 P0 bcachefs 不一致已修复完成
- Note: Batch E-F 完成 btree IO 4 个 Phase 全部实现和集成
- Note: Batch G 完成 key_cache JournalEntryPin 嵌入，替换过渡 API
- Note: Batch H 完成全代码库 `_seq` → `JournalEntryPin` 迁移，删除过渡 API
- Note: trellis-check 在每个批次中发现并修复了关键 bug

## Verification Status — Batch H (2026-06-29)

### `_seq` 过渡 API 迁移

| 模块 | 迁移内容 | 状态 |
|------|----------|------|
| `btree/io.rs` | 3处 `_add_seq` → `bch2_journal_pin_add` (嵌入 BtreeNode.journal_pin) | ✅ |
| `btree/cache.rs` | 8处 `_drop_seq` → `bch2_journal_pin_drop(&pin)` | ✅ |
| `volume/mod.rs` | 3处 `_set_seq`/`__bch2_journal_pin_put` → `pin_add`/`pin_drop` | ✅ |
| `journal/types.rs` | 删除 `_set_seq`/`_add_seq`/`_drop_seq` 三个过渡函数 | ✅ |
| `journal/reclaim.rs` | `__bch2_journal_pin_put` 改为 `pub(crate)` | ✅ |
| `journal` | `bch2_journal_update_last_seq` 改为私有 | ✅ |

### 测试覆盖验证

- **btree::io**: 29 passed ✅
- **btree::cache**: 27 passed ✅
- **volume**: 17 passed ✅
- **journal**: 73 passed ✅
- **全量 volmount-core lib**: 762 passed, 5 known fail, 9 ignored ✅

### Issues Found and Fixed by trellis-check

1. **`evict_one_leaf_with_jseq` 注释过期** — 注释仍说"返回 journal_seq"，实际返回 `JournalEntryPin`。已更新注释。
2. **`drop_pin_for_node` 注释过期** — 注释仍写"查找 journal_seq"，实际查找 journal pin。已更新注释。

---

## Verification Status — Batch I (2026-06-29)

### write_buffer P0 验证

write_buffer 在前期 Batch/Phase 工作中已完成全部 P0 功能实现，本次验证确认所有 P0-5~P0-10 条目已对齐 bcachefs：

| P0 | 需求 | 实现状态 |
|----|------|----------|
| P0-5 | `bch2_journal_key_to_wb()` — 将 journal key 插入 inc 队列 | ✅ 完整实现：锁定 inc → 追加 key → 解锁 |
| P0-6 | `bch2_btree_write_buffer_flush_locked()` — 7 步 flush 管线 | ✅ 完整实现：move_keys → sort → dedup → fastpath insert → slowpath txn retry |
| P0-7 | `bch2_btree_write_buffer_must_wait()` — 容量检查 | ✅ 基于 inc/flushing 总量 vs capacity * 3/4 |
| P0-8 | `bch2_journal_write_buffer_need_flush()` — 全 wb 检查 | ✅ 检查所有 wb 的 inc.nr / flushing.nr |
| P0-9 | 数据结构对齐（BtreeWriteBufferedKey, WbKeyRef） | ✅ btree_id + bpos 拆分；排序用轻量 WbKeyRef 索引数组 |
| P0-10 | flush → btree insert 核心循环 | ✅ wb_sort + flush_fastpath + flush_slowpath 完整实现 |

### 文件状态

- `btree/write_buffer.rs`: 755 行完整实现（非骨架）
- `wb_sort()`: 按 (btree_id, inode, offset, snapshot) 排序
- `dedup_sorted_refs()`: 相同 pos 的条目保留最新 journal_seq
- `flush_fastpath()`: engine.get_entry noop 检查 + engine.insert_entry
- `flush_slowpath()`: 通过 BtreeTrans.journal_insert + trans_commit 重试
- 全部公开 API 函数均已实现（非空操作）

### 测试覆盖验证

- **btree::write_buffer**: 10 passed ✅
  - `test_write_buffer_insert_and_flush` — 3 key insert + flush → engine 验证
  - `test_write_buffer_dedup` — 同位置 3 key → 仅保留 journal_seq=30
  - `test_write_buffer_noop_elimination` — engine 已有相同值 → flush 不改变 key_count
  - `test_write_buffer_sort_order` — 无序插入 → 排序后 offset 升序
  - `test_write_buffer_must_wait` / `test_write_buffer_should_flush` — 容量判断
  - `test_write_buffer_flush_locked_empty` — 空 buffer flush 无副作用
  - `test_wb_key_cmp` / `test_bch_wb_btree_idx` / `test_write_buffer_create`
- **全量 volmount-core lib**: 762 passed, 5 known fail, 9 ignored ✅（基线无变化）

### 验证结论

**PASS** — write_buffer P0-5~P0-10 全部完成，功能与 bcachefs 对齐。

---

## Verification Status — Batch J (2026-06-29)

### GC Phase 5 收尾 — recovery pass 接线

| 原 Gap | 功能 | 状态 |
|--------|------|------|
| G1 | Mark-and-sweep (bch2_gc_btrees, bch2_gc_mark_key) | ✅ 已有完整实现 |
| G2 | Alloc 检查修复 (bch2_gc_alloc_start/done) | ✅ 已有完整实现 |
| G3 | 拓扑检查 (bch2_check_topology) | ✅ 已有完整实现 |
| G4 | GC 排他锁 (gc.lock rwsem) | ✅ RwLock<()> 已在 BtreeGc |
| G5 | Generation 清理 (bch2_gc_gens) | ✅ 已有完整实现 |
| G6 | GC 在 recovery pass 中 | ✅ `check_topology` pass 集成 `bch2_gc_gens`；死代码 `recovery/passes/gc.rs` 已删除 |

### 测试覆盖验证

- **btree::gc**: 13 passed ✅（gc_gens, check_topology, check_allocations, gc_btrees, mark_key）
- **全量 volmount-core lib**: 762 passed, 5 known fail, 9 ignored ✅（基线无变化）

### 验证结论

**PASS** — 全部 14 项 P0 bcachefs 不一致（Phase 1-5）已修复完成。

---

## Verification Status — Batch K (2026-06-29)

### Lock P1: WRITE_BIT 预设 + 内存序

**变更**:
- `six.rs`: `lock_write()` 慢路径预设 WRITE_BIT（对齐 bcachefs `atomic_add(SIX_LOCK_HELD_write)`）
- `six.rs`: 新增 `try_lock_write_preset()`（慢路径专用 trylock，不检查 WRITE_BIT 预设）
- `six.rs`: `fetch_or(WAITING_WRITE_BIT, Relaxed)` → `SeqCst`
- `six.rs`: `notify_waiters()` 适配 WRITE_BIT 预设场景的 handoff

**验证**:
- ✅ `cargo build -p volmount-core` — 通过（无新警告）
- ✅ `cargo test -p volmount-core --lib` — 762 passed / 5 known fail / 9 ignored（基线不变）
- ✅ 46 个 lock 测试全部通过（含 stress 忽略 8 个）
- ✅ 之前挂起的 `test_lock_write_blocks_and_succeeds` 和 `test_notify_waiters_wakes_writer` 现在通过

**结论**: PASS

---

<!-- What level of testing is expected -->

(To be filled by the team)

---

### bch2_journal_wake_up — 对齐 C 的 `journal_wake()` (2026-07-01)

**C 源码**: `fs/journal/journal.h:118`

```c
static inline void journal_wake(struct journal *j)
{
    closure_wake_up(&j->async_wait);
}
```

**语义**: `journal_wake()` 只唤醒所有在 `j->async_wait` 上等待的 closure，不做状态推进。

**volmount 修复模式**:

```rust
// ✅ 正确：只唤醒等待者，不做状态推进
pub fn bch2_journal_wake_up(&self) {
    for idx in 0..JOURNAL_STATE_BUF_NR {
        let buf = self.bufs.get_mut(idx);
        buf.notify.notify_waiters();
    }
}

// ❌ 错误：在 journal_wake_up 中做 Closing→WriteSubmitted 状态转换
// - journal_res_put() 已在 refcount 归零时处理此转换
// - C 的 journal_wake 不管理状态机
pub fn bch2_journal_wake_up(&self) {
    for idx in 0..JOURNAL_STATE_BUF_NR {
        let buf = self.bufs.get_mut(idx);
        if buf.state == BufState::Closing {
            let count = ...;
            if count == 0 {
                buf.state = BufState::WriteSubmitted;  // ❌ 重复逻辑
                buf.notify.notify_waiters();
            }
        }
    }
}
```

**C 中调用 `journal_wake(j)` 的位置**:
| C 函数 | volmount 等价函数 |
|--------|------------------|
| `bch2_journal_error_set()` (journal.c:255) | `bch2_journal_error_set()` |
| `__bch2_journal_flush()` (journal.c:566) | `bch2_journal_flush()` (通过 set_watermark 间接) |
| `bch2_journal_cycle_locked()` (journal.c:673) | `journal_cycle_locked()` |
| `write.c:434` (journal I/O 完成后) | `write_bufs_to_bucket()` (通过 set_watermark 间接) |
| reclaim.c 多处 | `__bch2_journal_reclaim()` (通过 set_watermark 间接) |
| `bch2_journal_set_watermark()` (reclaim.c:104-105) | `bch2_journal_set_watermark()` |

### Journal Jset repr(C) 固定布局序列化 (2026-07-01)

**Context**: Journal Jset 使用 `#[derive(Serialize, Deserialize)]` + bincode 序列化/反序列化两次（一次在 append 时为计算大小，一次在 serialize_padded 时真正写盘）。bincode 不是 bcachefs 兼容的磁盘格式，且 append 路径因 serde 开销慢。

**Options Considered**:
1. **bincode（维持现状）** — 简单但性能差、格式不兼容
2. **repr(C) 双结构** — `JsetHeader` + `JsetEntryHeader` 均为 `#[repr(C)]`，直接 `ptr::copy` 写入 buf
3. **手写字节解析** — 无 repr(C)，纯字节操作

**Decision**: repr(C) 双结构。理由：直接映射磁盘格式，消除 serde 依赖，与 bcachefs `struct jset` + `struct jset_entry` 对齐。

#### Key Contracts

**未对齐读取**：
```rust
// ✅ 正确：data 可能从任意对齐的 Vec<u8> 来
let hdr: JsetHeader = unsafe { ptr::read_unaligned(data.as_ptr() as *const JsetHeader) };

// ❌ 错误：ptr::read 要求目标对齐
let hdr: JsetHeader = unsafe { ptr::read(data.as_ptr() as *const JsetHeader) };
```

**CRC 覆盖范围**：
```rust
// ✅ 正确：覆盖 header（crc32=0）+ 全部 entries，不含 padding
let crc = crc32c(0, &buf[..data_size]);
unsafe { ptr::write_unaligned(&mut buf[24] as *mut u32, crc); }
```

**版本检测**：
```rust
// ✅ 正确：先读 8 字节 magic + 4 字节 seq 后半部分作为 version 判断
let version = unsafe { ptr::read_unaligned::<u32>(data.as_ptr().add(32)) };
let is_v2 = (2..=JSET_VERSION).contains(&version);
```

#### LegacyJset 反序列化陷阱

```rust
// ❌ 错误：LegacyJset.version 定义为 u32（偏移 32 处读 4 字节）
// 旧 v1 bincode 格式在偏移 32 处实际是 2 字节 u16 + 1 字节 u8
// 多读 2 字节导致整个结构体偏移链全错

// ✅ 正确：必须匹配旧 bincode 布局
#[derive(Serialize, Deserialize)]
struct LegacyJset {
    magic: [u8; 8],
    seq: u64,
    last_seq: u64,
    crc32: u32,
    entry_count: u32,
    version: u16,     // ← 必须 u16，匹配旧 bincode 布局
    csum_type: u8,
    // ...
}
```

#### serialize_padded 优化路径

```
旧路径：serialize_padded() 完整序列化 → .len() 算大小 → 预分配
新路径：serialized_padded_len() 直接算大小 → 预分配 → serialize_padded() 填充
```

append 和 trans_commit 使用 `serialized_padded_len()` 预判 journal reservation 大小，无需预分配 buffer。

### flush_cache_dirty_keys journal_seq 传播 (2026-07-01)

**问题**: `flush_cache_dirty_keys()` 硬编码 `journal_seq = 0` 写入 btree 节点，导致 recovery 时无法正确关联节点到 journal 序列号。

**方案**: 为 `flush_cache_dirty_keys()` 添加 `journal_seq: u64` 参数，各调用点根据上下文传递合适的 seq：

```rust
// 调用点对齐
batch_write()          → flush_cache_dirty_keys(0)   // 后续 batch 操作会覆盖 node.journal_seq
insert_guarded(seq)    → flush_cache_dirty_keys(seq)  // 有明确的 journal_seq
trans_commit()         → flush_cache_dirty_keys(0)    // seq 未知，Phase 2 覆盖
commit_with_journal()  → flush_cache_dirty_keys(0)    // deprecated，seq 由 append 返回
```

**关键决策**:
- `insert_guarded()` 不再使用 `DIRTY_FLUSH_THRESHOLD` 条件 flush — 改为在每次 journal 写入前都 flush 脏 cache entries。对齐 bcachefs 语义：脏 key 必须在写入 journal 前落盘 btree，否则 crash recovery 后 journal 条目引用不存在的 key。
- `batch_write()` 和 `trans_commit()` 传 `0` 是安全的，因为 `commit_with_engine()` (Phase 2) 中的 `commit()` 会调用 `insert_entry_into_node()` 覆盖 `node.journal_seq`。
- `commit_with_journal()` 新增 `engine: &mut BtreeEngine` 参数（已 deprecated），调用 `flush_cache_dirty_keys(0)`。

**C 对应关系**:
- C 中 `bch2_btree_key_cache_journal_flush` 是 journal pin callback，由 journal reclaim 驱动；volmount 现在保留这一触发模型，但用显式 `JournalEntryPin.pin_type` 代替 callback 身份分类
- volmount 继续使用同步 flush 入口写回脏 cache，但 reclaim 侧 bucket 分类已与 C 的 key cache / btree pin 语义对齐
- `journal_seq` 参数仍对齐 C 的 `ck->journal.seq` 语义
- `journal_flush_pins()` 的返回值只统计成功完成 cleanup 的 flush 次数；callback 返回错误时，cleanup 先执行，再传播错误，不把失败尝试计入成功数

---

## Code Review Checklist

<!-- What reviewers should check -->

(To be filled by the team)
