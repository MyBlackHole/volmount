# P0/P1 差距修复执行计划

## 执行策略

- 按独立性和依赖关系分 3 个批次执行
- 每批次内独立任务并行委派给 `trellis-implement` 子代理
- 每个修复后运行对应模块的单元测试验证
- 所有批次完成后运行 `cargo test -p volmount-core --lib` 全量测试

## 验证命令

```bash
cargo test -p volmount-core --lib <module>  # 单模块测试
cargo test -p volmount-core --lib           # 全量测试（492 个）
cargo clippy -p volmount-core               # Lint
```

## Batch 1 — 独立模块修复（可完全并行）

### 1.1 volume: BchSb 添加 CRC32 校验和 [P0-4]

**文件**: `crates/volmount-core/src/meta/`（superblock.rs）
**参考**: bcachefs `BCACHEFS_SB_CRC` / superblock 校验和机制
**改动**:
- BchSb 添加 `crc: u32` 字段
- 序列化时计算 `crc32fast::hash()` 写入
- 反序列化时校验
- VERSION 枚举加迁移逻辑（老版本无校验和）
**验证**: `cargo test -p volmount-core --lib meta`

### 1.2 recovery: 添加 rewind + passes_failing [P0-2, P0-3]

**文件**: `crates/volmount-core/src/recovery/`
**参考**: bcachefs `bch2_err_throw()`, `c->recovery_pass_done`, `c->passes_failing`
**改动**:
- RecoveryState 添加 `passes_failing: u64`（位掩码）
- 添加 `RewindReason` 枚举 + `restart_recovery()` 函数
- 失败 pass 标记 → retry → 连续失败则降级
- pass 执行后检查 `passes_failing` 位, 达阈值返回错误
**验证**: `cargo test -p volmount-core --lib recovery`

### 1.3 types: BtreeId 扩展 [P1 — types]

**文件**: `crates/volmount-core/src/types.rs`, `crates/volmount-core/src/btree/mod.rs`
**参考**: bcachefs `BTREE_ID_*` 枚举（~22 个 btree 类型）
**改动**:
- BtreeId 枚举从 6 变体扩展到 14+（添加 Snapshots, Subvolumes, SubvolumeChildren, Accounting, Buckets, Freespace, LRU, NeedDiscard, Backpointers, BtreeNodeSizes 等）
- 添加 `btree_id_flags()` 辅助函数
- 注意: 新增 btree 类型暂不需要实现完整的 btree 引擎支持，只注册类型 ID
**验证**: `cargo check -p volmount-core`

### 1.4 lock: should_sleep_fn 添加 waiter 参数 [P1 — lock]

**文件**: `crates/volmount-core/src/lock/six.rs`
**参考**: bcachefs `six_lock_should_sleep_fn` 签名 `int (*)(struct six_lock *, struct six_lock_waiter *)`
**改动**:
- `WaiterBox` 公开字段
- `set_should_sleep_fn` 签名改为 `Fn(&SixLock, &WaiterBox) -> bool`
- 调用处传递 lock + waiter 引用
**验证**: `cargo test -p volmount-core --lib -- --ignored lock`

---

## Batch 2 — 核心模块修复（btree/journal 有依赖）

### 2.1 journal: btree→journal pin 通路 [P0-1]

**文件**: `crates/volmount-core/src/journal/`, `crates/volmount-core/src/btree/cache.rs`
**参考**: bcachefs `journal_pin.h`, `bch2_btree_node_write_dirty()` 中的 pin 逻辑
**改动**:
- journal 添加 `pin_list` / `pin` 注册机制
- btree node 写脏时 pin journal 条目
- journal flush 推进 `last_seq_ondisk` 时检查 btree pin
**依赖**: 需要 journal types.rs 添加 `JournalPin` 类型
**验证**: `cargo test -p volmount-core --lib journal && cargo test -p volmount-core --lib btree`

### 2.2 btree: Bpos 字段顺序修正 [P1 — btree]

**文件**: `crates/volmount-core/src/btree/key.rs`
**参考**: bcachefs `struct bpos { snapshot, offset, inode } __packed`（LE 排布支持 memcmp）
**改动**:
- Bpos 字段顺序改为 `(snapshot, offset, vol_id)` 以支持大整数比较
- 添加 `#[repr(C, packed)]` 标注
- 更新所有构造/解构处
- 关键: 需要格式版本迁移（Bpos 顺序影响 BKEY 编码）
**验证**: `cargo test -p volmount-core --lib btree`

### 2.3 snap: 添加内存 snapshot 表 + bitmap 祖先查询 [P1 — snap]

**文件**: `crates/volmount-core/src/snap/`
**参考**: bcachefs `struct snapshot_t`, `bch2_snapshot_is_ancestor()`, `is_ancestor[128]` bitmap
**改动**:
- 添加 `SnapshotTable` 结构（BTreeMap<u32, SnapshotT> 或 Vec 索引）
- `build_snapshot_table()` — 从 Snapshots btree 一次性加载
- `is_ancestor_with_bitmap()` — 填充并使用 128-bit bitmap 快速路径
- `lookup()` / `parent()` / `root()` 等便捷方法
**验证**: `cargo test -p volmount-core --lib snap`

### 2.4 subvol: 子卷操作集成 Snapshots btree [P1 — subvol]

**文件**: `crates/volmount-core/src/subvol/ops.rs`, `crates/volmount-core/src/snap/snapshot.rs`
**参考**: bcachefs `bch2_subvolume_create()` 内部调用 `bch2_snapshot_node_create()`
**改动**:
- `create_snapshot()` 内部调用 snapshot 模块创建 snapshot 节点
- `create()` 为根子卷创建默认 snapshot
- 子卷删除时清理关联 snapshot
**依赖**: 需要 Batch 2.3（snapshot 表）完成
**验证**: `cargo test -p volmount-core --lib subvol`

---

## Batch 3 — alloc + volume 深度修复

### 3.1 alloc: 扩展数据类型 + alloc_request [P1 — alloc]

**文件**: `crates/volmount-core/src/alloc/`
**参考**: bcachefs `BCH_DATA_*` 枚举（11 种），`alloc_request` 结构
**改动**:
- DataType 枚举从 4 种扩展到 11 种
- 添加 `AllocRequest { watermark, data_type, target, replicas }` 结构
- 修改 `allocate_bucket()` 接受 `AllocRequest`
**验证**: `cargo test -p volmount-core --lib alloc`

### 3.2 volume: 状态标志 + shutdown 协议 [P1 — volume]

**文件**: `crates/volmount-core/src/volume/`
**参考**: bcachefs `BCH_FS_STARTED`, `BCH_FS_EMERGENCY_RO`, `BCH_FS_WAS_RW` 标志 + `__bch2_fs_stop()`
**改动**:
- Volume 添加 `state: AtomicU8`（可取值: Created/Starting/RW/Error/Stopping/Stopped）
- 添加 `start()` / `stop()` / `is_rw()` 方法
- 状态转换加 assert 检查
**验证**: `cargo test -p volmount-core --lib volume`

---

## 风险

- Bpos 字段顺序修改（2.2）影响磁盘格式兼容性，需要版本迁移策略
- snap table（2.3）和 alloc_request（3.1）可能触发测试失败，需逐一排查
- journal pin（2.1）与现有 checkpoint 机制的集成需要仔细设计
- 每个修复后必须运行对应模块的单元测试，不能跳过
