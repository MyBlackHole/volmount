# volmount-core 全局类型 API ↔ bcachefs 内核类型定义差距分析报告

> **审计目标**: 逐项对比 volmount-core 的 Rust 全局类型（types.rs + 公共导出类型）与 bcachefs 内核 C 类型定义的差异
> **审计范围**: `volmount-core/src/types.rs` 所有公共类型 + `src/lib.rs` 的 `pub use types::*` 导出 + `btree/mod.rs` 的 `BtreeId`
> **参考基准**: bcachefs-tools `fs/bcachefs_format.h` + `fs/bcachefs.h` + `fs/errcode.h` + `fs/alloc/types.h` + `fs/btree/types.h`
> **日期**: 2026-06-24
> **严重程度**: P1（正确性/兼容性）> P2（可维护性/命名）> P3（缺失功能）> P4（合理Rust化差异）

---

## 1. 摘要

| 维度 | 对齐（Aligned） | 微小偏差（Minor） | 重大差异（Major） | 缺失功能（Missing） |
|------|:-:|:-:|:-:|:-:|
| **volmount types.rs** | 1 | 2 | 5 | 3 |
| **BtreeId 枚举** | 0 | 0 | 1 | 0 |
| **Watermark 枚举** | 1 | 0 | 0 | 0 |
| **错误处理** | 0 | 1 | 1 | 1 |
| **命名/导出约定** | 0 | 2 | 1 | 0 |
| **合计** | 2 | 5 | 8 | 4 |

### 1.1 P1（必须修复）

| # | 项目 | 位置 | 影响 |
|---|------|------|------|
| P1-1 | `Bpos` 字段名 `vol_id` 语义不匹配 | `key.rs:132` | 跨卷 inode 引用丢失，`from_key()` 硬编码为 0 |
| P1-2 | `BKEY_NR_FIELDS=5` vs bcachefs 6 个字段 | `key.rs:38` | **磁盘格式不兼容**，缺少 SIZE/VERSION_HI/VERSION_LO |
| P1-3 | `BtreeId` 仅 6 个变体 vs bcachefs 28 个 | `btree/mod.rs:41` | 缺少 22 个标准 btree 类型，无法与 bcachefs 数据互操作 |
| P1-4 | `KeyType` 仅 3 个变体 vs bcachefs 38 个 | `key.rs:98` | 缺少 35 个 key 类型，卷装载/fsck 路径严重受限 |
| P1-5 | `StorageError` 缺少 bcachefs 错误分类层次 | `types.rs:53` | 缺少事务重启、journal 阻塞、bucket 分配等子系统错误类别 |

### 1.2 P2（建议修复）

| # | 项目 | 位置 | 影响 |
|---|------|------|------|
| P2-1 | `BlockAddr` 概念不对应 bcachefs 物理指针 | `types.rs:15` | bcachefs 使用 `bch_extent_ptr`(dev+offset+gen+csum)，volmount 用简单(raw+ver) |
| P2-2 | `BackendType{S3,Nfs}` 无 bcachefs 对应 | `types.rs:38` | bcachefs 设备类型是 bch_member(state, disk_group) 而非后端抽象 |
| P2-3 | `HealthStatus` 无 bcachefs 对应 | `types.rs:45` | bcachefs 通过 flags 和 superblock 状态追踪健康，无独立枚举 |
| P2-4 | `Capacity = u64` 类型别名 vs 隐式单位 | `types.rs:34` | bcachefs 使用 `bucket_bytes/block_bytes` 转换函数，无裸容量类型 |

### 1.3 P3（功能缺失）

| # | 描述 |
|---|------|
| P3-1 | 缺少 `bch_dev`（设备描述符）的 Rust 等价类型 |
| P3-2 | 缺少 `bch_fs`（全局文件系统）的 Rust 等价类型 |
| P3-3 | 缺少 `bkey`（完整 unpacked key 含 bversion/size）的内联结构 |
| P3-4 | 缺少 `struct bversion`（12 字节版本号）类型 |

### 1.4 P4（合理 Rust 化差异，无需修复）

| # | 项目 | 说明 |
|---|------|------|
| P4-1 | `types.rs` 作为独立模块 | bcachefs 类型分散在 30+ 头文件中，Rust 集中管理合理 |
| P4-2 | `Capacity`/`BlockSize` 为类型别名 | bcachefs 使用 `block_bytes()` 内联函数，效果等价 |
| P4-3 | `Watermark` 为 `#[repr(u8)]` 枚举 | bcachefs 使用 `enum bch_watermark`，完全对齐 |
| P4-4 | `pub use types::*` 全局导出 | bcachefs 通过包含 bcachefs_format.h 获得全局类型，效果等价 |
| P4-5 | `BlockAddr` 而非 `bch_extent_ptr` | volmount 为后端抽象存储，非块设备，概念不同 |

---

## 2. 类型定义对比

### 2.1 `types.rs` 全局类型（`lib.rs` 通过 `pub use types::*` 导出）

| Rust 类型/别名 | C 对应 | 状态 | 差距描述 |
|----------------|--------|:----:|----------|
| `VolumeId = u64` | `u64` (inode 号) / `u32` (subvol_id) | **⚠ 偏差** | 1. volmount 用 `VolumeId` 标识卷，bcachefs 用 `u64 inode` 标识文件 + `u32 subvol_id` 标识子卷。2. bcachefs 的 inode 号也是 u64 但语义不同——`VolumeId` 更接近 "device/node id" 而非文件 inode。 |
| `BlockAddr { raw: u64, ver: u16 }` | `struct bch_extent_ptr` | **⚠ 偏差** | 1. bcachefs 的 `bch_extent_ptr` 包含 `dev(u8)+offset(48bit)+gen(u32)+csum_hi/lo`，总计 16 字节。Volmount 仅 `raw(64bit)+ver(16bit)`，**缺少 dev 设备号、gen 代数、csum**。2. volmount 的 `BlockAddr` 是后端抽象层的"逻辑块地址"，bcachefs 的 `bch_extent_ptr` 是物理设备指针——概念层次不同。**P2-1** |
| `BlockSize = u32` | `unsigned block_bytes()` / `opts.block_size` | **✓ 对齐** | bcachefs 以 `opts.block_size` (默认 4096) 存储 block 大小，volmount 的 `BlockSize = u32` 足够。 |
| `Capacity = u64` | — | **⚠ 偏差** | bcachefs 无 `Capacity` 类型别名——通过 `bch_dev.mi.bucket_size * nbuckets` 计算。隐式单位是字节，但调用方需自己转换。**P2-4** |
| `BackendType { S3, Nfs }` | — | **⚠ 偏差** | **bcachefs 无此概念**。bcachefs 操作块设备（bch_dev），远程存储通过 tiering/rebalance 间接支持。`BackendType` 是 volmount 后端抽象层的概念，非 bcachefs 对齐目标。**P2-2** |
| `HealthStatus { Healthy, Degraded, Unreachable }` | — | **⚠ 偏差** | **bcachefs 无此类型**。bcachefs 通过 `bch_fs.flags`（BCH_FS_error/topology_error 等）+ `bch_dev.mi.state` 追踪健康状态。**P2-3** |
| `WATERMARK_NR = 7` | `BCH_WATERMARK_NR = 7` | **✓ 对齐** | 完全一致。bcachefs 在 `alloc/types.h:14-28` 定义了 7 个水位 + `BCH_WATERMARK_NR` 结尾哨兵。 |
| `Watermark` enum | `enum bch_watermark` | **✓ 对齐** | 见 2.2 详细对比 |
| `StorageError` enum | `enum bch_errcode` | **⚠ 偏差** | 见 3.1 详细对比 |

### 2.2 Watermark 枚举精细对比

| Rust 变体 | 值 | C 变体 | 值 | 状态 |
|-----------|:--:|--------|:--:|:----:|
| `Stripe` | 0 | `BCH_WATERMARK_stripe` | 0 | ✓ |
| `Normal` | 1 | `BCH_WATERMARK_normal` | 1 | ✓ |
| `CopyGC` | 2 | `BCH_WATERMARK_copygc` | 2 | ✓ |
| `Btree` | 3 | `BCH_WATERMARK_btree` | 3 | ✓ |
| `BtreeCopyGC` | 4 | `BCH_WATERMARK_btree_copygc` | 4 | ✓ |
| `Reclaim` | 5 | `BCH_WATERMARK_reclaim` | 5 | ✓ |
| `InteriorUpdate` | 6 | `BCH_WATERMARK_interior_updates` | 6 | **⚠ 命名** |

**命名偏差**: Rust 用 `InteriorUpdate`（无下划线 + 缩写），C 用 `interior_updates`（全小写 + 复数）。命名风格不同但语义等价。

**实现方法对比**:

| Rust 方法 | C 对应 | 对齐 |
|-----------|--------|:----:|
| `Watermark::BITS = 3` | `BCH_WATERMARK_BITS = 3` | ✓ |
| `Watermark::MASK = 0b111` | `BCH_WATERMARK_MASK = ~(~0U << 3)` | ✓ |
| `Watermark::from_bits(u8)` | `(flags & BCH_WATERMARK_MASK)` | ✓ |
| `Watermark::reserved_buckets(u64)` | `bch2_dev_buckets_reserved(ca, watermark)` | **⚠ 偏差** |
| `Watermark::allows(request)` | `(flags & WATERMARK_MASK) >= j->watermark` | ✓ |
| `Watermark::from_journal_utilization()` | `journal_space_available()` 动态调整 | **⚠ 简化** |
| `Watermark::from_alloc_utilization()` | （间接通过 alloc 预留逻辑） | **⚠ 简化** |

**reserved_buckets 差异**: Rust 的 `reserved_buckets` 使用 if-chain 模拟 fallthrough，bcachefs 使用 switch fallthrough（`buckets.h:229`）。Rust 版本缺少了 `BTREE_NEEDS_SCAN_NR` 等补充预留逻辑，且公式 `nb/32, nb/64, nb/128` 的除数不完全一致（C 中 stripe/normal 共用 nb/32，copygc 用 nb/64，btree 用 nb/128）。

---

## 3. 错误处理对比

### 3.1 `StorageError` vs `enum bch_errcode`

| Rust `StorageError` 变体 | C `bch_errcode` 对应 | 状态 | 差距描述 |
|--------------------------|----------------------|:----:|----------|
| `Io(std::io::Error)` | `BCH_ERR_blockdev_io_error` 族 | **⚠ 偏差** | Rust 将 IO 错误映射为 `std::io::Error`（`thiserror#[from]`），bcachefs 细分了 `blockdev_io_error` + `BLK_STS_*`（20 个亚类）+ `device_offline` 等。Rust 丢失了细粒度分类。 |
| `BlockNotFound(BlockAddr)` | `ENOENT_bkey_type_mismatch` / `ENOENT_dev_not_found` | **⚠ 偏差** | bcachefs 的"未找到"按场景细分（bkey type/dev/inode/subvol/dirent/snapshot 等），Rust 合并为一个变体。 |
| `ChecksumMismatch { u32, u32 }` | `BCH_ERR_data_read_csum_err` | **⚠ 偏差** | bcachefs 将校验和错误分为 `data_read_csum_err`、`stripe_read_csum_err`、`btree_node_validate_err` 等。Rust 丢失了错误来源信息。 |
| `Unreachable(String)` | `EIO_device_offline` / `EROFS_insufficient_devices` | **⚠ 偏差** | bcachefs 区分设备离线、文件系统只读、前端不可修复等场景。Rust 用一个变体覆盖所有。 |
| `InvalidBlockSize(u64)` | `EINVAL_block_size_too_small` / `EINVAL_mismatched_block_size` | **⚠ 偏差** | bcachefs 细分了 `block_size_too_small` / `mismatched_block_size`。Rust 合并。 |
| `VolumeNotFound(VolumeId)` | `ENOENT_inode` / `ENOENT_subvolume` | **⚠ 偏差** | bcachefs 区分 inode 未找到和 subvol 未找到。 |
| `Serialization(bincode::Error)` | — | **⚠ Rust 特有** | bcachefs 使用 `bch2_bkey_unpack` 错误（通过 `EINVAL` 返回）。序列化错误概念不同。 |
| `NotFound(String)` | 多个 `ENOENT_*` 细分 | **⚠ 偏差** | Rust 通用 `NotFound(String)` 是 catch-all，bcachefs 细分了 10+ 种未找到场景。 |
| `AddressSpaceExhausted { u64 }` | `ENOSPC_bucket_alloc` / `ENOSPC_disk_reservation` | **⚠ 偏差** | bcachefs 区分 bucket 耗尽、磁盘预留耗尽、stripe 创建失败等。 |
| `TransactionLockConflict(Bpos)` | `BCH_ERR_transaction_restart_relock` | **⚠ 偏差** | bcachefs 细分了 20+ 种事务重启原因（relock, deadlock, upgrade, key_cache, split_race 等）。Rust 合并为一个。 |
| `TransactionRestartLimit(u64)` | `BCH_ERR_transaction_restart` 亚类 | **⚠ 偏差** | bcachefs 通过重启计数+重试限制实现，但更细致（`max_iters` 等）。 |
| `Transaction(String)` | 多个 `transaction_restart_*` | **⚠ 偏差** | 通用 catch-all，丢失了重启类型信息。 |
| `JournalError(String)` | `BCH_ERR_journal_*` 族（15+ 细分） | **⚠ 偏差** | bcachefs 细分了 `journal_full`, `journal_pin_full`, `journal_buf_enomem`, `journal_stuck`, `journal_retry_open` 等。 |
| `WatermarkTooLow { Watermark, Watermark }` | `BCH_ERR_operation_blocked` 族 | **⚠ 偏差** | bcachefs 使用 `operation_blocked` + 亚类（`journal_res_blocked`, `bucket_alloc_blocked`, `open_bucket_alloc_blocked`），水位线错误是间接的。 |

### 3.2 错误分类层次对比

**bcachefs 错误分类（`errcode.h`）**:
```
BCH_ERR_START = 2048
  ├── BCH_ERR_blockdev_io_error
  │     ├── BLK_STS_* (20 个)
  │     └── device_offline, ...
  ├── BCH_ERR_zstd_error
  │     └── ZSTD_error_* (20 个)
  ├── BCH_ERR_transaction_restart
  │     ├── transaction_restart_relock, relock_path, upgrade, ...
  │     └── (20+ 亚类)
  ├── BCH_ERR_no_btree_node
  │     └── (10+ 亚类)
  ├── BCH_ERR_operation_blocked
  │     ├── journal_res_blocked → journal_full, journal_pin_full, ...
  │     ├── bucket_alloc_blocked
  │     └── (10+ 亚类)
  ├── BCH_ERR_invalid
  │     └── invalid_sb → invalid_sb_magic, invalid_sb_version, ... (30+ 亚类)
  ├── BCH_ERR_fsck → fsck_fix, fsck_ask, ...
  ├── BCH_ERR_data_read → data_read_csum_err, data_read_retry, ...
  ├── ENOMEM_* (50+)
  ├── ENOSPC_* (15+)
  ├── ENOENT_* (15+)
  ├── EINVAL_* (100+)
  └── EROFS_* (12+)
```

**volmount 错误分类（`types.rs`）**:
```
StorageError (thiserror::Error)
  ├── Io(io::Error)
  ├── BlockNotFound(BlockAddr)
  ├── ChecksumMismatch { expected, actual }
  ├── Unreachable(String)
  ├── InvalidBlockSize(u64)
  ├── VolumeNotFound(VolumeId)
  ├── Serialization(bincode::Error)
  ├── NotFound(String)                    ← catch-all
  ├── AddressSpaceExhausted { u64 }
  ├── TransactionLockConflict(Bpos)
  ├── TransactionRestartLimit(u64)
  ├── Transaction(String)                 ← catch-all
  ├── JournalError(String)                ← catch-all
  └── WatermarkTooLow { Watermark, Watermark }
```

**关键差异**:
1. **层次深度**: bcachefs 使用两级层次（错误类→亚类）允许 `bch2_err_matches(err, BCH_ERR_transaction_restart)` 分类匹配。Rust 为扁平的 15 变体枚举，无法做按类匹配。
2. **错误数量**: bcachefs 定义了约 300+ 个错误代码，volmount 定义了 14 个。
3. **事务重启**: bcachefs 将 `transaction_restart` 设计为"成功"（值为 0，不打印错误），volmount 将其作为实际的 `StorageError::TransactionRestartLimit` 错误。**语义差异**——bcachefs 的事务重启不是错误，是正常控制流。

### 3.3 推荐行动

**P1**: 添加 `TransactionRestart` 作为"非错误"变体（值为 `0`），或拆分 `StorageError` 为"可恢复错误"和"系统错误"两层。

**P2**: 实现 `bch2_err_matches()` 等价功能——通过 `StorageError` 的位域或嵌套枚举支持错误类匹配。

---

## 4. BtreeId 对比

### 4.1 volmount `BtreeId` vs bcachefs `enum btree_id`

| Rust `BtreeId` (volmount) | C `btree_id` (bcachefs) | idx | 状态 |
|---------------------------|------------------------|:---:|:----:|
| `Extents` | `BTREE_ID_extents` | 0 | ✓ |
| — | `BTREE_ID_inodes` | 1 | **❌ 缺失** |
| — | `BTREE_ID_dirents` | 2 | **❌ 缺失** |
| — | `BTREE_ID_xattrs` | 3 | **❌ 缺失** |
| `Alloc` | `BTREE_ID_alloc` | 4 | ✓ |
| — | `BTREE_ID_quotas` | 5 | **❌ 缺失** |
| — | `BTREE_ID_stripes` | 6 | **❌ 缺失** |
| — | `BTREE_ID_reflink` | 7 | **❌ 缺失** |
| `Subvolumes` | `BTREE_ID_subvolumes` | 8 | ✓ |
| `Snapshots` | `BTREE_ID_snapshots` | 9 | ✓ |
| — | `BTREE_ID_lru` | 10 | **❌ 缺失** |
| `Freespace` | `BTREE_ID_freespace` | 11 | ✓ |
| — | `BTREE_ID_need_discard` | 12 | **❌ 缺失** |
| — | `BTREE_ID_backpointers` | 13 | **❌ 缺失** |
| — | `BTREE_ID_bucket_gens` | 14 | **❌ 缺失** |
| `SnapshotTrees` | `BTREE_ID_snapshot_trees` | 15 | ✓ |
| — | `BTREE_ID_deleted_inodes` | 16 | **❌ 缺失** |
| — | `BTREE_ID_logged_ops` | 17 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_work` | 18 | **❌ 缺失** |
| — | `BTREE_ID_subvolume_children` | 19 | **❌ 缺失** |
| — | `BTREE_ID_accounting` | 20 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_hipri` | 21 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_pending` | 22 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_scan` | 23 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_work_phys` | 24 | **❌ 缺失** |
| — | `BTREE_ID_reconcile_hipri_phys` | 25 | **❌ 缺失** |
| — | `BTREE_ID_bucket_to_stripe` | 26 | **❌ 缺失** |
| — | `BTREE_ID_stripe_backpointers` | 27 | **❌ 缺失** |

**统计数据**: volmount 覆盖 6/28（21%），缺失 22 个 btree 类型。

### 4.2 BtreeId 索引对齐

volmount 的 `BtreeId` 不使用 bcachefs 的数值索引：
- bcachefs: `extents=0, inodes=1, dirents=2, xattrs=3, alloc=4, ...`
- volmount: `Extents=0, Subvolumes=1, Snapshots=2, SnapshotTrees=3, Alloc=4, Freespace=5`

**这意味着 volmount `BtreeId` 不能直接映射到 bcachefs 的磁盘格式索引**。如果 volmount 需要读取 bcachefs 的 btree 数据，索引映射必须单独处理。

### 4.3 BtreeId 标志位缺失

bcachefs 的每个 btree_id 携带 `enum btree_id_flags`（`bcachefs_format.h:652-658`）：
```c
BTREE_IS_extents      = BIT(0),  // extent btree（支持 extent key 合并）
BTREE_IS_snapshots    = BIT(1),  // 快照可见性过滤
BTREE_IS_snapshot_field = BIT(2), // btree 有快照字段
BTREE_IS_data         = BIT(3),  // 包含数据指针（影响 GC）
BTREE_IS_write_buffer = BIT(4),  // 使用 write buffer
```

volmount 的 `BtreeId` 通过 `impl` 方法（`name()`, `index()` 等）提供少量元数据，但**缺少等效的标志位系统**。例如，无法确定某个 btree 是否支持快照过滤或使用 write buffer。

### 4.4 推荐行动

**P1-3**: 将 `BtreeId` 扩展为完整的 28 变体枚举，或至少添加 bcachefs 核心的 `inodes`, `dirents`, `xattrs`, `quotas`, `stripes`, `reflink`，并使索引对齐。

**P2**: 添加 `BtreeId` 标志位支持（类似 `BTREE_IS_extents` 等），或用位域方法 `BtreeId::flags()`。

---

## 5. KeyType 对比

### 5.1 volmount `KeyType` vs bcachefs `enum bch_bkey_type`

| Rust `KeyType` | 值 | C `bch_bkey_type` | 值 | 状态 |
|----------------|:--:|-------------------|:--:|:----:|
| `Normal` | 0 | — | — | **⚠ 偏差** |
| `Deleted` | 1 | `KEY_TYPE_deleted` | 0 | **⚠ 值不同** |
| `Whiteout` | 2 | `KEY_TYPE_whiteout` | 1 | **⚠ 值不同** |
| — | — | `KEY_TYPE_error` | 2 | **❌ 缺失** |
| — | — | `KEY_TYPE_cookie` | 3 | **❌ 缺失** |
| — | — | `KEY_TYPE_hash_whiteout` | 4 | **❌ 缺失** |
| — | — | `KEY_TYPE_btree_ptr` | 5 | **❌ 缺失** |
| — | — | `KEY_TYPE_extent` | 6 | **❌ 缺失** |
| ... | — | ... (31 个更多) | 7-37 | **❌ 缺失** |

**关键问题**:
1. **索引偏移**: volmount 的 `Normal=0` 在 bcachefs 中是 `KEY_TYPE_deleted`。这意味着 volmount 的默认 key type 在 bcachefs 看来是"删除"。
2. **覆盖度**: 6 / 38 类型（16%），缺少 `extent`, `inode`, `dirent`, `xattr`, `alloc`, `quota`, `stripe`, `reflink`, `backpointer`, `accounting` 等。
3. **Rust 特有**: `Normal` 是 volmount 自创概念——bcachefs 无此类型。bcachefs 的默认 key type 取决于 btree（extents btree 用 `KEY_TYPE_extent`）。

### 5.2 推荐行动

**P1-4**: 重新排列 `KeyType` 枚举值与 bcachefs 对齐（从 0 开始），并至少添加 `extent`, `inode_v3`, `dirent`, `alloc_v4`, `backpointer`, `accounting` 等核心类型。

---

## 6. BKEY 格式对比

### 6.1 磁盘 key 字段

| Field index | bcachefs C | Rust | 状态 |
|:-----------:|-----------|------|:----:|
| 0 | `INODE` (64bit) | `BKEY_FIELD_INODE` (64bit) | ✓ |
| 1 | `OFFSET` (64bit) | `BKEY_FIELD_OFFSET` (64bit) | ✓ |
| 2 | `SNAPSHOT` (32bit) | `BKEY_FIELD_SNAPSHOT` (32bit) | ✓ |
| 3 | `SIZE` (32bit) | `BKEY_FIELD_PADDR` (48bit) | **✗ 不同** |
| 4 | `VERSION_HI` (32bit) | `BKEY_FIELD_VER` (16bit) | **✗ 不同** |
| 5 | `VERSION_LO` (64bit) | — (缺失) | **✗ 缺失** |

```rust
// volmount BKEY_FIELD_BITS
pub const BKEY_FIELD_BITS: [u8; 5] = [64, 64, 32, 48, 16];
// bcachefs BKEY_FORMAT_CURRENT bits_per_field
// [64, 64, 32, 32, 32, 64] = 6 fields, 288 bits
```

**结果**: volmount 的 packed key 有 5 fields × 224 bits，bcachefs 有 6 fields × 288 bits。**格式不兼容**（P1-2）。

### 6.2 BKEY_U64S

```rust
// volmount
pub const BKEY_U64S: u8 = 3;   // 24 bytes = 3 × 8
pub const BKEY_HEADER_BYTES: u32 = 3;

// bcachefs
#define BKEY_U64s (sizeof(struct bkey) / sizeof(__u64))  // = 5 (40 bytes)
```

volmount 的 `BKEY_U64S = 3` 意味着它认为 unpacked key 只有 24 字节。bcachefs 的 `BKEY_U64s = 5`（含完整的 bversion/size/bpos）= 40 字节。

---

## 7. 命名约定对比

### 7.1 类型命名

| 概念 | bcachefs C | volmount Rust | 状态 |
|------|-----------|---------------|:----:|
| 物理块地址 | `bch_extent_ptr` | `BlockAddr` | ⚠ 概念不同 |
| 卷 ID | `u64 inode` / `u32 subvol_id` | `VolumeId = u64` | ⚠ 语义不同 |
| 块大小 | `block_bytes()` 函数 | `BlockSize = u32` | ✓ 合理抽象 |
| 容量 | 计算属性 | `Capacity = u64` | ✓ 合理抽象 |
| 后端类型 | —（无此概念） | `BackendType` | ⚠ Rust 特有 |
| 健康状态 | flags + state | `HealthStatus` | ⚠ Rust 特有 |
| 存储错误 | `enum bch_errcode` | `StorageError` | ⚠ 命名合理，但结构不同 |
| 水位线 | `enum bch_watermark` | `Watermark` | ✓ 简洁合理 |
| btree ID | `enum btree_id` | `BtreeId` | ✓ 命名一致 |
| key 类型 | `enum bch_bkey_type` | `KeyType` | ✓ 简洁合理 |
| 搜索位置 | `struct bpos` | `Bpos` | ✓ 命名一致 |
| 打包 key | `struct bkey_packed` | `BkeyPacked` | ✓ 命名一致 |
| 键格式 | `struct bkey_format` | `BkeyFormat` | ✓ 命名一致 |

### 7.2 函数/方法命名

| volmount | bcachefs C | 状态 |
|----------|-----------|:----:|
| `Watermark::from_bits()` | 内联 `(flags & MASK)` | ✓ 合理封装 |
| `Watermark::reserved_buckets()` | `bch2_dev_buckets_reserved()` | ⚠ 移除了 `dev_` 前缀，合理 |
| `Watermark::allows()` | `>= j->watermark` | ✓ 合理封装 |
| `BlockAddr::new()` / `with_ver()` | `bch_extent_ptr` 内联初始化 | ✓ 合理 Rust 化 |
| `BtreeId::name()` | `__bch2_btree_ids[]` 字符串数组 | ✓ 等价 |

### 7.3 常量命名

| volmount | bcachefs C | 状态 |
|----------|-----------|:----:|
| `WATERMARK_NR` | `BCH_WATERMARK_NR` | ⚠ 缺少 `BCH_` 前缀 |
| `BKEY_NR_FIELDS` | `BKEY_NR_FIELDS` | ✓ |
| `BKEY_FIELD_INODE` | `BKEY_FIELD_INODE` | ✓ |
| `BKEY_FIELD_OFFSET` | `BKEY_FIELD_OFFSET` | ✓ |
| `BKEY_FIELD_SNAPSHOT` | `BKEY_FIELD_SNAPSHOT` | ✓ |
| `BKEY_FIELD_PADDR` | —（C 无此字段） | ⚠ Rust 特有 |
| `BKEY_FIELD_VER` | —（C 分为 VER_HI + VER_LO） | ⚠ Rust 合并 |
| `KEY_FORMAT_CURRENT` | `KEY_FORMAT_CURRENT` | ✓ |
| `KEY_FORMAT_LOCAL_BTREE` | `KEY_FORMAT_LOCAL_BTREE` | ✓ |
| `KEY_PACKED_BITS_START` | `KEY_PACKED_BITS_START` | ✓ |
| `BKEY_U64S` | `BKEY_U64s` | ⚠ 大小写（`U64S` vs `U64s`） |
| `BKEY_HEADER_BYTES` | —（内联 `sizeof`） | ⚠ Rust 显式定义 |

### 7.4 前缀约定

bcachefs 使用三层前缀系统：
1. `BCH_` — 文件系统级（`BCH_ERR_*`, `BCH_WATERMARK_*`, `BCH_FS_*`）
2. `BKEY_` — key 格式级（`BKEY_FIELD_*`, `BKEY_U64s`）
3. `BTREE_` — btree 操作级（`BTREE_ID_*`, `BTREE_MAX_DEPTH`）
4. `KEY_TYPE_` — key 类型枚举

volmount 的对应：
1. (无 `BCH_` 前缀) — ⚠ 缺失
2. `BKEY_*` — ✓ 一致
3. `BTREE_*` — ✓ 部分一致
4. `KeyType::` — ✓ 合理 Rust 化（typesafe enum 替代前缀）

**影响**: volmount 缺少 `BCH_` 前缀的常量（如 `BCH_FS_flags`、`BCH_WATERMARK_*` 等对应关系）。如果未来需要磁盘互操作，需要添加 `BCH_` 前缀常量映射。

---

## 8. 导出策略对比

### 8.1 Include 层次

**bcachefs C**（`bcachefs.h` 包含约 40 个头文件）：
```
bcachefs.h (master header)
  ├── bcachefs_format.h (所有磁盘格式类型: bpos, bkey, bkey_packed, btree_id...)
  ├── errcode.h (错误代码)
  ├── opts.h (挂载选项枚举)
  ├── alloc/types.h (BCH_WATERMARKS, write_point...)
  ├── btree/types.h (btree_iter, btree_trans, btree_cache...)
  ├── journal/types.h
  ├── snapshots/types.h
  └── ...
```

**volmount Rust**（`lib.rs` 导出约 10 个模块）：
```
lib.rs
  ├── types (→ Watermark, StorageError, BlockAddr, VolumeId, BlockSize, Capacity, BackendType, HealthStatus)
  ├── btree (→ BtreeId, BtreeEngine, BtreeIter, BtreeTrans, TriggerRegistry, Bpos, BtreeKey, BchVal, KeyType...)
  ├── alloc
  ├── journal
  ├── snap → snapshot
  ├── subvol
  ├── volume
  ├── lock
  ├── recovery
  ├── storage
  ├── cache
  ├── config
  ├── block_device
  └── meta
```

### 8.2 关键差异

| 维度 | bcachefs C | volmount Rust | 评估 |
|------|-----------|---------------|:----:|
| 全局类型集中管理 | 分散在 30+ 头文件 | 集中在 `types.rs` | ✓ 合理精简 |
| 磁盘格式在单独头文件 | `bcachefs_format.h` | 在 `btree/key.rs` 中 | ⚠ 混合内存与磁盘类型 |
| `*_types.h` 模式 | 广泛使用（预声明类型） | 无此模式 | ⚠ 缺少前向声明 |
| 公共 vs 内部可见性 | 头文件选择公开 | `pub` / `pub(crate)` | ✓ Rust 原生 |
| 循环依赖处理 | 前向声明 + `*_types.h` | 模块树 + `pub use` | ✓ 架构不同但有效 |
| 条件编译类型 | `#ifdef __KERNEL__` | `#[cfg(test)]` 等 | ✓ Rust 原生 |

### 8.3 推荐行动

- **P2**: 将磁盘格式类型（`Bpos`, `BkeyFormat`, `BkeyPacked`, `BtreeNodeHeader`, `BtreeNodeDiskEntry`）从 `btree/key.rs` 和 `btree/node.rs` 独立到类似 `bcachefs_format.rs` 的模块，保持与 `types.rs` 的区分。

---

## 9. 缺失类型清单（bcachefs 有，volmount 无）

### 9.1 核心磁盘格式类型（P3）

| 缺失类型 | bcachefs 定义 | 影响 |
|----------|-------------|------|
| `struct bversion` | `bcachefs_format.h:196-208` | 缺少 12 字节版本号（lo=u64 + hi=u32），无法支持快照间 key 版本比较 |
| `struct bkey`（完整） | `bcachefs_format.h:211-276` | 缺少 unpacked key 完整结构（bversion，size，3 字节 header + pad） |
| `struct bkey_i` | `bcachefs_format.h:363-368` | 缺少带内联值的 key 结构 |
| `struct bch_val` | `bcachefs_format.h:192-194` | 缺少 value 基类型（零长数组），已在 `BchVal` 间接对应 |
| `struct bch_extent_ptr` | `extents_format.h` | 缺少物理设备指针（dev + offset + gen + csum） |
| `enum btree_id_flags` | `bcachefs_format.h:652-658` | 缺少 btree 特性标志（extents, snapshots, write_buffer 等） |
| `struct bpos` 的磁盘序对齐 | `bcachefs_format.h:137-168` | bcachefs 的 `bpos` 在 LE 和 BE 下的字段顺序不同，用于 memcmp 优化 |

### 9.2 文件系统级类型（P3）

| 缺失类型 | bcachefs 定义 | 影响 |
|----------|-------------|------|
| `struct bch_fs` | `bcachefs.h:703-874` | 全局文件系统状态：设备数组、btree 根、journal、allocator、snapshot、VFS 等 |
| `struct bch_dev` | `bcachefs.h:479-603` | 设备描述符：bucket allocator、journal 设备、IO 统计等 |
| `struct bch_opts` | `opts.h` | 挂载选项结构体 |

### 9.3 子模块类型（其他审计覆盖）

| 缺失类型 | bcachefs 定义 | 审计位置 |
|----------|-------------|----------|
| `struct btree_insert_entry` | `btree/types.h` | `audit-btree.md` |
| `struct btree_path` | `btree/types.h` | `audit-btree.md` |
| `struct btree_update` | `btree/types.h` | `audit-btree.md` |
| `struct bkey_cached` | `btree/key_cache_types.h` | `audit-btree.md` |
| `struct open_bucket` | `alloc/types.h` | 待审计 |

---

## 10. 详细差距清单

### 10.1 P1（必须修复）

| ID | 文件 | 行号 | 描述 |
|----|------|------|------|
| P1-1 | `key.rs` | 130-135 | `Bpos` 字段名为 `vol_id`（代替 C 的 `inode`）。`Bpos::from_key()` 将 vol_id 硬编码为 0，与 bcachefs 的 `bpos.inode` 语义不同。 |
| P1-2 | `key.rs` | 38-52 | `BKEY_NR_FIELDS=5`，`BKEY_U64S=3`，磁盘 key 格式与 bcachefs 不兼容。缺少 SIZE(32bit)、VERSION_HI(32bit)、VERSION_LO(64bit)，使用了 PADDR(48bit) 和 VER(16bit)。 |
| P1-3 | `btree/mod.rs` | 41-58 | `BtreeId` 仅 6 个变体（Extents, Subvolumes, Snapshots, SnapshotTrees, Alloc, Freespace），bcachefs 定义了 28 个。缺少 inodes/dirents/xattrs/quotas/stripes/reflink/backpointers 等核心 btree。 |
| P1-4 | `key.rs` | 96-102 | `KeyType` 仅 3 个变体（Normal, Deleted, Whiteout），bcachefs 定义了 38 个（KEY_TYPE_deleted=0, whiteout=1, ..., accounting=34）。值与名称均未对齐。 |
| P1-5 | `types.rs` | 53-95 | `StorageError` 为扁平枚举，缺少 bcachefs 的两级错误类层次。`TransactionRestartLimit` 等应是非错误（值=0）。 |

### 10.2 P2（建议修复）

| ID | 文件 | 行号 | 描述 |
|----|------|------|------|
| P2-1 | `types.rs` | 15-18 | `BlockAddr { raw: u64, ver: u16 }` 不对应 `bch_extent_ptr`（dev + offset + gen + csum）。虽然概念层次不同（后端抽象 vs 物理设备指针），但应记录差距。 |
| P2-2 | `types.rs` | 37-41 | `BackendType{S3,Nfs}` 无 bcachefs 对应。bcachefs 操作块设备而非 S3/NFS。这是 volmount 特有的抽象层。 |
| P2-3 | `types.rs` | 43-49 | `HealthStatus` 无 bcachefs 对应。bcachefs 通过 flags + dev state 追踪健康。 |
| P2-4 | `types.rs` | 33-34 | `Capacity = u64` 类型别名，无明确单位。bcachefs 无此类型，使用计算函数。 |
| P2-5 | `types.rs` | 61 | `ChecksumMismatch` 只有 expected/actual u32，bcachefs 的 `bch_csum` 是 128 位（two u64）。 |
| P2-6 | `types.rs` | 117-128 | `Watermark::InteriorUpdate` 命名 vs bcachefs `BCH_WATERMARK_interior_updates`（无缩写 + 复数）。 |
| P2-7 | `key.rs` | 45 | `BKEY_FIELD_PADDR` 和 `BKEY_FIELD_VER` 是 Rust 特有字段名，无 bcachefs 对应。 |
| P2-8 | `key.rs` | 52 | `BKEY_HEADER_BYTES = 3` 显式常量，bcachefs 通过 `sizeof` 推断。值已对齐。 |
| P2-9 | `btree/mod.rs` | 60-76 | `BtreeId::name()` 和 `BtreeId::count()` 方法，bcachefs 使用 `__bch2_btree_ids[]` 字符串数组。功能等价但无 flag 支持。 |

### 10.3 P3（功能缺失）

| ID | 描述 |
|----|------|
| P3-1 | **`struct bversion`** — 12 字节版本号（lo=u64, hi=u32），用于 key 版本比较和 journal seq 追踪。volmount 仅在 `BtreeKey` 中使用了 16bit 的简化版本号。 |
| P3-2 | **`struct bkey`（完整）** — bcachefs 的 `bkey` 包含 bversion(12B) + size(4B) + bpos(20B) = 36B 数据 + 3B header + 1B pad = 40B。volmount 的 `BtreeKey` 只有 13 字节（vaddr+snapshot_id+key_type）。 |
| P3-3 | **`struct bch_fs`** — 全局文件系统状态，包含所有子系统。volmount 的 `BtreeEngine` + `BtreeCache` + `Journal` 等分散对象无统一的 `BchFs` 容器。 |
| P3-4 | **`struct bch_dev`** — 设备描述符（bucket allocator, journal device, IO stats）。volmount 的 `BlockDevice` trait 是简化的抽象。 |
| P3-5 | **`enum bch_data_type`** — 数据类型的枚举（`BCH_DATA_sb`, `BCH_DATA_journal`, `BCH_DATA_btree`, `BCH_DATA_user` 等），用于 alloc 和 gc。volmount 无此类型。 |
| P3-6 | **`enum btree_id_flags`** — btree 特性标志（extents, snapshots, write_buffer 等）。 |
| P3-7 | **`enum bch_member_state`** — 设备成员状态（rw, ro, failed, removed, ...）。 |

---

## 11. 架构差异分析

### 11.1 后端抽象 vs 块设备模型

volmount 的核心抽象是"后端存储"（`storage` 模块 + `BackendType`），bcachefs 的核心抽象是"块设备"（`bch_dev` + bucket allocator + bio）。

```
volmount:  Volume → Btree → BtreeNode → BtreePtrV2 → BlockAddr → Backend(S3|Nfs)
bcachefs:  bch_fs → btree → btree_node → bch_btree_ptr_v2 → bch_extent_ptr → bch_dev → bio
```

这意味着 `BlockAddr` 不是 `bch_extent_ptr` 的替代品——`BlockAddr` 是"这个块的逻辑地址在第几个原始块"而 `bch_extent_ptr` 是"这个数据在哪个设备的哪个扇区"。

### 11.2 全局状态管理

bcachefs 使用 `struct bch_fs` 作为所有子系统的统一容器（约 170 个字段），通过指针传递。volmount 使用 `BtreeEngine` + 独立模块，全局状态分散。

```
bcachefs:  bch_fs → journal, btree, allocator, snapshots, gc, ...
volmount:  BtreeEngine, Journal (独立), Allocator::new(), ...
```

这不是一个"对齐差距"——它是 Rust 所有权模型与 C 指针传递的合理设计差异。但需要注意：如果未来需要集成 bcachefs 的复杂跨子系统操作（如 `bch2_trans_commit` 同时操作 btree + journal + allocator），volmount 的独立模块可能需要一个统一的 `BchFs` 等价物。

### 11.3 错误处理的哲学差异

bcachefs 使用两级错误代码 + `bch2_err_matches()` 做分类匹配。重启类错误（`transaction_restart`）值为 0，不视为错误。volmount 的 `StorageError` 将所有异常合并为扁平枚举，且将事务重启视为错误。

**影响**: 如果 volmount 需要运行 bcachefs 的复杂操作路径（含 `do_bch2_trans_commit` 那种 20+ 重试场景），当前的 `StorageError` 设计会导致过多的 `Err(TransactionRestartLimit(...))` 或 `Err(TransactionLockConflict(...))` 处理。

### 11.4 BtreeId 与 KeyType 的覆盖度

volmount 的 `BtreeId`（6 个）和 `KeyType`（3 个）是 bcachefs 的最小化子集。虽然 volmount 当前只需要 extents/subvolumes/snapshots/alloc/freespace 等核心功能，但如果未来需要支持：
- 目录项（`dirents` btree + `KEY_TYPE_dirent`）
- 扩展属性（`xattrs` btree + `KEY_TYPE_xattr`）
- 引用链接（`reflink` btree + `KEY_TYPE_reflink_v`）
- 配额（`quotas` btree + `KEY_TYPE_quota`）
- GC 和 fsck 功能

则必须扩展 `BtreeId` 和 `KeyType`。

---

## 12. 建议行动计划

### Phase A：高优先级修复（P1）

1. **修复 BKEY 格式**（P1-2）：将 `BKEY_NR_FIELDS` 从 5 扩展为 6，添加 `SIZE`, `VERSION_HI`, `VERSION_LO`，对齐 bcachefs `enum bch_bkey_fields`。更新 `BKEY_FIELD_BITS`, `BKEY_U64S`（3→5），`BKEY_FORMAT_CURRENT` 以及 pack/unpack 函数。
2. **修复 Bpos 字段名**（P1-1）：考虑 `vol_id` → `inode` 或添加文档说明语义差异。修改 `from_key()` 传递真实 inode。
3. **扩展 KeyType**（P1-4）：重新排列枚举值与 bcachefs 对齐（从 0 开始），添加 `extent`, `inode_v3`, `dirent`, `alloc_v4`, `backpointer`, `accounting`。

### Phase B：中期工作（P2）

1. **BtreeId 扩展**（P1-3）：按 bcachefs 索引添加 `inodes`, `dirents`, `xattrs`, `quotas`, `stripes`, `reflink` 等核心 btree 类型。
2. **错误层次重构**（P1-5）：将 `StorageError` 拆分为可恢复/不可恢复两层，添加 `bch2_err_matches()` 等价功能。
3. **添加 BCH_ 前缀常量**：添加 `BCH_WATERMARK_*` 等常量以支持直接的 bcachefs 常量引用。

### Phase C：长期工作（P3）

1. **添加 `bversion` 类型**（12 字节版本号）
2. **添加 `struct bch_fs` 等价容器**（如 `BchFs` 统一状态）
3. **扩展 BtreeId 至全部 28 个变体**
4. **添加 `enum btree_id_flags` 的支持**

### Phase D：无需操作（P4 — 合理差异）

1. `BackendType`、`HealthStatus` — volmount 特有的抽象，保持
2. `BlockAddr` 简化设计 — volmount 后端抽象层的合适选择，保持
3. `Capacity`/`BlockSize` 类型别名 — Rust 类型安全的合理实践，保持
4. `pub use types::*` 导出模式 — 合理的集中管理，保持

---

## 附录 A：文件映射

| bcachefs C 头文件 | volmount Rust 文件 | 对齐度 |
|-------------------|-------------------|:------:|
| `bcachefs_format.h` (bpos, bkey, bkey_packed, btree_id, bch_bkey_type) | `btree/key.rs` + `btree/mod.rs` (BtreeId) | ⚠ 偏差 (字段不匹配) |
| `bcachefs.h` (bch_fs, bch_dev, bch_fs_flags) | — | ❌ 缺失 |
| `errcode.h` (enum bch_errcode) | `types.rs` (StorageError) | ⚠ 偏差 (简化) |
| `alloc/types.h` (BCH_WATERMARKS) | `types.rs` (Watermark) | ✓ 对齐 |
| `opts.h` (enum bch_opts) | `config` 模块 | 待审计 |
| `btree/types.h` (btree_id flags, 预声明) | `btree/types.rs` + `btree/mod.rs` | ⚠ 偏差 (简化) |

## 附录 B：公共 API 覆盖率（全局类型）

| volmount 公共 API（`pub use types::*` 导出） | 有 C 对应 | 状态 |
|---------------------------------------------|:---------:|:----:|
| `VolumeId = u64` | ⚠ (inode/subvol_id 概念) | 语义偏差 |
| `BlockAddr { raw, ver }` | ⚠ (`bch_extent_ptr`) | 概念层次不同 |
| `BlockSize = u32` | ✓ `block_bytes()` | ✓ |
| `Capacity = u64` | — | 合理别名 |
| `BackendType` | — | volmount 特有 |
| `HealthStatus` | — | volmount 特有 |
| `StorageError` (14 variants) | ⚠ `enum bch_errcode` (~300) | 严重简化 |
| `Watermark` (7 variants) | ✓ `enum bch_watermark` (7) | ✓ |
| `WATERMARK_NR = 7` | ✓ `BCH_WATERMARK_NR = 7` | ✓ |
| `BtreeId` (6 variants) | ⚠ `enum btree_id` (28) | 严重不足 |
| `KeyType` (3 variants) | ⚠ `enum bch_bkey_type` (38) | 严重不足 |

---

*审计完成日期：2026-06-24 | 审计人：volmount-agent | 参考版本：bcachefs-tools（本地 `/home/black/Documents/bcachefs-tools/fs/`）*
