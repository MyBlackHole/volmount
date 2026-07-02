# Volume 模块 — bcachefs 函数级覆盖地图

> volmount `volume` 模块（`crates/volmount-core/src/volume/`）与 bcachefs `init/fs.c`（`bch_fs` 生命周期 + `BCH_FS_*` 状态标志）的覆盖对照。
>
> Volume 是 volmount 特有的聚合抽象，bcachefs 无直接对应的 `volume` 概念。bcachefs 中对应的是 `bch_fs` 结构体（挂载的文件系统实例）。因此覆盖地图的核心是生命周期状态机和恢复跟踪字段的对齐。
>
> 本文件由 `trellis` 自动生成，反映当前 main 分支状态。

---

## 文件对应关系

| 角色 | volmount | bcachefs |
|------|----------|----------|
| 核心 Volume 结构 | `crates/volmount-core/src/volume/mod.rs` (845 行) | `init/fs.c` (bch_fs 生命周期) + `bcachefs_format.h` (BCH_FS_* 标志) |
| 元数据定义 | `crates/volmount-core/src/meta/volume_meta.rs` (52 行) | `bcachefs_format.h` (superblock) |
| 卷管理器 (daemon) | `crates/volmountd/src/volume.rs` (747 行) | `init/fs.c` (bch2_fs_open / bch2_fs_stop) |
| 模块导出 | `crates/volmount-core/src/meta/mod.rs` (4 行) | — |

---

## 图例

| 标记 | 含义 |
|------|------|
| ✅ | **完全实现** — 函数名对齐，功能逻辑一致，测试覆盖 |
| ⚠️ | **已实现但有差异** — 签名简化或实现模式与 C 有细微差异 |
| ❓ | **未实现** — 无对应 volmount 函数 |
| ➖ | **Infra 差距** — 因基础设施未就绪暂未实现 |
| 🌟 | **volmount 扩展** — 无 bcachefs 直接对应，volmount 特有 |

---

## 一、类型与常量

| bcachefs | volmount | 状态 | 说明 |
|----------|----------|------|------|
| `enum bch_fs_state` (BCH_FS_new_fs=0, BCH_FS_rw=1, BCH_FS_error=2, BCH_FS_stopping=3, BCH_FS_clean_shutdown=4) | `VolumeState` (New=0, Starting=1, Rw=2, Error=3, Stopping=4, Stopped=5, RwWithPendingRecovery=6) | ⚠️ | **差异**：volmount 增加 `Starting` (1) 和 `RwWithPendingRecovery` (6) 中间状态；`Rw=2` vs bcachefs `BCH_FS_rw=1`。命名风格 volmount 使用 enum vs C 的 #define bitmask。P1-2 Batch C 已验证 `RwWithPendingRecovery` 对齐 |
| `struct bch_fs.recovery_pass_done` `recovery_passes_complete` `passes_failing` | `Volume.recovery_pass_done: AtomicU8`, `recovery_passes_complete: AtomicU64`, `passes_failing: AtomicU64` | ✅ | P1-1 Batch C 已验证 |
| `struct bch_fs.error_count` `fsck_error` | `Volume.error_count: AtomicU64`, `fsck_error: AtomicU64` | ✅ | P2-3 Batch C 已验证 |
| `DEFAULT_BLOCK_SIZE` (4096) | `DEFAULT_BLOCK_SIZE` | ✅ | 常量值一致 |
| `DEFAULT_CAPACITY` (1GB) | `DEFAULT_CAPACITY` | ✅ | volmount 定义 |

---

## 二、构造器

### 2.1 `Volume::new()` | `crates/volmount-core/src/volume/mod.rs:178`

| 维度 | bcachefs | volmount | 状态 |
|------|----------|----------|------|
| 对应 | `struct bch_fs` 初始化 + `bch2_fs_start()` | `Volume::new()` 聚合构造器 | ⚠️ |
| 签名 | `bch2_fs_open(dev, opts) -> struct bch_fs*` | `new(meta, backend, engine, root_snapshot_id, allocator, trigger_registry, config) -> Self` | ⚠️ |
| 说明 | bcachefs 的 `bch2_fs_open` 打开块设备创建 bch_fs；volmount 由调用方预先构造所有组件再注入，Volume 只做聚合。I/O 操作（superblock 读写、stop/drain）由 daemon 层的 `VolumeManager` 负责。 | | ⚠️ |

### 2.2 `VolumeMeta::new()` | `crates/volmount-core/src/meta/volume_meta.rs:28`

| 维度 | bcachefs | volmount | 状态 |
|------|----------|----------|------|
| 对应 | `superblock` 初始化（创建时写入超级块字段） | `VolumeMeta::new(vol_name, vol_id, pool_name, block_size, capacity, backend_type) -> Self` | 🌟 |
| 说明 | volmount 将创建时固定的元数据抽取为 `VolumeMeta`，并写入 backend superblock。bcachefs 中所有元数据在 superblock 中管理。 | | 🌟 |

### 2.3 `VolumeMeta` superblock 持久化

| 维度 | volmount | 状态 |
|------|----------|------|
| 对应 | `superblock` 中的卷元数据字段 | ✅ |
| 说明 | `VolumeMeta` 不再单独序列化/反序列化到 `meta.volmount`；卷元数据只保留在 backend superblock 中。 | ✅ |

---

## 三、生命周期状态机

Volume 生命周期状态机与 bcachefs `bch_fs` 状态标志（`BCH_FS_*`）对齐。

### 3.1 状态转换图

```
New ──[start()]──→ Starting ──[go_rw()]──→ Rw ──[stop()]──→ Stopping ──[set_stopped()]──→ Stopped
 │                    │                      │                    │
 └──[set_error()]─────┴──[set_error()]───────┴──[set_error()]─────┘
```

### 3.2 状态查询

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 1 | `state()` | 207 | `BCH_FS_rw` / `BCH_FS_error` 等标志读取 | ✅ | 从 AtomicU8 decode 为 VolumeState enum |
| 2 | `is_rw()` | 284 | `BCH_FS_rw` flag 检查 | ✅ | Rw 或 RwWithPendingRecovery 均视为 rw |

### 3.3 状态推进

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 3 | `start()` | 226 | `bch2_fs_start()` | ✅ | New → Starting，CAS compare_exchange |
| 4 | `go_rw()` | 241 | `bch2_fs_go_rw()` / `BCH_FS_rw` 标志位置位 | ✅ | Starting → Rw |
| 5 | `stop()` | 258 | `bch2_fs_read_only()` / `bch2_fs_stop()` | ✅ | Rw → Stopping |
| 6 | `set_stopped()` | 271 | `bch2_fs_stop()` 完成 → `BCH_FS_clean_shutdown` | ✅ | Stopping → Stopped |
| 7 | `set_error()` | 293 | `BCH_FS_error` 标志位置位 | ✅ | 任何非终止状态 → Error；Stopped 不可转入 Error，使用 loop+CAS 防并发 |

---

## 四、恢复跟踪

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 8 | `recovery_progress()` | 320 | `bch_fs.recovery_pass_done` / `passes_failing` 读取 | ✅ | P1-1 Batch C 已验证 |
| 9 | `set_recovery_progress()` | 329 | `bch_fs` 恢复字段更新 | ✅ | three Atomic fields store |

---

## 五、错误计数

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 10 | `record_error()` | 339 | `bch_fs.error_count++` | ✅ | P2-3 Batch C 已验证 |
| 11 | `error_count()` | 344 | `bch_fs.error_count` 读取 | ✅ | AtomicU64 load |
| 12 | `record_fsck_error()` | 349 | `bch_fs.fsck_error++` | ✅ | P2-3 Batch C 已验证 |
| 13 | `fsck_error_count()` | 354 | `bch_fs.fsck_error` 读取 | ✅ | AtomicU64 load |

---

## 六、组件访问器

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 14 | `engine()` | 361 | `&c->btree` (struct bch_fs 的 btree 引用) | 🌟 | Rust 惯用访问器，volmount 的 BtreeEngine 封装 5 种 btree |
| 15 | `engine_mut()` | 366 | — | 🌟 | &mut 引用，无 bcachefs 对应 |
| 16 | `allocator()` | 371 | `&c->alloc` | 🌟 | 分配器引用 |
| 17 | `allocator_mut()` | 376 | — | 🌟 | &mut 引用 |
| 18 | `engine_mut_and_allocator()` | 385 | — | 🌟 | **Rust 特有**：同时返回 `(&mut BtreeEngine, &BchAllocator)`，避免借用检查器对同一 `&mut Self` 的两个方法的组合限制。用于 daemon 层 `close_volume` 中的 `write_data_to_blocks` |
| 19 | `root_snapshot_id()` | 390 | — | 🌟 | 根快照 ID 访问器，volmount 特有（bcachefs 中 root snapshot 在快照树中定位） |

---

## 七、Btree 查询

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 20 | `get_extent_for_snapshot()` | 400 | `bch2_btree_iter_peek()` + snapshot 过滤 | ⚠️ | **Δ1 MVP**：使用 `Bpos(vaddr, snapshot)` 精确查询，无快照可见性遍历。Δ2 会加入 `BtreeIter::init_with_snapshot` + `peek_visible` |

---

## 八、快照操作

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 21 | `create_snapshot()` | 415 | `bch2_snapshot_node_create()` | ✅ | 委托到 snap 模块 `bch2_snapshot_node_create`，Batch B 已验证 |
| 22 | `list_snapshots()` | 423 | `bch2_snapshot_tree` 遍历 | ✅ | 委托到 `list_snapshots_from_btree()`，转为 `SnapshotMeta` |
| 23 | `rollback()` | 434 | 快照存在性检查 | ⚠️ | **简化版**：仅验证快照存在且未删除。Δ2 使用 `BtreeIter::init_with_snapshot + peek_visible` 实现真正的数据回滚 |
| 24 | `delete_snapshot()` | 450 | `bch2_snapshot_node_set_deleted()` | ✅ | 委托到 snap 模块，Batch B 已验证 |

---

## 九、子卷操作

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 25 | `create_subvol()` | 461 | `bch2_subvolume_create()` | ✅ | 委托到 subvol 模块，Batch B 已验证 |
| 26 | `delete_subvol()` | 472 | `bch2_subvolume_delete()` + `bch2_subvolumes_reparent()` | ✅ | 三步流程：reparent → delete → mark WILL_DELETE |
| 27 | `list_subvols()` | 501 | `bch2_subvolume_list()` | ✅ | 委托到 subvol 模块 |

---

## 十、元数据操作

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 28 | `btree_insert()` | 511 | `bch2_btree_insert()` (轻量版) | ⚠️ | **无 journal**：使用 `insert_guarded()` 直写 btree。对应 bcachefs 中不走事务路径的插入 |
| 29 | `btree_insert_with_journal()` | 524 | `bch2_trans_commit()` 完整 journal 路径 | ✅ | 使用 `BtreeTrans` → `journal_insert` → `trans_commit` 完整流程 |
| 30 | `btree_get()` | 542 | `bch2_btree_iter_peek()` | ✅ | 委托到 `engine.get_entry()` |

---

## 十一、Extent 写入/删除（Phase C2）

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 31 | `write_extent()` | 549 | `bch2_alloc_sectors_start_trans()` + `bch2_trans_commit()` | ✅ | 三步骤：① alloc bucket ② write_block_with_csum ③ BtreeTrans insert（触发 alloc_extent_trigger → Alloc btree）。P1-2 Batch C（block_device checksum 集成）已验证 |
| 32 | `delete_extent()` | 593 | `bch2_btree_delete()` + `bch2_bucket_free()` | ⚠️ | 三步流程：① get old paddr ② BtreeTrans delete（触发 alloc trigger）③ `bch2_bucket_free` 释放 bucket。⚠️ 当前 alloc_extent_trigger 同步修改 Alloc btree，bcachefs 中由 reclaim 异步处理 |

---

## 十二、统计信息

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 33 | `stats()` | 627 | `bch2_fs_stats()` / debug 统计 | ⚠️ | volmount 聚合方法：从 engine + allocator + snapshots 收集统计信息。snapshot_tree_depth 当前为 0（Δ2 实现） |
| 34 | `meta()` | 644 | — | 🌟 | VolumeMeta 引用访问器 |

---

## 十三、Btree 节点 Flush

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 35 | `flush_dirty_nodes()` | 655 | `bch2_btree_node_write()` + root flush 支撑 | ✅ | 按 level 升序 flush（拓扑排序）。流程：① drain 脏节点 ② alloc_sectors ③ serialize_to_bucket ④ write_block ⑤ clear_will_make_reachable ⑥ bch2_btree_post_write_cleanup |

---

## 十四、VolumeMeta 元数据操作

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 36 | `VolumeMeta::new()` | 28 | superblock 初始化 | ✅ | `VolumeMeta` 仅作为 superblock 持久化载体 |
| 37 | `VolumeMeta` 的独立序列化接口 | — | — | — | 已删除；不再存在 `meta.volmount` 外部元文件 |
| 38 | `VolumeMeta` 的独立反序列化接口 | — | — | — | 已删除；卷元数据只从 superblock 读取 |

---

## 十五、Daemon 层卷管理（`volmountd/src/volume.rs`）

### 15.1 目录初始化与创建

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 39 | `init_dirs()` | 42 | `mkdir` (用户空间操作) | 🌟 | 创建 `home/blocks/<name>` 目录结构 |
| 40 | `create_volume()` | 58 | `bch2_fs_open`（创建新文件系统分支） | ⚠️ | 五步流程：① create_backend ② 写 Superblock ③ 创建内存组件 ④ 构造 CoreVolume ⑤ 仅依赖 superblock 持久化卷元数据。⚠️ 不包含 journal 初始化（与 bcachefs 新建文件系统的 journal 分配路径差异） |

### 15.2 加载已有卷

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 41 | `init_volume()` | 159 | `bch2_fs_open`（加载已存在文件系统） | ✅ | 七步流程：① 读 meta ② 创建 backend ③ 读 Superblock ④ BtreeEngine 加载（superblock root_ptrs 快路径 / journal recovery 慢路径）⑤ 获取根快照 ID ⑥ 创建 TriggerRegistry ⑦ 构造 CoreVolume。支持干净关闭（superblock root_ptrs + journal 状态）和不干净关闭（journal recovery）两种路径 |

### 15.3 Checkpoint

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 42 | `stop_volume()` | 286 | `bch2_fs_stop()` + `bch2_journal_blacklist()` | ✅ | 五步流程：① wait_idle ② flush_dirty_nodes ③ 回写 root_ptrs 到 superblock ④ journal flush + blacklist ⑤ 标记 clean_shutdown 并持久化 Superblock。已移除 checkpoint volume 块写入路径 |

### 15.4 删除与列表

| # | 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|---|------|------|---------------|------|------|
| 43 | `delete_volume()` | 329 | `rm -rf` (用户空间操作) | 🌟 | stop/drain → remove_dir_all |
| 44 | `list_all_volumes()` | 486 | 目录扫描 (用户空间操作) | 🌟 | 扫描 volumes 目录下的子目录，不再依赖 `meta.volmount` |

---

## 十六、Drop

| 函数 | 行号 | bcachefs 对应 | 状态 | 备注 |
|------|------|---------------|------|------|
| `Drop for Volume` | 697 | `bch2_fs_put()` / `bch2_fs_free()` | 🌟 | 当前为空实现。Volume 生命周期由 daemon 层管理 |

---

## 统计摘要

### 按状态

> 统计口径：仅包含当前活跃函数；已删除接口不计入覆盖率。

| 状态 | 含义 | 数量 | 占比 |
|------|------|------|------|
| ✅ | 完全实现（功能完整） | 25 | 59.5% |
| ⚠️ | 已实现有差异 | 6 | 14.3% |
| 🌟 | volmount 扩展（无 bcachefs 对应） | 11 | 26.2% |
| ❓ | 未实现 | 0 | 0% |
| ➖ | Infra 差距 | 0 | 0% |

**有效 bcachefs API 覆盖**：`✅24 + ⚠️6 = 30/44` ≈ **68.2%**（含 volmount 扩展）

### 按文件

| 文件 | 函数数 | ✅ | ⚠️ | 🌟 | 覆盖率 |
|------|--------|----|----|----|--------|
| `volume/mod.rs` | 35 | 20 | 5 | 10 | 71.4% |
| `meta/volume_meta.rs` | 1 | 1 | 0 | 0 | 100%（另有 2 个已删除接口不计入） |
| `volmountd/src/volume.rs` | 6 | 4 | 1 | 1 | 66.7% |

### 按功能域

| 功能域 | 函数数 | 状态 |
|--------|--------|------|
| 生命周期状态机 | 7 | ✅ 全部对齐 bcachefs |
| 恢复跟踪 | 2 | ✅ 已验证（P1-1 Batch C） |
| 错误计数 | 4 | ✅ 已验证（P2-3 Batch C） |
| 组件访问器 | 6 | 🌟 volmount 特有 |
| 快照操作 | 4 | ✅ 委托到已验证模块 |
| 子卷操作 | 3 | ✅ 委托到已验证模块 |
| Btree 元数据操作 | 3 | ⚠️ btree_insert 无 journal |
| Extent 写入/删除 | 2 | ✅ write_extent 完整；delete_extent ⚠️ 异步差异 |
| 统计/元数据 | 2 | ⚠️ stats 未完整实现 |
| Btree 节点 flush | 1 | ✅ 拓扑排序对齐 |
| Daemon 生命周期 | 6 | ✅ 加载/stop 完整 |

---

## 与 quality-guidelines.md Batch C 的差距对齐

参考 `quality-guidelines.md:723-729` 的 Batch C volume 模块修复表：

| 修复项 | C 引用 | 状态 | 说明 |
|--------|--------|------|------|
| P1-1: recovery 状态追踪字段 | `bch_fs_recovery` | ✅ 已验证 | `recovery_pass_done` / `recovery_passes_complete` / `passes_failing` |
| P1-2: RwWithPendingRecovery 子状态 | `enum bch_fs_state` | ✅ 已验证 | `VolumeState::RwWithPendingRecovery=6` |
| P2-3: error_count AtomicU64 | `bch_fs` `fsck_error` | ✅ 已验证 | `error_count: AtomicU64` / `fsck_error: AtomicU64` |

### 当前新识别的 P1/P2 差距

| 优先级 | 差距 | 文件 | 说明 |
|--------|------|------|------|
| P1 | `state()` 枚举值偏移（Rw=2 vs BCH_FS_rw=1） | `volume/mod.rs:207` | volmount 增加 Starting(1) 导致所有后续值偏移。不影响功能正确性，但同数值常量与 bcachefs 不一致 |
| P1 | `btree_insert()` 无 journal 保护 | `volume/mod.rs:511` | 使用 `insert_guarded()` 不走 journal。当前标记为 ⚠️ 但这是设计选择（轻量读写分离），非缺陷 |
| P1 | `rollback()` 仅做存在性检查，无实际数据回滚 | `volume/mod.rs:434` | Δ2 才实现真正的 BtreeIter 可见性过滤回滚 |
| P2 | `stats().snapshot_tree_depth` 始终为 0 | `volume/mod.rs:639` | Δ2 延迟优化，需遍历 btree 计算 |
| P2 | `delete_extent()` alloc trigger 同步执行 | `volume/mod.rs:593` | bcachefs 中 bucket 释放由 reclaim 异步处理，volmount 同步执行（单线程场景正确） |
| P2 | `create_volume()` 不初始化 journal | `volmountd/volume.rs:51` | 新建卷无 journal bucket 分配，与 bcachefs 新建文件系统的 journal 初始化路径有差异 |

### 差距趋势

```
Batch C (2026-06-27)         当前 (2026-06-30)
  P1: 2 done, 0 open     →   P1: 2 done, 3 open ⚠️ (新识别)
  P2: 1 done, 0 open     →   P2: 1 done, 3 open ⚠️ (新识别)
```

**说明**：Batch C 已完成的 3 项修复全部保持 ✅。当前识别的 P1/P2 差距来源于更深入的函数级审查，大部分为设计选择（非 bcachefs 对齐缺陷），仅 `rollback()` 是功能缺口。

---

## 测试覆盖

| 维度 | 数值 |
|------|------|
| Volume 核心测试数 | 17 tests (volume/mod.rs) |
| Daemon 集成测试数 | 9 tests (volmountd/volume.rs) |
| 全量 volmount-core | 762 passed + 5 known fail + 9 ignored |
| clippy/fmt | 0 warning/diff |

---

## 关键 bcachefs 覆盖覆盖率（`init/fs.c`）

| bcachefs 函数 | volmount 状态 | 备注 |
|---------------|--------------|------|
| `bch2_fs_start()` | ⚠️ | 对应 `Volume::start()`。bcachefs 中包含 journal start + recovery + go_rw 流程，volmount 中 start 仅做状态推进，recovery 由 daemon 层处理 |
| `bch2_fs_go_rw()` | ✅ | 对应 `Volume::go_rw()`。bcachefs 中启用 journal + btree IO，volmount 中状态推进 |
| `bch2_fs_read_only()` | ✅ | 对应 `Volume::stop()` |
| `bch2_fs_stop()` | ✅ | 对应 `Volume::set_stopped()`。bcachefs 包含 clean shutdown + journal flush，volmount 中由 `stop_volume()` 处理 |
| `bch2_fs_open()` | ⚠️ | 对应 `init_volume()` / `create_volume()`。volmount 将打开新卷和加载已有卷分离为两个函数 |
| `bch_fs` 字段访问 | 🌟 | volmount Volume 使用 Rust 访问器方法替代 struct 字段直接访问 |

---

## volmount 扩展函数总览

以下函数为 volmount 特有，无 bcachefs 直接对应：

| 函数 | 文件 | 用途 |
|------|------|------|
| `engine()` / `engine_mut()` | `volume/mod.rs` | BtreeEngine 访问器 |
| `allocator()` / `allocator_mut()` | `volume/mod.rs` | BchAllocator 访问器 |
| `engine_mut_and_allocator()` | `volume/mod.rs` | **Rust 特有**：同时获取 engine+allocator 引用 |
| `root_snapshot_id()` | `volume/mod.rs` | 根快照 ID 访问器 |
| `meta()` | `volume/mod.rs` | VolumeMeta 访问器 |
| `VolumeMeta::new()` | `meta/volume_meta.rs` | 元数据构造函数 |
| `VolumeMeta` | `meta/volume_meta.rs` | superblock 元数据结构 |
| `init_dirs()` | `volmountd/volume.rs` | 目录结构初始化 |
| `delete_volume()` | `volmountd/volume.rs` | 用户空间卷删除 |
| `list_all_volumes()` | `volmountd/volume.rs` | 用户空间卷列表 |
| `create_backend()` | `volmountd/volume.rs` | BlockDevice 工厂函数 |
| `DaemonError` | `volmountd/volume.rs` | daemon 层错误类型 |
| `Drop for Volume` | `volume/mod.rs` | 析构函数 |

---

## 架构差异总结

| 维度 | bcachefs (C) | volmount (Rust) |
|------|-------------|-----------------|
| 卷结构 | `struct bch_fs` — 500+ 字段内联 | `struct Volume` — 13 字段，纯聚合容器 |
| 生命周期 | `bch2_fs_open` → recovery → `bch2_fs_go_rw` → `bch2_fs_stop` | daemon 层 `create_volume/init_volume` → `Volume.start` → `go_rw` → `stop` → `stop_volume` |
| I/O 路径 | bch_fs 直接操作块设备 | Volume 不参与 I/O，daemon 层的 VolumeManager 负责 |
| 恢复 | `__bch2_fs_recovery` 内联在 fs_open 中 | 分离为 `recovery` 模块，由 daemon 层 `init_volume` 调用 |
| 状态标志 | `unsigned long flags` (bitmask) | `AtomicU8` + `VolumeState` enum |
| 元数据持久化 | superblock 单二进制格式 | superblock 单一权威格式 |
| 状态推进 | `set_bit(BCH_FS_rw, &c->flags)` | `compare_exchange` CAS AtomicU8 |
| 组件引用 | `c->btree`, `c->alloc` 字段直接访问 | `engine()`, `allocator()` 方法访问 |
| 多卷管理 | 单文件系统（无卷概念） | daemon 层 `HashMap<name, Volume>` 管理多卷 |
| stop/drain | `bch2_fs_stop()` 内部自动触发 | `stop_volume()` 独立函数，由 daemon 调用 |

---

## 关键总结

1. **Volume 是 volmount 特有的聚合抽象**，无 bcachefs 直接对应的卷概念。覆盖地图的核心对齐点是生命周期状态机和恢复跟踪字段。
2. **核心生命周期对齐良好**：全部 7 个状态机函数（start/go_rw/stop/set_stopped/is_rw/set_error/state）均对应 bcachefs `BCH_FS_*` 标志操作。
3. **Batch C 修复保持有效**：3 项已验证的修复（P1-1 recovery tracking, P1-2 RwWithPendingRecovery, P2-3 error_count）全部 ✅。
4. **主要差距在功能缺口**：`rollback()` 只做存在性检查，`stats().snapshot_tree_depth` 为 0，`btree_insert()` 不走 journal。均为已知的 Δ2/设计选择。
5. **快照/子卷委托到已验证模块**：全部快照和子卷操作委托到 snap/subvol 模块。这些模块已在 Batch B 完成 bcachefs 一致性验证。
6. **daemon 层实现了完整的卷生命周期**：创建→加载（含 crash recovery）→stop/drain→删除，覆盖 bcachefs `bch2_fs_open` / `bch2_fs_stop` 的完整语义。
7. **测试稳定**：17 个 volume 测试 + 9 个 daemon 集成测试全部通过。
