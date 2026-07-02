# PRD: Volume 重构成 btree-based 架构（bcachefs 式）

## 背景

当前 Volume 架构存在多个独立子系统，各有各的持久化方式：

| 子系统 | 持久化方式 | 问题 |
|--------|-----------|------|
| CowMapping | 序列化到后端块（checkpoint）+ WAL 恢复 | 与 btree 分离，COW 逻辑重复 |
| BtreeEngine | 纯内存，checkpoint 全量序列化 | 不支持按需加载/换出 |
| StorageService | Superblock + checkpoint 元数据 | 额外抽象层，与 Volume 分配器冲突 |
| SnapshotManager | 文件系统目录 | 混合持久化后端 |
| WalWriter | 文件系统（WAL 文件） | 与 btree 内建 journal 重复 |

目标：一次性重构为 bcachefs 式架构——**一切持久状态通过 btree 表达，btree 节点直接存储在 backend 块上，空间由 BlockAllocator 管理**。

## 架构变更

### 核心原则

1. **全持久化 btree**：每个 btree node 对应一个 backend block。读时从 backend 加载，写时 COW 到新 block，写回 backend。
2. **无独立 WAL**：btree 操作内建 journal（intrinsic journal），crash safe 由 Superblock 的 root addr commit 保证。
3. **精简 StorageService**：只负责 Superblock 读/写，移除 checkpoint 和内部 allocator。
4. **无独立 CowMapping**：vaddr→paddr 映射通过 extent btree 表达。
5. **快照进 btree**：SnapshotManager 的状态存入 snapshot btree。
6. **同一分配器**：Volume 所有 block 分配通过唯一 BlockAllocator 管理。

### Btree 类型

Volume 管理多个 btree 实例：

| Btree 类型 | 内容 | Key/Value |
|-----------|------|-----------|
| **Root** (新增) | Volume 全局元数据 KV | path key → serialized value |
| **Extent** (替代 CowMapping) | vaddr→paddr + cow refcount | vaddr → extent record |
| **Alloc** (已有) | 块分配状态 | bucket addr → bucket state |
| **Snapshot** (已有，持久化增强) | 快照树 | snap_id → snapshot info |
| **Subvol** (已有) | 子卷信息 | subvol_id → subvol meta |

Superblock 存储每个 btree type 的 root node block addr。

### Btree Node 布局

```
[header 64B][entries...]
```

Header:
- `magic: u32` — 节点标识
- `version: u16` — 布局版本
- `level: u8` — 0=leaf, 1+=internal
- `node_type: u8` — btree type
- `key_count: u32` — 条目数
- `data_len: u32` — 数据区字节数
- `crc32: u32` — 数据校验
- `seq: u64` — journal sequence（crash recovery 用）
- `reserved: [u8; 44]` — 对齐到 128B header

### Journal / Crash Recovery

- 每次 btree 写操作（insert/delete）分配递增 seq
- 修改 node → 在新 block 写 node + seq → 更新 parent node 指针 → ...
- **commit point** = Superblock.root_addr[node_type] 指向新 root node
- Superblock 写总是最后一步（写完后 crash = 原子提交）
- 打开时：读 Superblock → 从 root node 递归加载 → 发现未提交的 node 分支则丢弃（seq 不连续）

### 分配器

BlockAllocator.allocate_bucket(engine) 已写入 Alloc btree，无需大改。
只改 btree node 分配策略：`allocate_bucket` 返回 256 块 bucket，btree node 使用第一块，剩余预留给 node split 用（类似 bcachefs 的 btree node prealloc）。

## 影响范围

### 移除的模块
- `src/storage/block_io.rs` — 不再需要 checkpoint I/O
- `src/mapping/` — CowMapping 整体移除
- `src/wal/` — 移除外置 WAL（journal 内建 btree）

### 精简的模块
- `src/storage/service.rs` — 移除内部 allocator、checkpoint I/O 方法
- `src/volume/mod.rs` — Volume struct 改为持有多个 BtreeInstance + allocator

### 大改的模块
- `src/btree/` — BtreeEngine 改为全持久化 node I/O
- `src/snap/manager.rs` — 快照状态进 snapshot btree
- `src/volume/mod.rs` — create/open/close 重写

## 验收标准

1. [ ] Volume::create 创建各 btree type，写 Superblock（含各 root addr）
2. [ ] Volume::open 读 Superblock，按需加载 btree nodes
3. [ ] Volume::close 刷写所有 dirty nodes，更新 Superblock
4. [ ] Btree 增删改直接分配 bucket 写 backend block，不依赖 checkpoint
5. [ ] CowMapping 功能由 extent btree 等效替代
6. [ ] 写操作的 crash safety 由 Superblock atomically 保证（无独立 WAL）
7. [ ] 所有现有测试在新架构下通过或明确标注不适用
