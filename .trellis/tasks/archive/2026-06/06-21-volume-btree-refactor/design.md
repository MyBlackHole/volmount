# Design: Btree-Based Volume（对齐 bcachefs）

## 概述

将 Volume 从「内存 btree + checkpoint 序列化」重构为 bcachefs 式架构：
**btree 是主存储结构，所有元数据通过 btree keys 表达**。Journal 是独立的 crash recovery 机制。

---

## 1. 核心架构

### 三大磁盘数据结构

```
┌────────────────────────────────────────────┐
│  Superblock                                │
│  - btree roots（每个 type 的 root addr）    │
│  - journal 位置                            │
│  - clean_shutdown flag                     │
│  - seq blacklist                           │
├────────────────────────────────────────────┤
│  Journal                                   │
│  - 预分配的 bucket 池                       │
│  - 记录 btree update keys 的 log            │
│  - commit point = journal entry 写完成      │
├────────────────────────────────────────────┤
│  Btrees（多个 type）                        │
│  - Extent / Root / Alloc / Snapshot / ...  │
│  - 每个 node = 一个 bucket（256 blocks）     │
│  - Log-structured（append-only within node）│
└────────────────────────────────────────────┘
```

### 操作流程

```
btree update commit flow（对齐 bcachefs）:

1. Transaction 收集 multi-btree update keys
2. 获取 journal reservation（分配 seq）
3. 按序锁 btree nodes（sorted order, deadlock avoidance）
4. 写 journal entry（含所有 keys）→ 提交点
5. 应用 updates 到 btree nodes（内存中）
6. 释放 locks
7. 异步：flush dirty nodes 到 backend
8. 异步：journal bucket 回收（当包含的 keys 已 flush）
```

### Crash Recovery

```
open():

1. 读 Superblock
2. if clean_shutdown:
   - 直接读 btree roots → 加载各 btree
   - 跳过 journal replay
3. if !clean_shutdown:
   - 从 Superblock 定位 journal
   - replay journal entries（re-insert keys into btrees）
   - 处理 seq blacklist（丢弃已写 node 但未写 journal 的更新）
4. 加载分配器状态（Alloc btree）
```

---

## 2. Btree Node 布局（对齐 bcachefs）

### 节点大小

每个 btree node 分配一个 bucket（当前 = 256 blocks × 4KB = 1MB）。
节点内是 log-structured 的：可以多次 append 写入。

### Node 内部结构

```
┌──────────────────────────────┐
│ struct BtreeNodeHeader (128B)│  ← 第一个 block 头部
├──────────────────────────────┤
│ bset 0 (sorted keys)         │  ← 第一次写入
├──────────────────────────────┤
│ struct BtreeNodeEntry (64B)  │  ← 后续 append 的头部
├──────────────────────────────┤
│ bset 1 (sorted keys)         │  ← 第二次写入
├──────────────────────────────┤
│ ...                          │
└──────────────────────────────┘
```

**bset** = sorted array of keys。每次 append 写一个新的 bset。
读时：搜索所有 bset（用 aux search tree 加速）。
写时：新 key 追加到最新 bset。
内存 compaction：当 bsets 太多时，merge 成一个。

### BtreeNodeHeader（第一个 block 的头部）

```rust
pub struct BtreeNodeHeader {
    pub magic: u32,              // BTREE_NODE_MAGIC
    pub version: u16,
    pub level: u8,               // 0=leaf, 1+=internal
    pub node_type: BtreeType,
    pub key_count: u32,          // 总 entry 数（所有 bset 之和）
    pub bset_count: u16,         // bset 数量
    pub crc32: u32,              // 节点数据校验
    pub seq: u64,                // 最新 journal seq
    pub bucket_addr: u64,        // 本 bucket 起始 addr
    pub parent_addr: u64,        // 父节点 addr（root=0）
    pub _pad: [u8; 32],          // 对齐到 128B
}
```

### BtreeNodeEntry（后续 append 的头部）

```rust
pub struct BtreeNodeEntry {
    pub magic: u32,              // BTREE_NODE_ENTRY_MAGIC
    pub seq: u64,                // 本 bset 的 journal seq
    pub bset_offset: u32,        // 本 bset 在 node 内的偏移
    pub bset_size: u32,          // 本 bset 的字节数
    pub crc32: u32,
    pub _pad: [u8; 12],
}
```

### 全量读 + Append-only 写

```
read_node(bucket_addr):
  1. 找到 bucket addr
  2. 读第一个 block → BtreeNodeHeader
  3. 遍历后续 blocks → 收集所有 BtreeNodeEntry + bsets
  4. 重建 aux search tree（每个 bset 的 lookup table）
  5. 内存中 merge 所有 bset 为 sorted index

append_node(bucket_addr, new_keys):
  1. 追加写入新的 bset（BtreeNodeEntry + sorted keys）
  2. 更新 BtreeNodeHeader.key_count + bset_count
  3. COW: 不 rewrite 已有数据

compact_node(bucket_addr, engine):
  1. 分配新 bucket
  2. merge 所有 bsets → 单个 bset
  3. 写入新 bucket
  4. 更新父节点指针 → new bucket_addr
```

---

## 3. Journal 子系统

### Journal 结构

Journal 是一组预分配的 bucket，构成一个循环缓冲区。
每个 journal entry = `Jset`（对应 bcachefs 的 `struct jset`）：

```rust
pub struct Jset {
    pub magic: u32,
    pub seq: u64,                // 递增 sequence number
    pub last_seq: u64,           // 最老的未 flush seq
    pub crc32: u32,
    pub entries: Vec<JsetEntry>, // 本 entry 包含的 keys
}

pub struct JsetEntry {
    pub btree_id: BtreeType,
    pub level: u8,
    pub entry_type: u8,          // btree_keys / btree_root / blacklist
    pub keys: Vec<BtreeEntry>,   // 序列化 key list
}
```

### Journal 分配

- 在 Volume::create 时预分配 N 个 bucket 作为 journal 池
- 位置记录在 Superblock 中
- 写满后轮换（回收已 flush 的 bucket）
- Journal bucket 的回收条件：该 bucket 中最老的 seq 对应的 btree updates 已 flush 到 backend

### Journal Replay

```
replay_journal():
  1. 从 Superblock 获取 journal bucket 列表 + last_seq
  2. 从 last_seq 开始，顺序读取 journal entries
  3. 对每个 entry：调用 btree_insert（re-insert keys）
  4. 完成后更新 Superblock（clean shutdown）
```

### Seq Blacklist

当 btree node 已写入 backend 但对应 journal entry 未写入磁盘时：
- btree node 的 seq > journal 的 last_seq
- 这些 seq 加入 blacklist（Superblock 中记录）
- 读取 btree node 时，跳过包含 blacklisted seq 的 bset

---

## 4. Superblock 增强

```rust
pub struct Superblock {
    pub magic: u32,
    pub version: u32,
    pub vol_meta: VolumeMeta,
    // Btree roots: type → (bucket addr, level)
    pub root_addrs: [u64; 64],
    pub root_levels: [u8; 64],
    // Journal 位置
    pub journal_buckets: [u64; 32],  // 预分配的 journal bucket addrs
    pub journal_bucket_count: u32,
    pub journal_last_seq: u64,       // 最近的 journal seq
    pub journal_last_bucket: u32,    // 当前 journal bucket index
    // Clean shutdown
    pub clean_shutdown: bool,
    // Seq blacklist（length-prefixed array）
    pub blacklist: Vec<SeqBlacklistEntry>,
    pub _pad: [u8; ...],
}

pub struct SeqBlacklistEntry {
    pub start: u64,
    pub end: u64,
}
```

---

## 5. Volume 重构后结构

```rust
pub struct Volume {
    backend: Arc<dyn StorageBackend>,
    allocator: BlockAllocator,
    sb: Superblock,
    // Journal
    journal: Journal,
    // Btree instances（每个 type 一个）
    btrees: [Option<BtreeInstance>; 64],
    // Transaction state
    trans: BtreeTransaction,
    config: VolumeConfig,
    is_open: bool,
}

pub struct BtreeInstance {
    node_type: BtreeType,
    root_addr: u64,
    root_level: u8,
    // In-memory node cache（dirty nodes pending flush）
    node_cache: HashMap<u64, CachedNode>,
}

pub struct CachedNode {
    node: BtreeNode,
    dirty: bool,
    seq: u64,
}

pub struct BtreeTransaction {
    updates: Vec<(BtreeType, BtreeEntry)>,
    locks: Vec<BtreeLock>,
}

/// Journal 实例
pub struct Journal {
    buckets: Vec<u64>,
    current_bucket: u32,
    current_offset: u32,
    last_seq: u64,
    pending: Vec<Jset>,
}
```

---

## 6. Create / Open / Close 流程

### Volume::create

```
1. 写 Superblock（初始化 root_addrs = 0, journal 位置）
2. 初始化 BlockAllocator（从 RESERVED_BLOCKS 之后）
3. 创建各 btree instance（无 root node，空）
4. 初始化 Journal（预分配 journal buckets）
5. 写 Superblock（含 journal 位置）
6. backend.flush()
```

### Volume::open

```
1. 读 Superblock
2. if clean_shutdown:
   - 对每个 btrees：加载 root node（root_addrs[type]）
   - 加载分配器状态（load_from_btree）
   - 跳过 journal replay
3. if !clean_shutdown:
   - replay_journal()
   - 处理 seq blacklist
   - 加载各 btree roots + 分配器
4. sb.clean_shutdown = false（mark dirty）
5. 更新 Superblock
```

### Volume::close

```
1. trans.commit() 确保 pending updates 全部提交
2. btree_transaction.flush():
   - 对每个 dirty node：写回 backend（分配新 bucket if COW）
   - 自底向上更新 parent 指针
   - 更新 root_addrs
3. 写 journal 的 btree_root entry（存最新 root 指针）
4. sb.clean_shutdown = true
5. 写 Superblock（commit point）
6. backend.flush()
```

---

## 7. 分配器集成

BlockAllocator 基本不变，关键接口：

```rust
// 分配 bucket（256 块连续空间）用于：
// - btree node 存储（全 bucket 或部分）
// - data extent 分配
allocator.allocate_bucket(engine) -> Result<u64>

// 释放 bucket
allocator.free(addr, engine) -> Result<()>

// 从 Alloc btree 恢复状态
allocator.load_from_btree(engine) -> Result<()>
```

Btree node 分配：`allocate_bucket` → 用 bucket 的前 N 块存 node 数据。
当前 node size = SUPERBLOCK_SIZE (4KB) 起步，后续可扩容到全 bucket。

---

## 8. 与 bcachefs 的对齐点

| 特性 | bcachefs | 本设计 |
|------|----------|--------|
| Btree 是主结构 | ✅ | ✅ |
| 多 btree type | extents/inodes/dirents/alloc... | extent/root/alloc/snapshot/subvol |
| Journal 独立 | ✅ | ✅ |
| Journal replay recovery | ✅ | ✅ |
| Btree node = bucket | ✅（128K-256K） | ✅（当前 256 blocks = 1MB） |
| Log-structured node（多 bset） | ✅ | ✅ |
| COW btree node | ✅ | ✅ |
| Seq blacklist | ✅ | ✅ |
| Clean shutdown skip replay | ✅ | ✅ |
| 事务层（multi-btree） | ✅（btree_trans） | ✅（BtreeTransaction） |
| 分配器 btrees | free/alloc/LRU | alloc only（阶段1） |
| Generation number | ✅ | ❌（阶段2） |
| Copy GC | ✅ | ❌（阶段2） |

---

## 9. 实施顺序（修正后）

### Wave 0: 分配器修复（当前已完成）
1. ✅ BlockAllocator start_block 参数（跳过 RESERVED_BLOCKS）
2. ✅ StorageService 去除内部 allocator（不再冲突）
3. ⬜ 当前代码可编译+基本测试通过

### Wave 1: Journal 子系统
1. Journal struct + Jset/JsetEntry 数据定义
2. Journal write（append entry 到预分配 bucket）
3. Journal read + replay
4. Superblock journal 字段集成
5. Volume::close 写 journal btree_root entry
6. Crash recovery path（!clean_shutdown → replay）

### Wave 2: Btree Node 持久化
1. BtreeNodeHeader + BtreeNodeEntry 序列化/反序列化
2. Bucket-level node read（多 block 读取）
3. Log-structured write（append bset）
4. Aux search tree（每 bset 的 lookup table）
5. BtreeInstance（node cache + dirty tracking）

### Wave 3: Volume 重构 + 事务
1. Volume::create → journal + btrees + superblock
2. Volume::open → clean/!clean shutdown path
3. Volume::close → flush + journal + superblock
4. BtreeTransaction（multi-btree atomic update）
5. COW node write + parent chain update

### Wave 4: 子系统迁移
1. CowMapping → Extent btree（保留 compat 层）
2. SnapshotManager → Snapshot btree
3. 移除 WAL（由 Journal 替代）
4. 移除 checkpoint I/O + StorageService allocator

### Wave 5: 清理
1. 移除 CowMapping / wal / block_io 模块
2. 全面测试通过
3. 更新 spec

---

## 10. 测试策略

- Journal 测试：create → insert → close → reopen → verify data（clean shutdown）
- Crash recovery 测试：insert → 模拟 crash → reopen → replay → verify data
- Multi-btree 事务测试：同时写 extent + root + snapshot → reopen → all consistent
- Btree node COW 测试：insert N keys → verify 每个 node 仅写一次（append）
- Blacklist 测试：构造 node seq > journal last_seq → reopen → blacklist detected
- 压力测试：10000 随机 key insert → close → reopen → verify 全量 match
