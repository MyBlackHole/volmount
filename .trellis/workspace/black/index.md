# Workspace Index - black

> Journal tracking for AI development sessions.

---

## Current Status

<!-- @@@auto:current-status -->
- **Active File**: `journal-2.md`
- **Total Sessions**: 80
- **Last Active**: 2026-07-02
<!-- @@@/auto:current-status -->

---

## Active Documents

<!-- @@@auto:active-documents -->
| File | Lines | Status |
|------|-------|--------|
| `journal-2.md` | ~734 | Active |
| `journal-1.md` | ~1975 | Archived |
<!-- @@@/auto:active-documents -->

---

## Session History

<!-- @@@auto:session-history -->
| # | Date | Title | Commits | Branch |
|---|------|-------|---------|--------|
| 80 | 2026-07-02 | Snapshot block device | `385e882` | `main` |
| 79 | 2026-07-02 | Recover root replay with cached btrees | `d23cf09` | `main` |
| 78 | 2026-07-02 | Btree cache dirty rewrite alignment | `3e1ee20` | `main` |
| 77 | 2026-07-02 | Btree cache system memory pressure alignment | `760ac48` | `main` |
| 76 | 2026-07-02 | Journal dirty idx last_seq alignment | `3ea670e` | `main` |
| 75 | 2026-07-02 | Sort iter bset_idx cleanup | `2ac415b` | `main` |
| 74 | 2026-07-02 | Read-done checksum alignment | `84049f2` | `main` |
| 73 | 2026-07-02 | Key cache sync point spec sync | `fc0986a` | `main` |
| 72 | 2026-07-02 | Check topology spec sync | `c6e09cf` | `main` |
| 71 | 2026-07-02 | Bcachefs test baseline sync | `8a84afe` | `main` |
| 70 | 2026-07-02 | Bcachefs continue | `b52612a` | `main` |
| 69 | 2026-07-02 | Btree iter path reuse alignment | `8147567` | `main` |
| 68 | 2026-07-02 | Btree interior update alignment | `ee0ef6b` | `main` |
| 67 | 2026-07-02 | BCachefs key cache journal flush alignment | `dca412d` | `main` |
| 66 | 2026-07-01 | BCachefs recovery alloc-pass alignment | `4dd21f3` | `main` |
| 65 | 2026-07-01 | BCachefs recovery online scheduling | `3c8f2e6` | `main` |
| 64 | 2026-07-01 | BCachefs continue | `2e900a0` | `main` |
| 63 | 2026-07-01 | BCachefs follow-up alignment | `e3a3b6f` | `main` |
| 62 | 2026-07-01 | BCachefs feature alignment | `cb515cf` | `main` |
| 61 | 2026-07-01 | Btree iter overlay peeking + P4+P5 flush_pins | `feb11b8` | `main` |
| 60 | 2026-07-01 | Phase 2+3: Btree 固定 C 布局序列化 + CRC32C 硬件加速 | `915e4b6` | `main` |
| 59 | 2026-06-30 | Journal Phase 3 — validate.c 校验链对齐 | `75a3896` | `main` |
| 58 | 2026-06-30 | Journal Phase 2 — write.c closure 链深对齐 | `75a3896` | `main` |
| 57 | 2026-06-30 | P1 check_snapshots 对齐 — skiplist 验证 / parent children / SUBVOL 交叉验证 | `89254e1`, `42fbb26` | `main` |
| 56 | 2026-06-30 | D8.5 VolumeMeta wal_seq/generation 字段清理 + recovery pass 修复 | `23c3f79`, `29595bc`, `2870478`, `83209f9` | `main` |
| 55 | 2026-06-30 | btree-cache P2 — prefetch/async fill/InFlight 等待 | `b31b307`, `5e27231` | `main` |
| 54 | 2026-06-30 | cache-alignment: transition_state/pin/unpin/reclaim + cache/mod.rs 清理 | `a3e57e1` | `main` |
| 53 | 2026-06-30 | keycache-lifecycle: flush_going_ro 死循环 bug 修复 | `8a61b43` | `main` |
| 52 | 2026-06-30 | wb-lifecycle: write_buffer 6 个生命周期函数实现 | `4e71096` | `main` |
| 51 | 2026-06-30 | coverage-maps: 创建 8 个模块函数级覆盖地图 | `3a575ae` | `main` |
| 50 | 2026-06-30 | p0-gc: GC sweep phase 实施 — bch2_gc_sweep + ReclaimStats + 4 测试 | `3a575ae` | `main` |
| 49 | 2026-06-30 | commit-flow-alignment: Phase 0b trigger ordering + begin/rollback bcachefs semantics | `8163ccb` | `main` |
| 48 | 2026-06-30 | bcachefs 事务对齐第二阶段 — 锁顺序 + 更新条目 | `1fb0f7d` | `main` |
| 47 | 2026-06-30 | Child-C: key cache 连接 — trigger_key_cache_miss + insert_entry_cached | `d9156f8` | `main` |
| 46 | 2026-06-30 | Child-B: 统一 pin 管理 — 移除 Volume 级 journal pin | `5d35c94` | `main` |
| 45 | 2026-06-30 | bcachefs 事务全链路整合 — Child-A + Child-D 实施完成 | `8833a99`, `b703ff9`, `b5576c2` | `main` |
| 44 | 2026-06-29 | SixLock wakeup 路径 BC3 修复（WAITING bit 清除竞态）+ 全局函数级 bcachefs 覆盖地图 | `54520eb` | `main` |
| 43 | 2026-06-29 | Lock wakeup bcachefs align (Option C) | `7f6ca93` | `main` |
| 42 | 2026-06-29 | Lock P1 修复: WRITE_BIT 预设 + 内存序 | `e8b31db` | `main` |
| 41 | 2026-06-29 | Batch H: _seq API 迁移 — BtreeNode/Volume 嵌入 JournalEntryPin | `51b70f9`, `c110cf0` | `main` |
| 40 | 2026-06-29 | KeyCache 嵌入 JournalEntryPin — Batch G | `c8921bc`, `6a597e0` | `main` |
| 39 | 2026-06-29 | Journal Reclaim bcachefs 对齐 | `611b007` | `main` |
| 38 | 2026-06-28 | BTree Cache will_make_reachable bcachefs 对齐 | `149e9ee` | `main` |
| 37 | 2026-06-28 | Phase 5: superblock feature flags + redundant field cleanup | `97c5e79` | `main` |
| 36 | 2026-06-28 | Phase 1: Stable Pass IDs + Pass Table Reorder | `3d457b9` | `main` |
| 35 | 2026-06-28 | Recovery Pass 真逻辑 — 6 个 pass NOP 存根替换为 bcachefs 对齐实现 | `b37e973` | `main` |
| 34 | 2026-06-28 | Batch E — Btree IO 节点读写对齐（含 Phase 1-4） | `d292299` | `main` |
| 33 | 2026-06-27 | Batch A fidelity 修复完成 + 验证 + 提交 | `47a34dc` | `main` |
| 32 | 2026-06-27 | Wave 5: journal P0 + snap/subvol + btree P2 + audit D1-D4 | `bba2deb` | `main` |
| 31 | 2026-06-27 | Wave 4 — P0 bcachefs 一致性差距修复 (7 模块 12 项) | `9affcc1` | `main` |
| 30 | 2026-06-27 | P0 bcachefs 差距修复 Wave 3 补充 — Alloc 字段 + Lock API | `181f8b5` | `main` |
| 29 | 2026-06-27 | P0 bcachefs 一致性差距修复 — Batches 1-7 | `fe4eb0f` | `main` |
| 28 | 2026-06-26 | Snap/Lock bcachefs 一致性修复 — D1-D4 全部完成 | `c8f2bd7` | `main` |
| 27 | 2026-06-26 | alloc P1 Wave4: try_decrease + 分配失败重试循环 | `9fdaf1f` | `main` |
| 26 | 2026-06-26 | alloc P0 全部修复 + P1 Wave 1-3（reserved_buckets + OpenBucket gen + 扇区级核算） | `6b1e352`, `75e5ac3`, `e7f815a`, `b0df055` | `main` |
| 25 | 2026-06-26 | P1-1 bpos packed compare + gc-p0 finish | `1575357`, `98ed788` | `main` |
| 24 | 2026-06-26 | Bcachefs 一致性检查 + 文档修复 | `90483c5` | `main` |
| 23 | 2026-06-26 | P0 功能逻辑修复 — recovery + CRC + cache + btree root level | `71cc866` | `main` |
| 22 | 2026-06-26 | 剩余 6 子系统功能逻辑审查 + spec 更新 | `f8c0061`, `46b6191`, `afb3586` | `main` |
| 21 | 2026-06-26 | bcachefs 核心模块 API 全面对齐 | `16f117b`, `7024c8b` | `main` |
| 20 | 2026-06-25 | bcachefs API 对齐 P1 继续 — Lock should_sleep_fn + Alloc depth | `49b01ec`, `045cc1c` | `main` |
| 19 | 2026-06-25 | API对齐bcachefs — 审计+P0/P1修复 | `dfbc9fb`, `e233c12`, `8db55ac`, `f77e883`, `491aa82`, `bf05fd2`, `dd4548b`, `508bc59`, `55584dd`, `05f9a6d`, `816dd33`, `12be63d` | `main` |
| 18 | 2026-06-24 | bcachefs API 命名对齐: journal + alloc 重命名 | `dfbc9fb` | `main` |
| 17 | 2026-06-24 | Journal P1 清理 | `7868c9b` | `main` |
| 16 | 2026-06-24 | P1: Alloc 写入点隔离 — bcachefs WRITE_POINT_MAX=32 对齐实现 | `29ebc0d` | `main` |
| 15 | 2026-06-24 | 补齐 docs/ 全部 12 篇 bcachefs 对比设计文档 + trellis-check 验证 | `d8c85af`, `d53e7eb`, `8e9ea86` | `main` |
| 14 | 2026-06-24 | Watermark + Freespace + Journal P1 收尾 | `160f97c`, `ce18c4a`, `ec9dc52` | `main` |
| 13 | 2026-06-23 | 模块设计文档 Phase 2 | `c1cd064` | `main` |
| 12 | 2026-06-23 | bcachefs 架构设计文档生成 | `3fbac38` | `main` |
| 11 | 2026-06-22 | Btree 并发模型优化全部完成 | `816f7c5`, `1462712` | `main` |
| 10 | 2026-06-22 | BlockDevice 重命名 + bcachefs 并发调研 + S3/Nfs 修复 | `4bd0ff5` | `main` |
| 9 | 2026-06-22 | Wave 3 Phase 1: btree-native Volume 迁移 | `29e3d3b` | `main` |
| 8 | 2026-06-21 | Wave 1 daemon 适配收尾 — volmountd WAL API 编译修复 | `4016a84` | `main` |
| 7 | 2026-06-21 | P1-1 btree split/merge — bcachefs 对齐实现 | `a41b0a9` | `main` |
| 6 | 2026-06-21 | bcachefs 对齐 - 4 个 SixLock 并发 bug 修复 | `e7d220d` | `main` |
| 5 | 2026-06-21 | P2 txn-optimizations: path cache + seq restart | `9e7e8b1` | `main` |
| 4 | 2026-06-21 | P0 delta: txn restart core — restart() + lockrestart_do! + sort_key level | `3ae9087` | `main` |
| 3 | 2026-06-21 | Phase C2: Alloc btree 对接 | `afd43ad` | `main` |
| 2 | 2026-06-21 | Phase 2: SnapshotTreeManager btree persistence + Volume integration | `54b5d72` | `main` |
| 1 | 2026-06-21 | Volume BtreeEngine 全面集成（Wave 5+6） | `154b50d` | `main` |
<!-- @@@/auto:session-history -->

---

## Notes

- Sessions are appended to journal files
- New journal file created when current exceeds 2000 lines
- Use `add_session.py` to record sessions