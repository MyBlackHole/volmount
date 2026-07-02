# Audit: Volume API — volmount-core vs bcachefs kernel

- **Date**: 2026-06-24
- **Target**: `volmount-core` → `volume/mod.rs`, `types.rs`, `config.rs`, `meta/volume_meta.rs`, `storage/superblock.rs`
- **Reference**: `bcachefs-tools/fs/` → `bcachefs.h`, `bcachefs_format.h`, `sb/io.h`, `sb/io_types.h`, `init/fs.h`, `init/fs.c`, `init/passes.h`, `init/passes_format.h`, `init/recovery.h`

---

## 1. 核心类型定义对比

### 1.1 Volume 结构体

| 维度 | bcachefs (`struct bch_fs`) | volmount (`struct Volume`) | 差异等级 |
|------|---------------------------|---------------------------|----------|
| 位置 | `fs/bcachefs.h:703` | `volume/mod.rs:83` | — |
| 字段数 | ~80+ (struct + 嵌入子结构) | 7 个字段 | P1 |
| 嵌入子结构 | journal, btree, allocator, sb, opts, counters, vfs, ec, reconcil, snapshots, gc, IO paths | engine, allocator, trigger_registry | P1 |
| 自包含性 | 完整文件系统状态，包含后台线程/IO 路径等 | 纯聚合容器：不参与 I/O、后台线程、恢复 | — |
| 设备支持 | 多设备数组 `devs[BCH_SB_MEMBERS_MAX]` | 单后端 `Arc<dyn BlockDevice>` | P2 |
| 状态标志 | `unsigned long flags` (27+ BCH_FS_* bits) | 无状态标志 | P2 |
| 超级块 | `struct bch_sb_cpu sb` + `struct bch_sb_handle disk_sb` | 分离在 `BchSb` (superblock.rs) + `VolumeMeta` | P2 |
| 写引用计数 | `struct enumerated_ref writes` | 无 | P2 |
| VFS | `struct bch_fs_vfs vfs` | 无 (NBD 模式无 VFS) | — |
| 恢复状态 | `struct bch_fs_recovery recovery` | 分离在 `recovery::RecoveryState` | P2 |

**差距分析**:

- bcachefs 的 `struct bch_fs` 是**极致单块设计**：一个结构体承载文件系统所有运行时状态。volmount 的设计是**分层聚合**：`Volume` 仅聚合最小组件，daemon 层 `volmountd::volume::Volume` 封装了 I/O 操作。
- volmount 的架构选择（Volume 纯聚合、daemon 做 I/O）是合理的，但丢失了 bcachefs 中的许多运行时状态跟踪：
  - **无 `BCH_FS_*` 标志位系统**：没有 `BCH_FS_started`/`rw`/`stopping`/`error` 等状态标志
  - **无写引用计数**：bcachefs 用 `c->writes` 跟踪所有未完成写操作，确保读写切换安全
  - **I/O 时钟/延迟统计**缺失
  - **无 shrinker/内存压力反馈**

### 1.2 VolumeConfig

| 字段 | bcachefs (`struct bch_opts`) | volmount (`VolumeConfig`) | 等级 |
|------|------------------------------|---------------------------|------|
| 位置 | `fs/opts.h` (大量选项) | `volume/mod.rs:42` | — |
| block_size | ✓ | ✓ | — |
| 容量 | 隐含 `capacity` (从设备推导) | ✓ | — |
| 设备选项 | 多设备/复制/EC/压缩/校验和 | 仅 `pool_name` + `vol_name` | P1 |
| 挂载选项 | read_only/nostart/degraded/fsck | 无 | P2 |
| 版本检查 | `no_version_check` | 无 | P2 |

- bcachefs 的 `struct bch_opts` 有**数十个优化和启动控制选项**。volmount 的 `VolumeConfig` 极为精简，仅包含最基本的标识+容量信息。
- `pool_name` 是 volmount 特有的概念，对应 bcachefs 的 disk_group 概念但简化了许多。

### 1.3 VolumeStats

| 指标 | bcachefs 等效 | volmount `VolumeStats` | 等级 |
|------|--------------|------------------------|------|
| 块信息 | `bch2_dev_usage` | `total_blocks`/`allocated_blocks` | — |
| btree keys | 遍历 btree 计数器 | `btree_keys` | — |
| 快照计数 | `c->snapshots.nr` | `snapshot_count` | — |
| **复制状态** | `c->replicas` | 缺失 | P2 |
| **IO 延迟** | `c->times[BCH_TIME_*]` | 缺失 | P2 |
| **磁盘使用详情** | `bch2_dev_usage_full` | 只有分配的块数 | P2 |
| **错误统计** | `c->errors` + `c->counters` | 缺失 | P2 |

---

## 2. 函数签名对比

### 2.1 创建/打开/关闭生命周期

| bcachefs | volmount-core | volmountd | 差异等级 |
|----------|---------------|-----------|----------|
| `bch2_fs_open(devices, opts)` → `struct bch_fs*` | `Volume::new(...)` (纯构造) | `create_volume()` + `init_volume()` | P1 |
| `bch2_fs_start(c)` | 无 (不涉及启动) | `init_volume()` 内含恢复 | P1 |
| `bch2_fs_stop(c)` → shutdown | `Drop for Volume` (仅 drop) | `checkpoint_volume()` | P1 |
| `bch2_fs_exit(c)` → stop+free | — | `checkpoint_volume()` + 目录移除 | P1 |
| `bch2_fs_read_only(c)` | 无 | 无 | P2 |
| `bch2_fs_read_write(c)` | 无 | 无 | P2 |

**关键差异**:

1. **`bch2_fs_open` 是单片式**：read_super → alloc → init → start (recovery/initialize) 全部在单个函数中完成。volmount 将此流程拆分为：
   - `create_volume()`（新卷）：建目录 → BchSb::new → 写 superblock → 构造 CoreVolume
   - `init_volume()`（已有卷）：读 meta → 后端 → BchSb::read → checkpoint/journal_recovery → 构造 CoreVolume

2. **bcachefs 有显式的 RO→RW 转换**：`bch2_fs_read_write()` 启动所有后台线程（journal reclaim、allocator、GC、discard 等）。volmount 没有 RW/RO 概念——daemon 启动后即可读写。

3. **bcachefs 有精确的停止协议**：`bch2_fs_stop()` 设置 `BCH_FS_stopping` → wait ref drain → RO → unlink → debug/chardev exit。volmount 的 `checkpoint_volume()` 仅持久化状态，无精细的 drain/refcount 控制。

### 2.2 恢复/初始化

| 函数 | bcachefs | volmount | 等级 |
|------|----------|----------|------|
| 新文件系统初始化 | `bch2_fs_initialize()` | `create_volume()` | — |
| 恢复 | `bch2_fs_recovery()` → 多 pass | `recovery::run_passes()` | P2 |
| journal replay | `bch2_journal_replay()` | `recovery::run_passes()` 内部 | — |
| 恢复 passes | 50+ pass (有序) | ~4 passes (journal→alloc_read→gc→fs_freespace) | P1 |
| 恢复进度持久化 | `bch_sb_field_recovery_passes` | `BchSb.pass_done: u64` (位掩码) | P2 |

bcachefs 的 recovery 有**50+ 个有序 passes**，覆盖：
- 拓扑检查、快照重建、子卷检查、inode/dirent/extent 完整性
- alloc 完整性、backpointer 验证、LRU 检查
- 登录操作恢复

volmount 的 recovery 是**最小子集**（journal_replay → alloc_read → gc_trigger → fs_freespace_init），缺失了几乎所有 fsck 级别的数据完整性检查。

### 2.3 快照/子卷操作

| 操作 | bcachefs | volmount-core `Volume` | 等级 |
|------|----------|----------------------|------|
| 创建快照 | `bch2_snapshot_create()` | `create_snapshot()` | — |
| 删除快照 | `bch2_snapshot_delete()` | `delete_snapshot()` | — |
| 列出快照 | 遍历 snapshot btree | `list_snapshots()` | — |
| 回滚 | 通过 btree iter visibility | `rollback()` (仅验证存在) | P2 |
| **快照 skiplist** | skiplist 指针 (O(log n)) | 无 | P1 |
| **快照树** | BCH_RECOVERY_PASS 级别树重建 | 仅链表 (parent/children) | P2 |
| 创建子卷 | `bch2_subvolume_create()` | `create_subvol()` | — |
| 删除子卷 | unlink + WILL_DELETE | `delete_subvol()` | — |

### 2.4 extent I/O

| 操作 | bcachefs | volmount-core `Volume` | 等级 |
|------|----------|----------------------|------|
| 写 extent | `bch2_write()` → IO path | `write_extent()` (直写 + journal) | P2 |
| 删除 extent | btree delete + trigger | `delete_extent()` | — |
| 读取 | `bch2_read()` → IO path (checksum+decompress) | `get_extent_for_snapshot()` (btree lookup only) | P1 |
| **压缩** | `bch2_compress()` | 无 | P1 |
| **校验和** | per-extent checksums | 无 | P1 |
| **加密** | ChaCha20 + Poly1305 | 无 | P1 |
| **EC纠删码** | Reed-Solomon | 无 | P1 |
| **复制** | 按 replicas 配置的 multi-device mirroring | 无 | P1 |

volmount `write_extent()` 做了 bcachefs 写路径的最小等价（分配 bucket → 写后端 → btree insert + trigger），但缺失：
- 压缩、校验和、加密、EC、复制等数据保护特性
- 完整的 IO 完成路径（bio/io_uring）

---

## 3. 初始化流程对比

```
bcachefs bch2_fs_open:
  read_super (每个设备)
  → bch2_fs_alloc → bch2_fs_init
    → bch2_fs_online (sysfs + chardev + 注册全局列表)
    → bch2_fs_recovery_init
  → __bch2_fs_start
    → bch2_fs_may_start (设备可用性检查)
    → bch2_fs_recovery() 或 bch2_fs_initialize()
      → 50+ recovery passes
    → set_bit(BCH_FS_started)
    → bch2_fs_read_write() (启动后台线程)

volmount create_volume:
  create_dir_all
  → create_backend (NFS/S3)
  → BchSb::new → write_to_backend
  → BlockAllocator::new + BtreeEngine::new (纯内存)
  → CoreVolume::new (聚合)

volmount init_volume:
  read meta.volmount → create_backend
  → BchSb::read_from_backend
  → (clean) BtreeEngine::deserialize_checkpoint_from_bytes
    → allocator.load_from_btree + rebuild_freespace_from_alloc
  → (dirty) Journal::from_superblock → recovery::run_passes
  → 从 Snapshots btree 获取 root_snapshot_id
  → CoreVolume::new
```

**关键缺失**:
- volmount 无 `bch2_fs_online` 等效：sysfs/chardev 注册
- volmount 无多设备仲裁（`bch2_sbs_filter_dead` 检测 split-brain）
- volmount 恢复 passes 是 bcachefs 的 **~8%**（4 vs 50+）
- volmount 无 `state_lock`（`rwsem`）保护并发状态转换
- volmount 无后台线程生命周期管理（journal reclaim、GC、allocator background）

---

## 4. 序列化/元数据格式对比

### 4.1 Superblock

| 特性 | bcachefs (`struct bch_sb`) | volmount (`BchSb`) | 等级 |
|------|---------------------------|-------------------|------|
| 格式 | packed C struct (little-endian) | bincode (serde) | — |
| 大小 | 可变（~1-8KB） | 固定 4KB | — |
| 位置 | 设备固定偏移（8KB） | BlockAddr 0 | — |
| 校验和 | `struct bch_csum csum` (CRC64) | 无 | P1 |
| 版本 | `version` + `version_min` + `version_incompat` | 单一 `version: u32` | P2 |
| UUID | `uuid` + `user_uuid` | 无 (用 `vol_id: u64` 替代) | P2 |
| 标签 | `label[64]` | `vol_name: String` | — |
| 序列号 | `seq` (每次写递增) | 无（直接 overwrite） | P2 |
| 特性位图 | `features[2]` + `compat[2]` | 无 | P1 |
| **字段扩展** | type-tagged `bch_sb_field` sections | `#[serde(default)]` 字段 | P2 |
| 设备成员 | `bch_sb_field_members` | 单后端，无成员表 | P2 |
| journal buckets | `bch_sb_field_journal` / `bch_sb_field_journal_v2` | `journal_buckets: Vec<u64>` | P2 |
| **clean section** | `bch_sb_field_clean` | `clean_section: Option<CleanSection>` | — |
| **recovery passes** | `bch_sb_field_recovery_passes` | `pass_done: u64` | P2 |
| **错误日志** | `bch_sb_field_errors` | 无 | P2 |
| **disk groups** | `bch_sb_field_disk_groups` | 无（用 `pool_name` 替代） | P2 |
| **加密** | `bch_sb_field_crypt` | 无 | P1 |
| **配额** | `bch_sb_field_quota` | 无 | P2 |
| **counters** | `bch_sb_field_counters` | 无 | P2 |

### 4.2 VolumeMeta

`VolumeMeta`（`meta.volmount`）是 volmount 特有的文件系统外元数据文件。bcachefs 将所有元数据放在 superblock 或 btree 内。volmount 分离的设计：
- **优点**：快速读取卷列表（扫目录读 meta 文件），无需 I/O 到 superblock
- **缺点**：meta 文件可能和 superblock 不一致（需 `generation` 字段校验）
- `VolumeMeta` 存储在后端**外**的文件系统中（NFS/S3 的后端路径之外），bcachefs 没有等效概念

### 4.3 CleanSection

| 字段 | bcachefs `bch_sb_field_clean` | volmount `CleanSection` | 等级 |
|------|-------------------------------|------------------------|------|
| btree roots | `struct jset_entry` 编码的 root btree 指针 | `root_addrs: Vec<u64>` + `root_levels: Vec<u8>` | P2 |
| journal seq | 隐含在 roots 的 journal_seq | `journal_seq: u64` | — |
| **时间戳** | 关机/挂载时间 | 无 | P3 |
| **csum** | CRC64 | 无 | P1 |

---

## 5. 严重差距汇总

### P1（必须修复 — 功能完整性缺失）

| 编号 | 差距 | 说明 |
|------|------|------|
| P1.1 | **数据校验和** | bcachefs 每个 extent/superblock/btree node 都有 CRC64/csum，volmount 完全无校验和 |
| P1.2 | **数据压缩** | volmount 无压缩支持，bcachefs 支持 zstd/lz4/gzip |
| P1.3 | **数据加密** | volmount 无加密，bcachefs 支持 ChaCha20+Poly1305 |
| P1.4 | **复制/EC** | volmount 单后端无复制或纠删码 |
| P1.5 | **多设备支持** | bcachefs 支持 N 个设备动态管理，volmount 单后端 |
| P1.6 | **Snapshot skiplist** | bcachefs 快照 skiplist 提供 O(log n) 祖先查询，volmount 无 |
| P1.7 | **恢复 passes 完整性** | volmount 仅有 4 passes，bcachefs 有 50+ passes（含 fsck） |
| P1.8 | **Superblock 校验和** | BchSb 无校验和，块损坏不可检测 |
| P1.9 | **特性位图** | 无 compat/incompat 特性跟踪，升级路径不安全 |
| P1.10 | **Volume 状态标志** | 无 `BCH_FS_*` 系统，无法区分 started/rw/error/stopping 等状态 |

### P2（应该修复 — 运行时健壮性缺失）

| 编号 | 差距 | 说明 |
|------|------|------|
| P2.1 | **写引用计数** | 无 drain 机制，RO/RW 切换不安全 |
| P2.2 | **state_lock** | bcachefs 用 `rwsem` 保护状态转换，volmount 无等效保护 |
| P2.3 | **错误跟踪系统** | bcachefs 有 per-device error counters + fsck error 分类，volmount 仅有 `StorageError` |
| P2.4 | **恢复进度持久化** | volmount 用 `pass_done: u64` 位掩码（简化），bcachefs 有完整 per-pass last_run/runtime |
| P2.5 | **后台线程管理** | journal reclaim、GC、discard、alloc background 等线程均缺失 |
| P2.6 | **Superblock 序列号** | BchSb 直接 overwrite，无 seq 递增，异常断电可能导致 stale sb 无法检测 |
| P2.7 | **UUID 系统** | bcachefs 有 `uuid` + `user_uuid`，volmount 仅有 `vol_id: u64` |
| P2.8 | **RO→RW 转换** | bcachefs 有精确的读-写协议，volmount 无 |
| P2.9 | **配置选项系统** | bcachefs 有丰富的 mount/time/performance 选项，volmount 仅有 4 字段 |
| P2.10 | **I/O 延迟统计** | bcachefs 有 30+ time stats，volmount 无 |
| P2.11 | **元数据版本兼容性** | bcachefs 有 `version`/`version_min`/`version_incompat`，volmount 单一版本 |

### P3（建议修复 — 工程完整性）

| 编号 | 差距 | 说明 |
|------|------|------|
| P3.1 | **sysfs/chardev** | bcachefs 有 sysfs/chardev 接口，volmount 通过 HTTP API 替代 |
| P3.2 | **shutdown 协议** | bcachefs 有精确的三阶段关闭（stop→drain→free），volmount 的 checkpoint 较简单 |
| P3.3 | **单线程恢复保护** | bcachefs 有 `recovery_task` 断言恢复期单线程，volmount 无 |
| P3.4 | **缩容** | bcachefs 支持设备 resize，volmount 无 |
| P3.5 | **磁盘布局文档化** | bcachefs 格式有完整文档，volmount 格式无离线文档 |

---

## 6. bcachefs 独有特性（volmount 无对应实现）

以下 bcachefs 特性在 volmount 中无任何对应：

1. **后台 GC** — `bch2_gc()` 重新计算 oldest_gen
2. **CopyGC** — 碎片整理
3. **Discard 线程** — 后台回收
4. **LRU btree** — 缓存数据生命周期管理
5. **Backpointers** — 反向指针完整性
6. **Logged Ops** — 断点续传操作
7. **Quota** — 用户/组/项目配额
8. **Reconcile** — 后台数据验证
9. **Fallocate/Punch hole** — 空间预分配
10. **Reflink** — 块级去重
11. **Nocow locking** — 无 COW 写锁定
12. **Inode 32bit 限制** — 小 inode 优化
13. **UTF-8 casefolding** — 文件名规范

这些不在当前审计范围内，但标记为长期追赶目标。

---

## 7. 结论

volmount-core 的 Volume API 在**架构层面做了合理的简化**（纯聚合容器 + daemon I/O 分离），但从 bcachefs 对齐角度看：

1. **生命周期管理**（P2）：缺少 RO/RW 转换、写引用计数、state_lock 等运行时保障机制
2. **元数据格式**（P1）：缺少校验和、特性位图、序列号等磁盘格式安全特性
3. **数据路径**（P1）：缺少校验和/压缩/加密/复制/EC 等数据保护
4. **恢复健壮性**（P1）：50+ passes → 4 passes，缺少所有 fsck 级完整性检查
5. **配置系统**（P2）：数十个 bcachefs 选项 vs 4 字段
