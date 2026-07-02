# 继续 bcachefs API 对齐 — 基于 docs 推动

## Goal

继续推进 volmount-core 的 bcachefs API 对齐工作，基于 `docs/` 中的 13 份设计文档指引，
在上一轮已完成 P0 全部 4 项 + 关键 P1 修复的基础上，持续推进剩余的对齐项。

## 已完成（上一轮 `06-24-api-align-bcachefs`）

### P0（全部完成 ✅）
- Journal btree pin 回调机制 
- Recovery passes_failing + restart_recovery 
- BchSb CRC32 校验和

### P1（关键项已完成 ✅）
- Volume 状态机（Created/Starting/RW/Error/Stopping/Stopped）
- Alloc DataType 扩展（4→12 种对齐 BCH_DATA_TYPES）
- Snap 内存表（SnapshotTable, 497行） + Skiplist bug 修复
- Subvol↔snap 集成（create/create_snapshot/delete）
- 类型重命名对齐（btree/snap/subvol/lock/alloc/journal/volume/types/recovery）
- 常量重命名（Watermark::NR, BtreeId 扩展）
- BtreeId 从 6→14+ 枚举变体
- AllocRequest 结构 + BchDataType hooks

### 计划内未完成 ❌
- Bpos 字段顺序 P1 — 格式兼容需要 VERSION 迁移，已跳过
- Lock should_sleep_fn waiter 参数 P1 — 未实施
- P2/P3 共 ~322 项 — 未触及

## 证据事实（已从代码库确认）

1. **docs/ 有 13 份设计文档**（alloc, architecture, block-device, btree, cache, journal, lock, nbd, recovery, snapshots, subvol-volume, superblock, trigger）
2. **当前测试**：571 passed, 0 failed, 6 ignored
3. **锁模型**：SixLock（Read/Intent/Write 三态锁，三级等待）已对齐 bcachefs
4. **BTree**：核心算法已对齐，但 key_cache、packed bpos 比较、Eytzinger 查找等缺失
5. **Journal**：双缓冲已对齐，但慢路径、完整状态机缺失
6. **Snapshot**：跳过列表内存表已修复，但 is_ancestor bitmap、事务支持缺失
7. **Alloc**：DataType 扩展已合并，但 disk_reservation、open_bucket 系统未实现
8. **Volume**：状态机已实现，但完整 shutdown 协议缺少数个状态位

## Requirements

- 以 `docs/` 设计文档为参考指引，确定下一个修复模块
- 每个修复后运行 `cargo test -p volmount-core --lib -- --test-threads=1` 验证
- 遵守现有代码风格和序列化格式（不改 alloc/btree/journal 序列化格式的兼容性）

## Acceptance Criteria

- [ ] 确定当前优先级最高的对齐差距
- [ ] 实施修复后 571+ 测试继续通过（0 failure）
- [ ] 若有格式变动，确保 VERSION 迁移策略

## Out of Scope

- HTTP API（volmountd）设计或重构
- CLI（volmount）子命令调整
- volmount-nbd crate
- 前端/用户界面

## Implementation Plan

### Step 1: Lock should_sleep_fn waiter 参数 [P1]

**文件**: `crates/volmount-core/src/lock/six.rs`
**改动**: `set_should_sleep_fn` 签名增加 `WaiterBox` 参数，匹配 bcachefs `six_lock_should_sleep_fn` 的 `int (*)(struct six_lock *, struct six_lock_waiter *)`。
**验证**: `cargo test -p volmount-core --lib -- --ignored lock`

### Step 2: Alloc 深度对齐 [P1] ✅（已实施）

**文件**:
- `crates/volmount-core/src/alloc/mod.rs`
- `crates/volmount-core/src/alloc/open_bucket.rs`
- `crates/volmount-core/src/alloc/bucket.rs`（新文件: `reservation.rs`）

**具体范围**:
1. **disk_reservation 系统**: `DiskReservation` 结构 + `reserve()`/`commit()`/`rollback()` — 纯内存，无格式变更
2. **OpenBucket.sectors_free**: 添加 `sectors_free: u32` 字段追踪桶内剩余空间
3. **AllocRequest 扩展**: 集成 `DiskReservation` 引用到分配请求中
4. **分配入口更新**: `allocate_blocks` 接入 multi-level 策略（先复用 open_bucket → 再分配新桶）
**验证**: `cargo test -p volmount-core --lib alloc`

## Open Questions

（已关闭）
