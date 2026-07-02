# 崩溃恢复对齐 bcachefs

## Goal

将 volmount 的崩溃恢复流程与 bcachefs 对齐，补充当前缺失的关键机制。

## 调研结果

### bcachefs 恢复流程（简化）

```
bch2_fs_open()
  └─ __bch2_fs_open() → __bch2_fs_start()
       └─ bch2_fs_recovery()
            ├─ sb.clean? → 跳过 journal 读（clean section 快路径）
            ├─ !sb.clean → bch2_journal_read() 全量读
            ├─ journal_replay_early()  — 恢复 btree root
            ├─ read_btree_roots()      — 从磁盘加载 btree 根节点
            ├─ bch2_run_recovery_passes_startup()
            │    ├─ accounting_read / alloc_read / snapshots_read
            │    ├─ set_may_go_rw       ← journal overlay 切换点
            │    ├─ journal_replay      ← 重放 journal keys 到 btree
            │    └─ ... (41 passes total)
            └─ bch2_fs_read_write()     ← 启动后台线程
```

### volmount 恢复流程（当前）

```
init_volume()
  ├─ Superblock::read_from_backend()
  ├─ sb.clean_shutdown? 
  │    ├─ true → checkpoint 快路径
  │    └─ false → BtreeEngine::recover_from_journal()
  │         ├─ Journal::from_superblock()
  │         ├─ read_btree_roots()       — Phase 1
  │         ├─ merge + load_root()      — Phase 2
  │         └─ replay_all_to_engine()   — Phase 3
  └─ root_snapshot + allocator 加载
```

### 已识别的差距

| # | Gap | 严重性 | 说明 |
|---|-----|--------|------|
| 1 | **checkpoint 前不 flush journal** | 正确性 | bcachefs shutdown 先 flush journal 再 checkpoint。volmount 忽略 pending 条目 |
| 2 | **replayed_seqs 不持久化** | 正确性 | 恢复后不回写到 Superblock。再次崩溃会重复回放所有 entries |
| 3 | **无 journal seq 截断/blacklist** | 正确性 | bcachefs 用黑名单标记 checkpoint 已覆盖的 seq。volmount 可能回放过期 entry |
| 4 | **无 clean section** | 性能 | bcachefs clean shutdown 把 btree root 存在 Superblock 中，无需扫描 |
| 5 | **JournalSuperblockState 不回写** | 健壮性 | discard_idx/dirty_idx 等状态不持久，下次恢复可能误判 |
| 6 | **无 gap 检测** | 健壮性 | `read_bucket()` CRC 失败就 break，可能跳过有效数据 |
| 7 | **无 recovery passes 系统** | 架构 | bcachefs 41 个 pass，volmount 单线线性恢复 |
| 8 | **无 journal overlay** | 架构 | bcachefs 有 journal keys overlay 分离恢复中和写入的 keys |
| 9 | **journal_buckets 固定 32** | 扩展性 | Superblock 硬编码 |

## Requirements ✅ 全部完成

- [x] bcachefs-exact recovery passes 系统：位掩码调度（`trailing_zeros` 迭代）、`passes_complete`（位掩码）+ `pass_done`（标量 max）、`PASS_ALWAYS`/`PASS_UNCLEAN`/`PASS_SILENT` flags
- [x] bcachefs-exact journal overlay：排序 VecDeque + overwritten 标记，`set_may_go_rw` → `journal_replay` 生命周期
- [x] Journal flush → checkpoint 顺序（checkpoint 前 flush journal + write_blacklist）
- [x] replayed_seqs 持久化到 Superblock（sync_to_superblock + checkpoint 写回）
- [x] Journal seq 截断/blacklist 机制（write_blacklist + extract_blacklist_entries）
- [x] Clean section（clean_shutdown 时的 btree root 快路径）
- [x] JournalSuperblockState 写回（recovery 完成后回写给 superblock + 编码）
- [x] Gap 检测强化（read_bucket CRC 失败改 continue 而非 break）
- [x] 动态 journal bucket 数量（Superblock `[u64; 32]` → `Vec<u64>`）

## Acceptance Criteria ✅ 全部完成

- [x] unclean shutdown → bitmask 调度执行 journal_read + btree_roots + set_may_go_rw + journal_replay → 数据一致
- [x] clean shutdown → clean section 快路径 → 秒级加载
- [x] 多次崩溃 → 每次恢复正确（run_passes 重跑所有 ALWAYS pass，幂等 replay）
- [x] checkpoint 前后 journal seq 正确截断（blacklist 过滤 + replayed_seqs 持久化）
- [x] bcachefs pass ordering：`pass_done = max(pass_done, pass_idx)`，`passes_complete` 位掩码正确
- [x] journal overlay：`insert_guarded()` → `JournalOverlay.push()`（active 且非 draining 时）
- [x] set_may_go_rw 后新写入走 `insert_guarded()` → overlay；replay/drain 走 `insert_entry_raw()` → btree

## 已决策的设计选择

| 问题 | 决策 |
|------|------|
| Recovery passes 模块归属 | volmount-core 新 `recovery/` 模块，daemon 调用 `recovery::run_passes()` |
| Journal overlay 集成 | Overlay 在 `set_may_go_rw` pass 创建，`insert_guarded()` 守卫外部写入路径，内部操作（replay/compaction/split）走 `insert_raw()` 绕过 |
| Clean section 格式 | `CleanSection { root_addrs: Vec<(btree_id, addr)>, journal_seq: u64 }` 存入 Superblock |
| pass 调度机制 | bcachefs-exact 位掩码调度：`trailing_zeros()` 迭代 + `passes_complete` 位掩码 + `pass_done` 标量 max |

## 审查修复摘要

来自 engineering review 的修复（已合并到 design.md 和 implement.md）：

1. **replay bypass overlay** — `replay_all_to_engine` 调用 `insert_raw()`，不经过 `insert_guarded()`
2. **passes_done 不跳过 ALWAYS pass** — journal 内容每次崩溃后可能不同，必须重跑
3. **alloc_read stub** — 等待独立的 alloc journal 任务启用
4. **依赖修正** — P2/P3/P4 在 P1 后可并行 | P5 完全独立
5. **测试策略** — 每个 pass 有单元测试 + 集成测试 + 多次崩溃验证
6. **Superblock 兼容性** — 新字段用 `Option<T>` 或默认值，旧磁盘格式优雅降级
