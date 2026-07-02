# Subvolume API 差距分析审计报告

> **日期**: 2026-06-24  
> **目标**: volmount-core `subvol/` Rust API vs bcachefs 内核子卷 C API  
> **参考源**:  
> - bcachefs 内核: `fs/bcachefs/snapshots/subvolume.{h,c,format.h,types.h}`  
> - bcachefs-tools: `fs/snapshots/subvolume.{h,c}`  
> - bcachefs-tools CLI: `src/commands/subvolume.rs`  
> - volmount-core: `crates/volmount-core/src/subvol/{mod.rs,ops.rs,types.rs}`

---

## 1. 类型定义对比

### 1.1 `bch_subvolume` 数据结构

| 字段 | bcachefs 内核 | volmount-core | 差距 |
|------|--------------|---------------|------|
| flags | `__le32 flags` (RO/SNAP/UNLINKED) | `BchSubvolumeFlags(u32)` (相同三标志) | ✅ 一致 |
| snapshot | `__le32 snapshot` — 指向 snapshots btree 的 ID | `snapshot: u32` — 记录快照 ID | ✅ 语义一致 |
| inode | `__le64 inode` — 根目录 inode 号 | **缺失** | ❌ **P1** volmount 无 inode 层，无法关联子卷根目录 |
| creation_parent | `__le32 creation_parent` — 快照创建来源子卷 ID | `parent_subvol: u32` | ⚠️ **P2** 字段名不同，但语义匹配 |
| fs_path_parent | `__le32 fs_path_parent` — 文件系统路径父节点 | **缺失** | ❌ **P2** volmount 无路径层级概念 |
| otime | `bch_le128 otime` — 128 位创建时间戳 | `created_at: i64` — Unix 时间戳 | ⚠️ **P3** 精度维度不同，可用但不可二进制兼容 |
| size | 无 | `size: u64` — 子卷大小 | ⚠️ **P3** volmount 额外字段；bcachefs 从 extent 计算 |
| `bch_val v` | 通用 btree value 头 | **无对应** — 直接 bincode 序列化 | ⚠️ **P3** 设计取舍，不影响功能 |

### 1.2 枚举/辅助类型

| 类型 | bcachefs 内核 | volmount-core | 差距 |
|------|--------------|---------------|------|
| Flags 位域 | `LE32_BITMASK` 宏: RO(0), SNAP(1), UNLINKED(2) | `BchSubvolumeFlags`: 完全相同 | ✅ 一致 |
| `subvol_inum` | `{u64 subvol, u64 inum}` — 子卷+inode 对 | 无 | ❌ **P1** inode 层缺失导致 |
| BCACHEFS_ROOT_SUBVOL | `1` (root subvolume) | 无常量，`AtomicU32` 从 1 递增 | ⚠️ **P2** 未初始化 root subvol |
| SUBVOL_POS_MIN/MAX | `POS(0,1)` ~ `POS(0,S32_MAX)` | 无显式范围检查 | ⚠️ **P3** 缺少边界校验 |

### 1.3 序列化格式

| 维度 | bcachefs 内核 | volmount-core | 差距 |
|------|--------------|---------------|------|
| 格式 | 原生 C struct + `__le32`/`__le64` 字段 | bincode（Rust 原生） | ❌ **P2** 二进制不兼容 |
| 版本控制 | 通过 `bkey_val_bytes` 动态检测旧格式 | 无可变大小逻辑 | ⚠️ **P3** 无向前兼容 |

---

## 2. 函数签名对比

### 2.1 生命周期与所有权

| 维度 | bcachefs 内核 | volmount-core |
|------|--------------|---------------|
| 参数模式 | `struct btree_trans *`（可变事务指针） | `&mut BtreeEngine` 或 `&BtreeEngine` |
| 错误处理 | `int` 返回值（0=成功，负 errno） | `Result<T, StorageError>` / `Option<T>` |
| 事务语义 | 显式事务：`commit_do`, `lockrestart_do` | 隐式：每次调用直接操作 engine |
| 引用传递 | 输出参数通过指针 (`*dst`) | 返回值通过 Rust 所有权 |

### 2.2 核心函数对比

| bcachefs 内核 | volmount-core | 差距说明 |
|---------------|---------------|----------|
| `bch2_subvolume_create(trans, inode, parent_subvolid, src_subvolid, &new_subvolid, &new_snapshotid, ro)` | `SubvolumeManager::create(engine, snapshot_id, size, created_at) -> u32` | ⚠️ **P1** 签名差异大：volmount 无 inode 参数、无 new_snapshotid 输出、无 ro 参数；多了 size/created_at |
| _(同上，src_subvolid!=0 时为快照)_ | `SubvolumeManager::create_snapshot(engine, parent_subvol, parent_snapshot, size, created_at) -> Result<u32>` | ⚠️ **P1** bcachefs 用同一函数处理创建和快照；volmount 分离 |
| `bch2_subvolume_unlink(trans, subvolid)` | `SubvolumeManager::delete(engine, subvol_id) -> Result<()>` | ⚠️ **P2** 语义相似但 bcachefs 有异步删除（workqueue + pagecache 清理） |
| `bch2_subvolume_get(trans, subvol, inconsistent_if_not_found, &s)` | `SubvolumeManager::load(engine, subvol_id) -> Option<BchSubvolume>` | ⚠️ **P2** bcachefs 有 `inconsistent_if_not_found` 标志触发 fsck 修复 |
| `bch2_subvolume_get_snapshot(trans, subvolid, &snapid)` | _无直接对应_ (需手动 load → read.snapshot) | ⚠️ **P2** 小差距，可内联解决 |
| `bch2_subvol_has_children(trans, subvol)` | _无直接对应_ (reparent_children 扫描全量) | ❌ **P1** bcachefs 使用 `BTREE_ID_subvolume_children` 位图高效查询 |
| `bch2_subvol_is_ro_trans(trans, subvol)` | _无直接对应_ (仅 `BchSubvolume::is_read_only()`) | ⚠️ **P3** 可通过 load 后检查，但无外部调用封装 |
| `bch2_subvolumes_reparent(trans, subvolid_to_delete)` | `SubvolumeManager::reparent_children(engine, subvol_id, new_parent)` | ⚠️ **P2** 语义一致，但 volmount 扫描全 btree，bcachefs 用子卷 children btree |
| `bch2_initialize_subvolumes(c)` | _无直接对应_ | ❌ **P2** 无 root subvolume 初始化 |

### 2.3 volmount 额外函数

| 函数 | 说明 |
|------|------|
| `SubvolumeManager::list(engine) -> Vec<(u32, BchSubvolume)>` | bcachefs 无直接等价（通过 for_each_btree_key 迭代） |
| `SubvolumeManager::count(engine) -> usize` | bcachefs 无直接等价 |
| `reset_id_allocator()` | 仅测试用 |

---

## 3. 调用约定对比

### 3.1 事务模型

```
bcachefs:                          volmount:
─────────────────────────────      ─────────────────────────────
struct btree_trans *trans          &mut BtreeEngine
  ├─ bch2_trans_init()               ├─ 引擎内部管理事务
  ├─ lockrestart_do()                ├─ 无显式 restart
  ├─ commit_do()                     ├─ 无显式 commit
  └─ bch2_trans_exit()               └─ engine.drop()
```

**差距**: ⚠️ **P2** — volmount 无事务层抽象。bcachefs 的子卷操作始终在事务上下文中执行，支持 lock restart、原子提交和回滚。volmount 直接操作 `BtreeEngine`，事务控制由 `BtreeEngine` 内部管理，但 subvolume 层无事务边界可见。

### 3.2 Trigger/钩子系统

```
bcachefs:                          volmount:
─────────────────────────────      ─────────────────────────────
bch2_subvolume_trigger()           无 trigger 注册
  ├─ subvolume_children_mod()       subvolume 变更无钩子
  └─ BTREE_TRIGGER_transactional
```

**差距**: ❌ **P2** — bcachefs 的 btree trigger 在子卷变更时自动维护 `subvolume_children` btree。volmount 无 trigger，需要手动调用 `reparent_children()`。

### 3.3 删除流程对比

```
bcachefs bch2_subvolume_unlink():                  volmount SubvolumeManager::delete():
──────────────────────────────                     ─────────────────────────────────
1. SET_BCH_SUBVOLUME_UNLINKED                      1. load → mark_unlinked()
2. fs_path_parent = 0                              2. delete_entry (墓碑)
3. 注册 commit hook                                 3. insert_entry_raw (UNLINKED)
4. hook → unlinked list push                        4. /* 无异步删除 */
5. workqueue → evict pagecache
6. workqueue → bch2_subvolume_delete()
   ├─ bch2_subvolumes_reparent()
   └─ bch2_snapshot_node_set_deleted()
```

**差距**: ❌ **P1** — bcachefs 的删除流程包含 reparent（自动）、snapshot 节点标记删除、pagecache 清理和异步 workqueue。volmount 仅做软删除标记，无后续清理。

---

## 4. 关系模型对比

### 4.1 子卷 ↔ 快照关系

```
bcachefs:                               volmount:
══════════════════════════════════       ══════════════════════════════════
子卷 btree: BTREE_ID_subvolumes         子卷 btree: BtreeId::Subvolumes
POS(0, subvol_id)                       Bpos::new(0, subvol_id, 0)
  └─ .snapshot -> BTREE_ID_snapshots      └─ .snapshot: u32（仅记录编号）
       └─ snapshot_t.parent（树结构）          ⚠️ 无 snapshot btree 关联
       └─ snapshot_t.subvol（反向引用）

快照创建:
bch2_subvolume_create():
  1. bch2_snapshot_node_create()         SubvolumeManager::create_snapshot():
      ├─ 创建 snapshot_t 节点              1. allocate_subvol_id()
      ├─ 更新 skiplist                     2. BchSubvolume::new_snapshot()
      └─ 更新源子卷 .snapshot ID            3. 写入 btree
  2. 创建子卷条目                          ⚠️ 不创建快照节点
  3. 返回 new_subvolid + new_snapshotid      不返回快照 ID
```

**差距**: ❌ **P1** — volmount 的子卷快照创建不操作 Snapshots btree。SnapshotTree 仅存在于 `btree::snapshot::SnapshotTree` 作为独立内存结构，未与 subvolume 创建流程集成。这导致：
- 快照树不会随子卷创建自动扩展
- `test_create_snapshot_subvolume` 等测试可能因快照树状态不一致失败（已知预存失败）

### 4.2 子卷子树关系

```
bcachefs:                               volmount:
══════════════════════════════════       ══════════════════════════════════
子卷关系追踪:                          子卷关系追踪:
  BTREE_ID_subvolume_children           全 btree 扫描
  POS(fs_path_parent, subvol_id)        engine.get(BtreeId::Subvolumes)
  └─ 位图 btree（KEY_TYPE_set）           .for_each_entry(...)

操作:
- 创建时自动更新 trigger               - 无 trigger
- has_children: 位图 peek              - 无 O(1) 检查
- reparent: 更新 trigger 管理位图      - reparent_children: 全扫描 + 重写
```

**差距**: ❌ **P1** — volmount 缺少 `subvolume_children` btree。`reparent_children()` 的时间复杂度为 O(n)，随子卷数量线性增长。无 `has_children()` 快速检查。

### 4.3 全局 ID 分配

```
bcachefs:                               volmount:
══════════════════════════════════       ══════════════════════════════════
分配方式:                              分配方式:
  bch2_bkey_get_empty_slot()            static AtomicU32
  在 btree 中找空槽                     单调递增
  支持紧凑分配（不会浪费 ID）             永不回收 ID
```

**差距**: ⚠️ **P3** — 单调递增分配器在测试中可工作，但生产环境中（crash + 重启）会重置。bcachefs 的 btree 空槽分配是无状态且 crash-safe 的。注意：`reset_id_allocator()` 仅用于测试。

---

## 5. 差距总结与优先级

### P1 — 必须修复（功能完整性）

| # | 差距 | 影响 |
|---|------|------|
| 1 | **缺少 `inode` 字段** | volmount 无法将子卷关联到文件系统目录树；子卷创建需要预先分配的 inode 号 |
| 2 | **快照树未与子卷创建集成** | 子卷创建不创建 snapshot btree 节点，导致快照关系无法持久化 |
| 3 | **缺少 `subvolume_children` btree** | has_children 和 reparent 效率低；无 trigger 自动维护 |
| 4 | **删除流程不完整** | 无 reparent、无 snapshot 清理、无异步 pagecache 清理 |
| 5 | **create/create_snapshot 与 bcachefs 签名差距大** | 缺少 inode、new_snapshotid、ro 参数 |

### P2 — 应修复（结构完整性与兼容性）

| # | 差距 | 影响 |
|---|------|------|
| 1 | **二进制序列化不兼容** | 无法与 bcachefs-tools 互操作 |
| 2 | **缺少 `fs_path_parent` 字段** | 无路径层级概念 |
| 3 | **无事务层抽象** | 子卷操作无原子性保障 |
| 4 | **无 trigger 钩子系统** | 无法自动维护派生数据 |
| 5 | **无 `initialize_subvolumes()`** | 无 root subvolume，引导流程不完整 |
| 6 | **无 `subvolume_get_snapshot()` 封装** | 需手动 load 两次 |
| 7 | **`parent_subvol` 命名不匹配** | bcachefs 区分 `creation_parent` / `fs_path_parent` |

### P3 — 建议修复（边缘案例与质量）

| # | 差距 | 影响 |
|---|------|------|
| 1 | **无 key 范围验证** | 不会拒绝越界 subvol_id |
| 2 | **全局 AtomicU32 ID 分配器非 crash-safe** | 测试用，生产需改为 btree 空槽分配 |
| 3 | **时间戳为 i64 而非 128 位** | bcachefs otime 更高精度，但实用差异小 |
| 4 | **`size` 字段来源不明确** | 需文档化是预留空间还是已用空间 |
| 5 | **无 `subvol_is_ro()` 封装** | 已可通过 `BchSubvolume::is_read_only()` 检查 |

---

## 6. 已知预存测试失败备注

以下 `volmount-core` 测试在审计前已存在且失败，与本次差距分析一致：

| 测试 | 根因 |
|------|------|
| `subvol::ops::tests::test_create_multiple` | 可能与 AtomicU32 分配器 + 测试隔离相关 |
| `subvol::ops::tests::test_create_snapshot_subvolume` | 快照树未集成，子卷创建后快照状态可能不一致 |
| `snap::snapshot::tests::test_skip_list_ordered` | Skiplist skiplist 构建算法可能不对（快照树层，非子卷层） |

---

## 7. 建议修复顺序

1. **P1#1+P1#2**: 添加 `inode` 字段 + 将 subvolume create 与 Snapshots btree 集成（最核心）
2. **P1#3**: 添加 `subvolume_children` btree + trigger 自动维护（替换全扫描）
3. **P1#4+P1#5**: 统一 create/create_snapshot 签名，补充删除流程
4. **P2#1~P2#3**: 二进制兼容 + `fs_path_parent` + 事务抽象
5. **P2#4~P2#7**: Trigger 系统 + 初始化和查询函数
6. **P3**: 边缘案例加固

---

*审计员: AI (volmount API alignment task)*  
*审计范围: 类型定义、函数签名、调用约定、关系模型*  
*参考 bcachefs 版本: Linux 内核 bcachefs (2026) + bcachefs-tools (head)*
