# 剩余子系统功能逻辑审查 — 结果汇总

> 审查日期: 2026-06-26
> 任务: 06-26-remaining-subsystems-review
> 方法: 6 个 explore agent 并行对比 volmount Rust 实现 vs bcachefs C 参考

---

## 总体评估

6 个子系统共发现 **17 项 P0 级差异**（数据安全/恢复失败风险）和 **大量 P1/P2 级差异**。

最关键结论: **recovery 模块是死代码**（未集成到 Volume 启动路径），**GC 是空骨架**（所有核心函数返回 0），**journal 校验和只覆盖子集**。

---

## 1. btree trans/iter

**文件**: `btree/transaction.rs`, `btree/iter.rs`
**bcachefs 参考**: `iter.c`, `trans.c`

### P0 — 数据损坏/死锁风险

| # | 差异 | 位置 | 描述 |
|---|------|------|------|
| T1 | `advance()` 跳过路径重遍历 | `iter.rs:397` | 仅递增 `leaf.offset`，bcachefs 每次 advance 后 set_pos + traverse。并发 split/compaction 后 offset 可能指向错误条目 |
| T2 | 缺少 multi-source overlay | `iter.rs:356` | `peek()` 只从 bset 读取，无 journal/key_cache/trans_updates overlay。journal 中有未刷盘修改时返回旧值 → 读后写丢失 |
| T3 | `back_up_and_advance()` 无锁验证 | `iter.rs:421` | 直接读父节点 `key_count` 和 `read_entry`，未验证锁 seq。并发 split 后跳到错误 sibling |
| T4 | `init()` 父节点锁释放策略缺陷 | `iter.rs:101` | intent=true 时父节点只持 Read 锁不阻止其他线程获取写锁做 split |

### P1

| # | 差异 | 位置 |
|---|------|------|
| T5 | path 缓存复用条件太严格 | `transaction.rs:201` |
| T6 | `restart_optimized()` 无节点身份验证 | `iter.rs:650` |
| T7 | `trans_relock()` 无 seq 验证 | `transaction.rs:993` |
| T8 | `lockrestart_do!` 嵌套重启不安全 | `transaction.rs:1092` |
| T9 | `descend_to_first_leaf()` locked_seq=0 | `iter.rs:472` |
| T10 | `traverse()` 为空实现 | `iter.rs:825` |

---

## 2. btree cache/IO

**文件**: `btree/cache.rs`, `btree/io.rs`, `btree/bucket_io.rs`
**bcachefs 参考**: `cache.c`, `write.c`, `read.c`

### P0 — 数据损坏

| # | 差异 | 位置 | 描述 |
|---|------|------|------|
| C1 | `mark_dirty` auto-flush 直接 `dirty.clear()` 丢弃脏数据 | `cache.rs:169` | `inner.dirty.len() >= MAX_DIRTY` 时清空集合，脏节点引用丢失 → **数据丢失** |
| C2 | 无 `will_make_reachable` 保证 | 全局缺失 | COW 父/子写序无保证：父节点可能先于子节点到达磁盘 |

### P1

| # | 差异 | 位置 |
|---|------|------|
| C3 | Cannibalize 无重入保护 → 嵌套死锁 | `cache.rs:298` |
| C4 | Read 验证过少 — 磁盘损坏不可检测 | `io.rs:63` |
| C5 | Write 无 `write_blocked` 保护 → 并发写冲突 | 全局缺失 |
| C6 | Dirty 节点命中无 LRU 提升 | `cache.rs:115` |
| C7 | 无 journal pin 管理 | 全局缺失 |
| C8 | COW 写序无 `will_make_reachable` | 全局缺失 |

---

## 3. btree GC

**文件**: `btree/gc.rs`
**bcachefs 参考**: `check.c`, `check.h`, `check_types.h`, `alloc/accounting.c`, `data/reflink.c`

### P0 — 空骨架，所有核心功能缺失

| # | 缺失功能 | 位置 | 严重度 |
|---|---------|------|--------|
| G1 | **mark-and-sweep 核心**：`bch2_gc_btrees`/`bch2_gc_mark_key` 缺失，bucket 引用计数从未重建 | `gc.rs:124-135` | **P0** |
| G2 | **alloc 检查与修复**：`bch2_gc_alloc_start/done` 缺失 | `gc.rs:129-135` | **P0** |
| G3 | **btree 拓扑检查**：`bch2_check_topology` 空桩 | `gc.rs:119-127` | **P0** |
| G4 | **无 GC 排他锁**：`gc.lock` rwsem 缺失 | `gc.rs:39-46` | **P0** |
| G5 | **generation stale pointer 清理**：`bch2_gc_gens` 空桩 | `gc.rs:109-112` | **P0** |
| G6 | **GC 不在 recovery pass 中** | `recovery/mod.rs` | **P0** |
| G7 | **无 superblock/journal 标记** | 缺失 | **P0** |
| G8 | **无 btree bitmap GC** | 缺失 | **P0** |

### P1

| # | 差异 | 位置 |
|---|------|------|
| G9 | 无 stripe GC | 缺失 |
| G10 | 无 reflink GC | 缺失 |
| G11 | 无 accounting GC（空桩） | `gc.rs:162-168` |
| G12 | `gc_pos_set` 无前向进度断言 | 缺失 |
| G13 | `gc_gens` 无 trylock 防死锁 | 缺失 |

---

## 4. journal flush/write/read

**文件**: `journal/types.rs`, `journal/jset.rs`
**bcachefs 参考**: `journal.c`, `write.c`, `read.c`

### P0 — 数据损坏/恢复失败风险

| # | 差异 | 位置 | 描述 |
|---|------|------|------|
| J1 | CRC32 仅覆盖 entries 而非完整 Jset | `jset.rs:89-101` | magic/seq/last_seq/entry_count 不在校验范围内，位翻转可篡改 |
| J2 | `bch2_journal_flush` 持锁执行 async I/O | `types.rs:1475` | `flush()` 读 buf data 与 `add_entry()` 写入间无同步 → 数据竞争 |
| J3 | 无 journal entry 版本号 | `jset.rs:23-36` | 未来格式变更无兼容路径 |

### P1

| # | 差异 | 位置 |
|---|------|------|
| J4 | 无加密支持 | `jset.rs:109` |
| J5 | 无 flush/noflush 区分 | `types.rs:1475` |
| J6 | 无自动 btree_root/super_entries 注入 | `types.rs:1300` |
| J7 | 无 clean→dirty 转换处理 | `types.rs:1475` |
| J8 | 无多设备读取支持 | `types.rs:1573` |
| J9 | Jset seq/last_seq 不做语义验证 | `types.rs:1577` |

---

## 5. lock six

**文件**: `lock/six.rs`
**bcachefs 参考**: `six.c`, `six.h`

### P0 — 锁语义差异

| # | 差异 | 位置 | 描述 |
|---|------|------|------|
| L1 | `downgrade_write_to_intent` 不递 increment seq | `six.rs:680` | bcachefs 每次 write unlock（含降级）都 `lock->seq++`。volmount 跳过 seq increment → relock 看不到变化 |
| L2 | 无 handoff protocol（lock_acquired 未使用） | `six.rs` 全局 | bcachefs 的 wakeup 为 waiter 获取锁后设 `lock_acquired=true`；volmount 只是 unpark，waiter 自行竞争 → 公平性差 |

### P1

| # | 差异 | 位置 |
|---|------|------|
| L3 | WAITING_WRITE 检查在标准 try_lock_read 中（比 bcachefs 更严格） | `six.rs:298` |
| L4 | try_lock_write percpu ordering: check-then-CAS vs bcachefs set-WRITE_BIT-first | `six.rs:397` |
| L5 | 固定 slot percpu readers vs 真实 percpu | `six.rs:278` |
| L6 | 固定计数 spin vs 自适应 owner_on_cpu | `six.rs` 全局 |
| L7 | Percpu 路径 memory ordering: Acquire fence vs smp_mb() | `six.rs:278` |
| L8 | 无 `should_sleep_fn` acquired lock 处理 | `six.rs:1042` |
| L9 | 约束放松：volmount 不要求先持 intent 再持 write | `six.rs:344` |
| L10 | `lock_readers_add` 用 Relaxed ordering on state | `six.rs:1195` |

---

## 6. recovery

**文件**: `recovery/` 全部，`journal/replay.rs`
**bcachefs 参考**: `recovery.c`, `passes.c`, `passes_format.h`

### P0 — 恢复失败/数据丢失

| # | 差异 | 位置 | 描述 |
|---|------|------|------|
| R1 | **Recovery 模块未集成到 Volume 启动** | `volume/mod.rs` | `bch2_fs_recovery()` 已定义但 Volume::new 不调用，recovery passes 从不执行 → **死代码** |
| R2 | **btree root level 信息丢失** | `btree_roots.rs:17` | 只提取 `(BtreeId, u64)` 地址，丢失 `level` 字段，非 level-0 root 加载后 btree 不可用 |
| R3 | **缺少 check_allocations（GC pass）** | `gc.rs` (空桩) | 无 bucket 引用计数重建，崩溃后分配器可能错误分配被引用的 bucket |
| R4 | **Journal 双重读取** | `journal_read.rs` + `journal_replay.rs` | journal_read 读到 `state.jsets`，但 journal_replay 重新读一次，2x I/O |
| R5 | **无 unclean shutdown seq skip (+64)** | `journal_read.rs` | bcachefs 跳过 64 seq 并黑名单化，防止 btree 引用未落盘 seq |
| R6 | **无 journal rewind 支持** | 缺失 | 无法防御未正确实现 FUA/FLUSH 的块设备 |

### P1

| # | 差异 | 位置 |
|---|------|------|
| R7 | 两阶段重放缺失（accounting→data） | `journal/replay.rs:98` |
| R8 | 缺少 fsck_err 模式区分可修复/致命错误 | `recovery/mod.rs` 全局 |
| R9 | 缺少 snapshots_read pass | 缺失 |
| R10 | 缺少 account_read pass（空桩） | `alloc_read.rs` |
| R11 | `bch2_fs_initialize` 不为新文件系统完整初始化 | `recovery/mod.rs:407` |

---

## 按子系统 P0 计数

| 子系统 | P0 项数 | 最严重问题 |
|--------|---------|-----------|
| btree trans/iter | 4 | advance 无路径重遍历, 缺 overlay |
| btree cache/IO | 2 | `dirty.clear()` 数据丢失, 无 write ordering |
| btree GC | 8 | 全部是空骨架，无 GC 核心逻辑 |
| journal flush/write | 3 | CRC 范围不足, flush 数据竞争, 无版本号 |
| lock six | 2 | seq 不递增, 无 handoff |
| recovery | 6 | 死代码, level 丢失, 缺 GC pass |
| **总计** | **17** | |

---

## 修复优先级建议

### Tier 0 — 必须立即修复（数据安全）
1. **R1**: 将 recovery 集成到 Volume 启动路径
2. **C1**: `dirty.clear()` 改为真正的 flush
3. **J1**: 扩展 CRC32 覆盖整个 Jset
4. **R2**: 修复 btree root level 信息

### Tier 1 — 高优修复（恢复正确性）
5. **G1-G6**: 实现最小可用 GC（mark-and-sweep + alloc repair）
6. **R5**: 添加 unclean shutdown seq skip
7. **T1+T3**: 重写 `advance()` 用 set_pos + traverse
8. **L1**: 修复 `downgrade_write_to_intent` 的 seq increment

### Tier 2 — 中优先（行为一致性）
9. **T2**: 添加 multi-source overlay
10. **C4**: 增强 read verification
11. **L2**: 实现 handoff protocol
12. **J6**: 添加 btree_root 自动注入

### Tier 3 — 低优先（鲁棒性/性能）
- 剩余 P1/P2 项可在后续迭代中解决
