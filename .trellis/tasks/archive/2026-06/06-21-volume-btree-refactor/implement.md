# Implement: Btree-Based Volume 重构（对齐 bcachefs）

## 概览

此文档按 Wave 分解具体实现步骤，包含文件路径、改动量、关键代码结构。
每个 Wave 可独立 dispatch 给 `trellis-implement` 子 agent。

### 审查修复日志

| # | 问题 | 修复 |
|---|------|------|
| R1 | Wave 1 改 Volume 在 Wave 3 被重写 | ✅ Wave 1 完全**不碰 Volume**，只创建 Journal 模块 + Superblock 增强 |
| R2 | BtreeEngine checkpoint→node 桥接不明确 | ✅ Wave 3 添加 `root_addrs` 分支逻辑和过渡路径 |
| R3 | Journal overflow 策略缺失 | ✅ Wave 1 添加 overflow 检测 + 容量上限 |
| R4 | bincode 反序列化兼容性 | ✅ 所有新 Superblock 字段用 `#[serde(default)]` |
| R5 | WAL 移除的前置条件未列出 | ✅ Wave 3 标记旧方法 `#[deprecated]`，Wave 4 预检 |
| R6 | DEFAULT_JOURNAL_BUCKETS 未定义 | ✅ 定义在 `journal/types.rs` |
| R7 | blacklist 在 Wave 1 不需要 | ✅ blacklist 字段移到 Wave 3 再加 |

---

## Wave 1 — Journal 子系统（不碰 Volume）

**目标**：新增 Journal 模块 + 更新 Superblock/StorageService。Volume 完全不改。
Journal 可通过独立单元测试验证。Volume 集成在 Wave 3 一次完成。

### 关键常量定义

```rust
// src/journal/types.rs 或 src/volume/config.rs

/// 预分配的 journal bucket 数量
/// Wave 1-2 期间 journal bucket 不回收，需足够避免 overflow。
/// 32 buckets × 256 blocks/bucket × 4KB/block = 32MB 元数据空间，
/// 每个 Jset ~1KB，约 8000 次事务写满。保守安全。
pub const DEFAULT_JOURNAL_BUCKETS: u32 = 32;
```

### 1.1 新文件：`src/journal/mod.rs`

```rust
//! Journal — Bcachefs 对齐的独立日志子系统
//!
//! Journal 是一组预分配的 bucket（循环缓冲区），
//! 每个 journal entry = Jset（含 btree update keys）。
//! 用作 crash recovery 的主机制。

mod jset;
mod replay;
mod types;

pub use jset::{Jset, JsetEntry, JsetEntryType};
pub use replay::JournalReplayer;
pub use types::{Journal, JournalError, DEFAULT_JOURNAL_BUCKETS};

use std::sync::Arc;
use crate::backend::StorageBackend;
use crate::btree::key::BtreeEntry;
use crate::btree::BtreeType;
use crate::types::StorageError;
```

### 1.2 新文件：`src/journal/types.rs`

```rust
/// Journal 错误
#[derive(Debug)]
pub enum JournalError {
    Overflow(String),    // journal 写满
    ChecksumMismatch,    // CRC32 不匹配
    Io(StorageError),
}

/// Journal 实例结构
pub struct Journal {
    /// 预分配的 journal bucket addrs
    pub buckets: Vec<u64>,
    /// 当前写入的 bucket 索引
    pub current_bucket: usize,
    /// 当前 bucket 内的偏移（字节）
    pub current_offset: u32,
    /// 当前 bucket 还剩多少可用字节
    pub remaining_bytes: u32,
    /// 最新分配的 seq
    pub last_seq: u64,
    /// 最老的未 flush seq（用于回收判定）
    pub last_seq_ondisk: u64,
    /// pending 但尚未 flush 的 entries
    pending: Vec<Jset>,
}

impl Journal {
    /// 创建新 Journal（从分配器获取 bucket）
    /// 不依赖 Volume 或 BtreeEngine。
    pub fn new(bucket_addrs: Vec<u64>) -> Self;
    
    /// 恢复 Journal（从 Superblock 加载状态）
    pub fn from_superblock(buckets: Vec<u64>, last_seq: u64, last_bucket: u32) -> Self;
    
    /// 分配一个新的 seq
    pub fn reserve_seq(&mut self) -> u64;
    
    /// 追加 btree update（insert/delete），写入当前 pending batch
    /// 返回 seq。如果 journal 剩余空间不足，返回 JournalError::Overflow。
    pub fn append(&mut self, btree_type: BtreeType, entries: &[BtreeEntry]) -> Result<u64, JournalError>;
    
    /// 追加 btree_root entry（记录 root 指针变化）
    pub fn append_btree_root(&mut self, btree_type: BtreeType, root_addr: u64) -> Result<u64, JournalError>;
    
    /// 将 pending entries 打包成 Jset 序列化到当前 bucket
    /// 写满当前 bucket 则轮换到下一个。如果所有 bucket 都满，
    /// 且最老的 bucket 包含未 flush 的 seq，返回 Overflow。
    async fn write_jset(&mut self, backend: &dyn StorageBackend) -> Result<(), JournalError>;
    
    /// flush pending entries 到 backend
    pub async fn flush(&mut self, backend: &dyn StorageBackend) -> Result<(), JournalError>;
    
    /// 返回当前写入率（0.0~1.0），1.0 = 满
    pub fn utilization(&self) -> f64;
    
    /// 读一个 journal bucket 的全部 entries（用于 replay）
    pub async fn read_bucket(&self, backend: &dyn StorageBackend, bucket_idx: u32) -> Result<Vec<Jset>, JournalError>;
    
    /// 遍历所有 journal bucket 的 entries（用于 replay）
    pub async fn iter_entries(&self, backend: &dyn StorageBackend) -> JournalIter<'_>;
}
```

**Overflow 策略**：
- `append` 检测当前 bucket 剩余空间 < 512 bytes → 尝试轮换到下一个 bucket
- 如果所有 bucket 都已使用且未回收 → 返回 `JournalError::Overflow`
- Wave 1-2 caller 收到 Overflow 应 flush btree（但无 btree flush 能力），所以 **应预分配足够 bucket 避免 Overflow**（32 buckets）
- Wave 3 后，Volume::close 和事务提交时调用 `flush` 和 `recycle_bucket`

### 1.3 新文件：`src/journal/jset.rs`

```rust
/// Journal entry — 对应 bcachefs `struct jset`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jset {
    pub magic: [u8; 8],           // JOURNAL_MAGIC
    pub seq: u64,                 // 递增 seq
    pub last_seq: u64,            // 最老未 flush seq
    pub crc32: u32,               // entries 的 CRC32
    pub entry_count: u32,
    pub entries: Vec<JsetEntry>,
}

/// Journal entry 中的单条记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsetEntry {
    pub btree_type: u8,           // BtreeType as u8
    pub entry_type: JsetEntryType,
    pub btree_keys: Vec<u8>,      // bincode: Vec<BtreeEntry>
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsetEntryType {
    BtreeKeys = 0,     // btree insert/delete keys
    BtreeRoot = 1,     // root pointer update
}

impl Jset {
    pub fn new(seq: u64, last_seq: u64) -> Self;
    pub fn verify(&self) -> bool;
    pub fn serialize_padded(&self) -> Result<Vec<u8>, StorageError>;
    pub fn deserialize(data: &[u8]) -> Result<Option<Self>, StorageError>;
}
```

### 1.4 新文件：`src/journal/replay.rs`

```rust
/// Journal 恢复器 — 遍历 journal bucket 并 replay entries
pub struct JournalReplayer<'a> {
    journal: &'a Journal,
    backend: &'a dyn StorageBackend,
}

impl JournalReplayer {
    pub async fn replay_from(&self, from_seq: u64) -> Result<Vec<ReplayedEntry>, StorageError>;
    pub async fn replay_all(&self) -> Result<Vec<ReplayedEntry>, StorageError>;
}

pub struct ReplayedEntry {
    pub seq: u64,
    pub btree_type: BtreeType,
    pub entry_type: JsetEntryType,
    pub btree_keys: Vec<BtreeEntry>,
}
```

### 1.5 修改：`src/storage/superblock.rs`

**新增字段**（注意：#[serde(default)] 保持向前兼容）：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Superblock {
    // ... 已有字段 magic, version, vol_meta ...
    
    // ─── Journal 位置（Wave 1 新增）───
    /// 预分配的 journal bucket addrs
    /// #[serde(default)] 确保旧格式 Volume 打开时默认全零
    #[serde(default)]
    pub journal_buckets: [u64; 32],
    #[serde(default)]
    pub journal_bucket_count: u32,
    #[serde(default)]
    pub journal_last_seq: u64,
    #[serde(default)]
    pub journal_last_bucket: u32,
    
    // ─── Btree roots（Wave 3 使用，Wave 1 预占位）───
    #[serde(default)]
    pub root_addrs: [u64; 64],
    #[serde(default)]
    pub root_levels: [u8; 64],
}
```

**注意**：
- 使用 `#[serde(default)]` 确保旧格式反序列化时新字段自动归零
- 这是 **Superblock 格式变更**，但因开发阶段无生产数据，可接受
- 旧 checkpoint 字段（`mapping_cp_addr` 等）**不变**
- 修改后检查 `SUPERBLOCK_SIZE (4096B)` 是否够用：现有 ~150B + 新增 journal (32*8+4+8+4=272B) + roots (64*8+64=576B) = ~998B，安全

**改 `Superblock::new`** — 初始化新字段。

### 1.6 修改：`src/storage/service.rs`

**添加** getter/setter（仅 journal root_addrs 相关，clean_shutdown 已有）：

```rust
impl StorageService {
    // New getters
    pub fn journal_buckets(&self) -> &[u64; 32];
    pub fn journal_bucket_count(&self) -> u32;
    pub fn journal_last_seq(&self) -> u64;
    pub fn journal_last_bucket(&self) -> u32;
    
    // New setters
    pub fn set_journal_buckets(&mut self, buckets: &[u64; 32]);
    pub fn set_journal_bucket_count(&mut self, n: u32);
    pub fn set_journal_last_seq(&mut self, seq: u64);
    pub fn set_journal_last_bucket(&mut self, idx: u32);
    
    // root_addrs（Wave 3 才使用，Wave 1 预加）
    pub fn root_addrs(&self) -> &[u64; 64];
    pub fn set_root_addr(&mut self, ty_index: usize, addr: u64);
}
```

### 1.7 修改：`src/lib.rs`

```rust
pub mod journal;  // 新增
```

### 1.8 测试（独立单元测试，不依赖 Volume）

```rust
// src/journal/tests.rs（在 jset.rs 或 types.rs 底部）

#[test]
fn test_jset_roundtrip() { /* 构造 Jset → serialize → deserialize → verify fields + crc32 */ }

#[test]
fn test_journal_append_seq_increment() { /* append N entries → verify seq=1,2,3 */ }

#[test]
fn test_journal_flush_readback() { /* flush → MockBackend 中 read → verify Jset 内容 */ }

#[test]
fn test_journal_overflow() { /* 设 1 个小 bucket → 写满 → 验证 Overflow error */ }

#[test]
fn test_journal_replay_all() { /* 写 3 个 Jset → replay → 验证所有 entries */ }

#[test]
fn test_journal_empty_replay() { /* 空 journal → replay → 空 Vec */ }
```

### Wave 1 验证点

- `cargo test` 全部通过（现有测试不变）
- `lsp_diagnostics` 无错误
- Journal 单元测试全部通过：构造 → append → flush → readback → replay
- Superblock 序列化前后兼容（旧格式 → deserialize → 新字段 = 0）
- `SUPERBLOCK_SIZE` 不溢出

---

## Wave 2 — Btree Node 持久化（接口定义 + I/O 层）

**目标**：定义磁盘上的 BtreeNodeHeader 格式，实现 bucket-level 的读写 I/O。
现有 BtreeEngine 不变（仍是纯内存）。Wave 2 只是加能力（plumbing），
Wave 3 才用起来（使用）。

### 2.1 修改：`src/btree/node.rs`

**新增 `BtreeNodeHeader` 和 `BtreeNodeDiskEntry`**（与现有内存 `BtreeNode` 结构共存）：

```rust
/// 磁盘 btree node header（写入每个 bucket 第一块，128B）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BtreeNodeHeader {
    pub magic: u32,              // BTREE_NODE_MAGIC
    pub version: u16,
    pub level: u8,
    pub node_type: u8,           // BtreeType as u8
    pub key_count: u32,
    pub bset_count: u16,
    pub crc32: u32,
    pub seq: u64,                // 最新 journal seq
    pub bucket_addr: u64,
    pub parent_addr: u64,
    pub _pad: [u8; 32],
}

/// 后续 append 的 bset entry 头（32B）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct BtreeNodeDiskEntry {
    pub magic: u32,
    pub seq: u64,
    pub bset_offset: u32,
    pub bset_size: u32,
    pub crc32: u32,
    pub _pad: [u8; 8],
}
```

**新增 `BtreeNode` 方法**（纯数据变换，不涉及 I/O）：

```rust
impl BtreeNode {
    /// 序列化当前 node 到字节数组（header + 所有 bset）
    pub fn serialize_to_bucket(&self, bucket_addr: u64) -> Vec<u8>;
    
    /// 从字节数组反序列化 BtreeNode（读取 header + 重建内存结构）
    pub fn deserialize_from_bucket(data: &[u8]) -> Result<Self, StorageError>;
    
    /// 将当前 node 的 key/values 打包成一个新 bset 的字节
    pub fn pack_bset(&self) -> Vec<u8>;
}
```

### 2.2 新文件：`src/btree/bucket_io.rs`

```rust
//! Bucket-level btree node I/O
//!
//! 管理 multi-block bucket（256 blocks = 1MB）的读写。
//! 当前实现使用 bucket 第一块（4KB）存 node 数据，
//! 剩余 blocks 预留供未来扩容（log-structured append）。

use crate::backend::StorageBackend;
use crate::btree::node::{BtreeNode, BtreeNodeHeader};

/// 从 backend 读取 bucket 的第一个 block（4KB）
pub async fn read_bucket_block(backend: &dyn StorageBackend, bucket_addr: u64) -> Result<Vec<u8>, StorageError>;

/// 加载 btree node（从 bucket first block 解析 header + bset）
pub async fn load_btree_node(backend: &dyn StorageBackend, bucket_addr: u64) -> Result<BtreeNode, StorageError>;

/// 写 btree node 到新 bucket（COW 写入 first block）
pub async fn write_node_to_bucket(
    node: &BtreeNode,
    bucket_addr: u64,
    backend: &dyn StorageBackend,
) -> Result<(), StorageError>;

/// 分配新 bucket + 写 btree node（COW 完整路径）
pub async fn allocate_and_write_node(
    node: &BtreeNode,
    allocator: &BlockAllocator,
    engine: &mut BtreeEngine,
    backend: &dyn StorageBackend,
) -> Result<u64, StorageError>;
```

### 2.3 修改：`src/btree/mod.rs`

添加 `pub mod bucket_io;`

### 2.4 测试（集成 MockBackend）

```rust
// src/btree/tests.rs 或 node.rs 底部

#[test]
fn test_btree_node_header_roundtrip() { /* header → ser/deser → verify */ }

#[tokio::test]
async fn test_bucket_io_roundtrip() {
    // 构造一个满 node，写 bucket，读回，验证所有 entries
    let backend = MockBackend::new();
    let node = build_filled_node();
    write_node_to_bucket(&node, 100, &backend).await.unwrap();
    let loaded = load_btree_node(&backend, 100).await.unwrap();
    assert_eq!(loaded.key_count, node.key_count);
    // verify entries match
}

#[test]
fn test_btree_node_serialize_roundtrip() {
    // 内存 node → serialize → deserialize → entries match
}
```

### Wave 2 验证点

- `cargo test` 全部通过（现有 btree 测试不受影响）
- BtreeNode 仍可纯内存工作（checkpoint 系列化不变）
- 磁盘格式 roundtrip 正确
- `MockBackend` 集成无问题

---

## Wave 3 — Volume 重构 + Journal 集成 + 事务

**目标**：Volume 集成 Journal。BufferEngine 从纯内存过渡到 node-based 持久化。
同时保留 checkpoint 路径作为 fallback。Volume::open 支持 crash recovery。

### 3.1 改：`src/volume/mod.rs` — Volume struct

```rust
pub struct Volume {
    // ... 已有字段（保留不变）：
    //   meta, volume_dir, backend, engine, mapping, snapshot_tree,
    //   root_snapshot_id, snapshot_manager, allocator, storage,
    //   trigger_registry, wal, config, is_open
    
    // NEW: Journal 实例
    pub journal: Journal,
    
    // NEW: btree root 指针（持久化到 Superblock）
    root_addrs: [u64; 64],
    root_levels: [u8; 64],
}
```

### 3.2 改：`src/volume/mod.rs` — Volume::create（已集成 Journal）

```rust
pub async fn create(
    backend: Arc<dyn StorageBackend>,
    volume_dir: impl AsRef<Path>,
    config: VolumeConfig,
) -> Result<Self, StorageError> {
    // ... 已有：目录创建, VolumeMeta, StorageService::create, allocator ...
    
    // NEW: 预分配 journal buckets
    let journal_addrs = allocator.allocate_buckets(DEFAULT_JOURNAL_BUCKETS, &mut engine)?;
    let journal = Journal::new(journal_addrs.clone());
    
    // NEW: 更新 Superblock journal 字段
    let mut jb = [0u64; 32];
    for (i, addr) in journal_addrs.iter().enumerate() {
        jb[i] = *addr;
    }
    storage.set_journal_buckets(&jb);
    storage.set_journal_bucket_count(journal_addrs.len() as u32);
    storage.set_journal_last_seq(0);
    storage.set_journal_last_bucket(0);
    storage.set_clean_shutdown(false);
    storage.close().await?;  // 写 Superblock
    
    // ... 继续：snapshot_tree, snapshot_manager, trigger_registry ...
    // ... journaled insert/delete 方法保留（新增 Journal 路径）...
    
    Ok(Volume {
        // ... 所有现有字段 ...
        journal,
        root_addrs: [0u64; 64],
        root_levels: [0u8; 64],
    })
}
```

### 3.3 改：`src/volume/mod.rs` — Volume::open（含 crash recovery）

```rust
pub async fn open(
    backend: Arc<dyn StorageBackend>,
    volume_dir: &Path,
) -> Result<Self, StorageError> {
    // 1. 读 Superblock
    let storage = StorageService::open(backend.clone()).await?;
    let sb = ...;  // 现有逻辑
    
    // 2. 初始化分配器（从 Alloc btree 恢复）
    // ... 现有 allocator 初始化 ...
    
    // 3. 加载 BtreeEngine
    // ── 过渡桥接分支 ──
    // root_addrs 全零 = 旧格式（checkpoint-based）或无持久化 node
    // 非全零 = 已有持久化 btree node
    
    let engine = if sb.root_addrs.iter().any(|&a| a != 0) {
        // PATH A: 有持久化 btree node，从 bucket 重建 engine
        // 对每个 btree type，若 root_addr != 0，从 bucket 加载 root node
        // 递归加载所有 child nodes（当前 Btree::from_root 恢复）
        Self::load_engine_from_roots(&sb.root_addrs, &backend)?
    } else {
        // PATH B: 旧格式（checkpoint-based）
        // 从 checkpoint 数据加载（现有路径）
        let mut engine = BtreeEngine::new();
        // ... 加载 checkpoint，recover_from_wal ...
        engine
    };
    
    // 4. Journal 初始化
    let jb = storage.journal_buckets();
    let journal_bucket_vec: Vec<u64> = jb.iter().copied().filter(|&a| a != 0).collect();
    let journal = if !journal_bucket_vec.is_empty() {
        Journal::from_superblock(
            journal_bucket_vec,
            storage.journal_last_seq(),
            storage.journal_last_bucket(),
        )
    };
    
    // 5. Crash recovery
    if !storage.clean_shutdown() {
        // Replay journal → 叠加到 engine
        let replayer = JournalReplayer { journal: &journal, backend: &*backend };
        let entries = replayer.replay_all().await?;
        for entry in entries {
            for btree_key in &entry.btree_keys {
                engine.insert_entry_raw(entry.btree_type, btree_key.clone());
            }
        }
    }
    
    // 6. 从 Alloc btree 恢复分配器状态
    allocator.load_from_btree(&engine)?;
    
    // 后续字段初始化同现有...
}
```

### 3.4 改：`src/volume/mod.rs` — Volume::close（双通道过渡）

```rust
pub async fn close(&mut self) -> Result<(), StorageError> {
    if !self.is_open { return Ok(()); }
    
    // ── Phase 1: 持久化 btree（第一次或增量）──
    // 将当前 engine 的所有 keys 序列化到 bucket-based btree node
    // 写入 journal btree_root entries（后续 crash recovery 用）
    //
    // 注意：这是过渡代码——遍历 engine 手动写 bucket
    // 未来 Btree 自身应支持 flush/load（PHASE 2）
    {
        for ty in BtreeType::ALL {
            let entries: Vec<BtreeEntry> = collect_btree_entries(&self.engine, ty);
            if entries.is_empty() { continue; }
            
            // 创建 leaf node 并写 bucket
            let mut node = BtreeNode::new_leaf();
            for e in &entries {
                node.insert_entry(e);
                // 如果满则 compact+retry，仍满则 split（简化：先原地 compact）
                if !node.insert_entry(e) {
                    node.compact();
                    node.insert_entry(e); // 假设 compact 后足够
                }
            }
            
            // 分配 bucket + 写 node
            let bucket_addr = allocate_and_write_node(&node, &self.allocator, &mut self.engine, &*self.backend).await?;
            self.root_addrs[ty.index()] = bucket_addr;
            self.root_levels[ty.index()] = 0; // leaf
            
            // 写 journal btree_root entry
            self.journal.append_btree_root(ty, bucket_addr)?;
        }
    }
    
    // ── Phase 2: flush journal ──
    self.journal.flush(&*self.backend).await?;
    
    // ── Phase 3: 写 Superblock（commit point）──
    // 同时写 checkpoint（保持旧路径兼容）
    let mapping_data = self.mapping.serialize()?;       // 旧路径
    let btree_data = self.engine.serialize_checkpoint_to_bytes(0)?; // 旧路径
    
    // 写 checkpoint（现有逻辑）
    if !mapping_data.is_empty() { ... }
    if !btree_data.is_empty() { ... }
    
    // 更新 journal 状态到 Superblock
    self.storage.set_journal_last_seq(self.journal.last_seq);
    self.storage.set_journal_last_bucket(self.journal.current_bucket as u32);
    self.storage.set_clean_shutdown(true);
    
    // 写 Superblock（新旧字段都写）
    self.storage.close().await?;
    
    // 关闭 WAL
    if let Some(wal) = self.wal.take() { wal.close().await.unwrap_or(()); }
    self.is_open = false;
    Ok(())
}
```

### 3.5 改：`src/btree/transaction.rs` — Journal 集成

```rust
impl BtreeTransaction {
    /// 追加操作到事务 journal（写入 journal 而非直接修改 engine）
    pub fn journal_insert(&mut self, ty: BtreeType, key: BtreeKey, value: BtreeValue);
    pub fn journal_delete(&mut self, ty: BtreeType, key: BtreeKey);
    
    /// 将事务提交到 Journal + Engine
    /// 先写 journal entry（commit point），再应用修改到 engine
    pub fn commit_journaled(
        &mut self,
        engine: &mut BtreeEngine,
        journal: &mut Journal,
    ) -> Result<(), StorageError> {
        let entries = self.collect_entries();
        journal.append(entries.ty, &entries.keys)?;   // commit point
        // 同步写入 engine
        self.commit_with_engine(engine)?;
        Ok(())
    }
}
```

### 3.6 改：`src/volume/mod.rs` — 标记旧方法 deprecated

```rust
#[deprecated(note = "use BtreeTransaction::commit_journaled instead")]
pub async fn btree_insert_journaled(&mut self, ...) { ... }

#[deprecated(note = "use BtreeTransaction::commit_journaled instead")]
pub async fn btree_delete_journaled(&mut self, ...) { ... }
```

现有调用方（write_extent, delete_extent, 测试）在 Wave 3 内改为新路径。
Wave 4 时移除 deprecated 方法 + 其调用方。

### 3.7 测试

```
test_wave3_clean_close_reopen: Volume::create → insert → close → reopen → keys found
test_wave3_crash_replay: Volume::create → insert → 不 close（drop）→ reopen → replay → keys found
test_wave3_engine_rebuild_from_roots: 写 node bucket → engine load from roots → entries match
test_wave3_transaction_journaled: BtreeTransaction + Journal → commit → flush → replay → verify
```

### Wave 3 验证点

- `cargo test` 全部通过
- Volume::create → insert → close → reopen（clean shutdown）→ 数据完整
- Volume::create → insert → crash（drop without close）→ reopen → journal replay → 数据完整
- root_addrs 在 close 后正确写入 Superblock，open 时正确读取
- Journal append 不导致 Overflow（32 buckets × 256 blocks = 足够空间）

---

## Wave 4 — 子系统迁移

**前置条件**：Wave 3 `btree_insert_journaled`/`btree_delete_journaled` 已标记 `#[deprecated]`，
所有内部调用已改为走 Journal 路径。Wave 4 将这些标记移除并完全切到新架构。

### 4.1 CowMapping → Extent btree

**改 `src/cow/mapping.rs`**：
- `insert(lba, paddr)` → 写 Extents btree
- `lookup(vaddr)` → 从 Extents btree 查
- `remove(vaddr)` → Extents btree delete
- `serialize()` / `deserialize()` → 移除（数据已在 btree）

CowMapping 作为适配层保留签名不变，内部存储改为 btree backend。

**改 `src/volume/mod.rs`**：
- Volume::open 不再从 checkpoint 加载 CowMapping
- Volume::close 不再写 mapping checkpoint
- `mapping: CowMapping` 字段改为 wrapper（可从 engine 构建）

**改 `src/storage/superblock.rs`**：
- `mapping_cp_addr` / `mapping_cp_len` 停止写入（保留字段但写 0）
- Wave 5 才移除字段

### 4.2 SnapshotManager → Snapshot btree

**改 `src/snap/manager.rs`**：
- `snapshots: HashMap<u64, SnapshotMeta>` → 读写 Snapshots btree
- `id_counter: u64` → 读写 SnapshotTrees btree 的 "snap_id_counter" key
- `open_snapshots: HashSet<u64>` → 内存状态（启动时从 btree 加载）
- `volume_dir` 文件路径依赖 → 移除
- `create_snapshot` 不再序列化 CowMapping（数据已通过 Journal 持久化）

**改 `src/volume/mod.rs`**：
- Volume::open 不再通过文件路径创建 SnapshotManager
- Volume::close 不再写 snap_index

### 4.3 移除 WAL

**改 `src/volume/mod.rs`**：
- 移除 `use crate::wal::*` 引用
- 移除 Volume struct 中的 `wal: Option<WalWriter>` 字段
- 移除 Volume::open 中的 `recover_from_wal` 调用
- 移除 Volume::close 中的 WAL flush/close 逻辑
- 移除 `btree_insert_journaled` / `btree_delete_journaled` 方法（deprecated 移除）
- 移除 `recover_from_wal` 方法
- 移除 `wal_dir` 目录创建（Volume::create 中）

**不改**（Wave 5）：
- `src/wal/` 目录不移除（Wave 5 清理）
- 测试中仍可能引用 wal 类型

### 4.4 测试

```
test_wave4_extent_backed_mapping: CowMapping CRUD → 验证写入的是 Extents btree
test_wave4_snapshot_btree_persistence: create snapshot → close → reopen → snapshot list 正确
test_wave4_no_wal_path: Volume create/open/close 完全不引用 WAL
test_wave4_cow_checkpoint_removed: close 后 Superblock mapping_cp_addr = 0
```

### Wave 4 验证点

- `cargo test` 全部通过
- CowMapping 功能不变（内部已切换到 btree）
- SnapshotManager 不依赖文件路径
- 无 WAL 引用（编译通过）
- CowMapping checkpoint 不再写入（Superblock 中 mapping_cp_addr = 0）

---

## Wave 5 — 清理

### 5.1 移除模块和代码

```
移除目录：
  src/wal/ — 不再需要（引用已全部移除）
  src/storage/block_io.rs — checkpoint I/O 不再使用

从 Superblock 移除字段：
  mapping_cp_addr, mapping_cp_len
  btree_cp_addr, btree_cp_len
  snap_index_addr, snap_index_len
  journal_seq（已由 journal_last_seq 替代）

从 lib.rs 移除模块：
  pub mod wal
```

### 5.2 代码清理

- `src/volume/mod.rs`：移除 `CHECKPOINT_FILE` / `BTREE_CHECKPOINT_FILE` / `META_FILE`
- `src/volume/mod.rs`：移除 `volume_dir` 相关 WAL、snaps 目录创建
- `src/volume/mod.rs`：简化 `VolumeConfig`（移除 `wal_prefix`）
- `src/volume/mod.rs`：Volume::close 简化（去掉 checkpoint 写入 + WAL 关闭）
- `src/volume/mod.rs`：Volume::open 简化（去掉 checkpoint 加载 + WAL 恢复）
- `src/storage/mod.rs`：移除 `block_io` 的 re-export
- `src/storage/service.rs`：移除 `write_checkpoint` / `read_checkpoint` 方法

### 5.3 最终验证

- `cargo test` 全部通过
- `cargo clippy` 无 warning（+#![allow(deprecated)] 清理）
- `lsp_diagnostics` 无错误
- 完整场景测试：create → write → close → reopen（clean + crash）→ data verified
- `src/wal/` 目录不存在
- `block_io.rs` 文件不存在

---

## 文件变更汇总（修正后）

| 文件 | Wave | 操作 | 改动量 |
|------|------|------|--------|
| `src/journal/mod.rs` | 1 | 创建 | ~30 lines |
| `src/journal/types.rs` | 1 | 创建 | ~150 lines |
| `src/journal/jset.rs` | 1 | 创建 | ~100 lines |
| `src/journal/replay.rs` | 1 | 创建 | ~80 lines |
| `src/storage/superblock.rs` | 1 | 修改（加 #[serde(default)] 字段） | ~25 lines |
| `src/storage/service.rs` | 1 | 修改（加 getter/setter） | ~30 lines |
| `src/lib.rs` | 1 | 修改（加 pub mod journal） | 1 line |
| `src/btree/node.rs` | 2 | 修改（加 Header/Entry 类型 + 方法） | ~100 lines |
| `src/btree/bucket_io.rs` | 2 | 创建 | ~80 lines |
| `src/btree/mod.rs` | 2 | 修改（加 bucket_io） | 1 line |
| `src/volume/mod.rs` | 3 | 大改（create/open/close + Journal 集成） | ~250 lines |
| `src/btree/transaction.rs` | 3 | 修改（commit_journaled 方法） | ~30 lines |
| `src/cow/mapping.rs` | 4 | 大改（btree-backed） | ~100 lines |
| `src/snap/manager.rs` | 4 | 大改（btree-backed） | ~120 lines |
| `src/volume/mod.rs` | 4 | 修改（移除 WAL 引用 + deprecated 方法） | ~50 lines |
| `src/storage/superblock.rs` | 4 | 修改（停止写旧 checkpoint 字段） | ~5 lines |
| `src/wal/` | 5 | 移除 | ~500 lines del |
| `src/storage/block_io.rs` | 5 | 移除 | ~50 lines del |
| `src/storage/mod.rs` | 5 | 修改（移除 block_io re-export） | 1 line |
| `src/storage/service.rs` | 5 | 修改（移除 checkpoint I/O） | ~30 lines |
| `src/storage/superblock.rs` | 5 | 修改（移除旧字段） | ~10 lines |
| `src/lib.rs` | 5 | 修改（移除 wal） | 1 line |
| `src/volume/mod.rs` | 5 | 修改（清理常量 + config） | ~20 lines |

---

## 已知风险与决策

| 风险 | 影响 | 缓解 |
|------|------|------|
| Wave 1-2 journal 永不回收 | 32 buckets ≈ 8000 次事务 | 设足够 bucket，短期安全 |
| BtreeEngine checkpoint→node 过渡代码冗余 | Wave 3 close 中手动遍历 engine 写 bucket | 标记为过渡代码，PHASE 2 重构 |
| bincode 超级块序列化兼容 | 旧 Volume 不可打开 | 开发阶段接受，serde(default) 减轻 |
| Wave 3 close 同时写 checkpoint + journal | 重复持久化 | 过渡期可接受，Wave 4 去除 checkpoint |
| WAL 在 Wave 3 仍存在 | 双日志系统 | Wave 4 移除 |
