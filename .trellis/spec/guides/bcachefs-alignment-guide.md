# bcachefs 对齐验证指南

> **目的**：防止"声称对齐但实际未验证"的问题，确保所有模块的 bcachefs 对齐声明可追溯、可验证。

---

## 问题

volmount 多个模块声明"对齐 bcachefs"（`n 对齐`、`对应 bcachefs`、`对齐 bcachefs`），但**声明不验证等于没对齐**。

SixLock 的教训：
- C1 竞态：push_waiter 外 trylock = 没看 bcachefs wait_lock 内重试协议
- C2 位置：should_sleep 在入队前调 = 没看 bcachefs 实际调用位置(line 637)

**根因**：写"对齐"时没有先读 bcachefs 源码确认。

---

## 验证清单

### 写前必做

- [ ] **找到对应源码** — 在 `/home/black/Documents/bcachefs-tools/fs/` 中找到对应函数/文件
- [ ] **理解完整上下文** — 不只读目标行，读前后 30 行确认调用链和并发约束
- [ ] **记录参考位置** — 在注释中写 `对应 bcachefs <file>:<line>`，不写模糊的"对齐"

### 写中必做

- [ ] **确认语义等价** — Rust 抽象是否改变了语义？（`Mutex` vs `raw_spinlock_t`、`Atomic` vs `atomic_t`）
- [ ] **确认函数选择正确** — 不能用 `try_lock_write()` 的地方用了 `try_lock_write_preset()`？
- [ ] **确认边界条件** — bcachefs 的死锁回滚、错误路径是否都有映射？

### 写后必做

- [ ] **测试通过** — `cargo test -p volmount-core` 通过
- [ ] **spec 更新** — 如果学到了新的约束，更新对应模块的 spec
- [ ] **函数覆盖地图更新** — 被修改模块的 spec 中 `bcachefs 函数覆盖地图` 对应条目同步更新

### 切换模块前必做

- [ ] **已修改的模块的覆盖地图已更新** — 每个修改过的函数标 ✅（已验证）或 ⚠️（已知偏差）
- [ ] **目标模块的覆盖地图已读取** — 确认 ❓ 数量，了解未验证风险

---

## 模块 → 参考源码映射

所有比较文档/注释中的 `n` 缩写均指 bcachefs。

源码根路径：`/home/black/Documents/bcachefs-tools/fs/`。以下路径均为相对此根路径。

| volmount 模块 | bcachefs 参考文件 | 说明 |
|---|---|---|---|
| `lock/six.rs` | `util/six.c` (1109 行) + `util/six.h` (536 行) | 核心六锁实现（atomic bitfield + percpu reader）。关键流：SET_WAITING(line 590)→TRYLOCK(line 591)→ENQUEUE(line 598) 全部在 wait_lock 内原子。should_sleep_fn 仅在 park 循环(line 637)调，不在入队前。**wakeup 路径**：`six_lock_wakeup`(line 412-424) + `__six_lock_wakeup`(line 316-410)。两个关键区别：(1) `six_lock_wakeup` 有 `write + held_read → skip` 检查 (2) `__six_lock_wakeup` handoff 失败 via `goto out` 不清 WAITING bit |
| `lock/deadlock.rs` | `btree/locking.c` + `btree/locking.h` | btree-level 锁排序 + 死锁检测。`bch2_six_check_for_deadlock`(locking.c:783) 是实际 should_sleep_fn 回调 |
| `lock/wait_fifo.rs` | `util/fifo.h`（通用 FIFO）+ `six.c`（six_lock_waiter） | 等待队列，six.c 中内嵌使用 |
| **btree 整体** | `btree/` 目录 | 全部 btree 模块。**注意命名差异**：bcachefs C 文件无 `btree_` 前缀（`iter.c`, `read.c`, `write.c`, `commit.c` 等），而 volmount Rust 文件有 `btree_` 前缀（`btree_iter.rs`, `btree_io.rs` 等） |
| `btree/btree.rs` | `btree/init.c` + `btree/types.h` | Btree 主结构（bch_fs 中的 btree 实例） |
| `btree/types.rs` | `btree/types.h` + `bkey_types.h` | 共享类型（bpos, bkey, btree_path_level） |
| `btree/key.rs` | `btree/bkey.h` + `btree/bkey.c` + `bkey_types.h` | bkey 打包/解包/bpos 操作 |
| `btree/node.rs` | `btree/bset.h` + `btree/bset.c` | BtreeNode + bset 布局 + 辅助搜索树 |
| `btree/iter.rs` | `btree/iter.c` + `btree/iter.h` | BtreeIter 遍历器 |
| `btree/transaction.rs` | `btree/commit.c` + `btree/update.h` | BtreeTrans 事务 |
| `btree/io.rs` | `btree/read.c` + `btree/write.c` + `btree/io.h` | btree 节点 I/O 读写 |
| `btree/cache.rs` | `btree/cache.c` + `btree/cache.h` | btree 节点缓存 + eviction |
| `btree/write_buffer.rs` | `btree/write_buffer.c` + `btree/write_buffer.h` | 写缓冲区 flush |
| `btree/key_cache.rs` | `btree/key_cache.c` + `btree/key_cache.h` | key cache（hash 表 + per-entry 锁） |
| `btree/interior.rs` | `btree/interior.c` + `btree/interior.h` | 内部节点操作（split/merge/rewrite/set_root） |
| `btree/update.rs` | `btree/update.c` + `btree/update.h` | btree interior update state machine |
| `btree/search.rs` | `btree/iter.c`（`bch2_btree_iter_traverse`） | 搜索优先级 + 路径下降 |
| `btree/trigger.rs` | `btree/commit.c`（triggers）+ 各 `*_trigger.c` | 触发器注册与执行 |
| `btree/gc.rs` | `btree/check.c` + `check.h`（无独立 gc.c） | GC 遍历 + 一致性检查 |
| `btree/node_scan.rs` | `btree/node_scan.c` + `btree/node_scan.h` | 设备 btree 节点扫描 |
| `btree/mod.rs` | `btree/types.h`（BTREE_ID 枚举） | BtreeId + subvol_ino_map |
| **alloc 整体** | `alloc/` 目录 | 全部 alloc 模块 |
| `alloc/mod.rs` | `alloc/foreground.c` + `alloc/background.h` | BchAllocator 主结构 + 分配入口 |
| `alloc/bucket.rs` | `alloc/background.c` + `alloc/types.h` + `alloc/format.h` | Bucket 状态管理 + bch_alloc_v4 |
| `alloc/foreground.rs` | `alloc/foreground.c` + `alloc/foreground.h` | 前台分配 + alloc_prio_hint |
| `alloc/background.rs` | `alloc/background.c` + `alloc/background.h` | 后台 GC 分配 |
| `alloc/btree.rs` | `alloc/background.c` + `buckets.c` | Alloc btree 操作 |
| `alloc/open_bucket.rs` | `alloc/foreground.h`（open_bucket 结构）+ `alloc/types.h` | 开放桶引用计数 |
| `alloc/reservation.rs` | `alloc/buckets.h`（`disk_reservation`） | 扇区预留系统 |
| `alloc/write_point.rs` | `alloc/foreground.c`（`write_point`）+ `alloc/types.h` | 写点管理 |
| **journal 整体** | `journal/` 目录 | 全部 journal 模块 |
| `journal/types.rs` | `journal/types.h` + `journal.h` | Journal 类型 + 状态 |
| `journal/mod.rs` | `journal/journal.c` + `journal/journal.h` | Journal 核心（buf/commit/flush） |
| `journal/reclaim.rs` | `journal/reclaim.c` + `journal/reclaim.h` | Journal 回收 |
| `journal/replay.rs` | `journal/read.c` + `init/recovery.c`（调用方） | Journal 回放 |
| `journal/jset.rs` | `journal/types.h`（jset 结构）+ `bcachefs_format.h` | Journal entry 格式 |
| `subvol/` | `snapshots/subvolume.c` + `snapshots/subvolume.h` | 子卷管理 |
| `snap/` | `snapshots/snapshot.c` + `snapshots/snapshot.h` + `snapshots/check_snapshots.c` | 快照 skip_list + 一致性检查 |
| `recovery/` | `init/recovery.c` + `init/passes.c` + `init/passes.h` | 崩溃恢复框架 + pass 调度 |
| `super_block/` | `sb/` 目录（`sb/members.c`, `sb/clean.c`, `sb/io.c` 等） | Superblock 管理 |
| `volume/` | `init/fs.c`（fs lifecycle, BCH_FS_* flags） | 卷生命周期管理 |

### 覆盖地图状态（2026-06-30 更新）

| 模块 | 覆盖地图文件 | ✅+⚠️ 覆盖率 | ❓ 未验证 |
|------|-------------|-------------|-----------|
| lock/six | `lock-concurrency.md` | 100% | 0 (0%) |
| btree/transaction | `btree-transaction.md` | 100% | 0 (0%) |
| alloc | `alloc-coverage.md` | 64.6% | 0 |
| journal | `journal-coverage.md` | 58% | 7 (6%) |
| snap | `snap-coverage.md` | 36.1% | — |
| subvol | `subvol-coverage.md` | 57.7% | — |
| recovery | `recovery-coverage.md` | 97.4% | — |
| volume | `volume-coverage.md` | 68.2% | — |
| btree/io | `btree-io-coverage.md` | 88.9% | 3 (11.1%) |
| btree/cache | `btree-cache-coverage.md` | 90% | 10 |
| `block_device/` | `data/checksum.h` + `data/io_misc.c` | 块设备 + 校验和 |

---

## 正确做法 vs 错误做法

### 正确

```rust
// 对应 bcachefs __six_lock_slowpath line 637
// 在 FIFO 入队（line 598）之后的 park 循环内调 should_sleep_fn
```

包含：文件名 + 行号 + 逻辑顺序描述

### 错误

```rust
// 对齐 bcachefs——入队前先调 should_sleep
```

不包含：无行号、无验证、顺序错误

---

## 发现不一致的修正流程

```
发现"对齐"声明 → 读 bcachefs 对应源码 → 确认是否一致
  一致 → 补上行号，确认 ✅
  不一致 → 修改实现对齐 bcachefs
          → 在 commit 中说明差异细节
          → 更新 spec（学到了什么）
          → 更新本清单的常见误区表
```

---

## 函数级覆盖地图

> 声明对齐的每个模块必须在自己的 spec 中维护一张函数级覆盖地图。
> **文件级映射只告诉你"去哪个文件找"，函数级映射才知道"哪些已经验过、哪些还没验"。**

### 模板

每个声明对齐的模块在其 spec 文件中（如 `lock-concurrency.md`）添加以下表格：

```markdown
### bcachefs 函数覆盖地图

| 我们的函数 | bcachefs 对应 | 行号 | 状态 |
|-----------|--------------|------|------|
| `pub fn lock_write(&self)` | `do_six_lock_ip` | `six.c:528` | ✅ |
| `fn lock_slowpath(...)` | `__six_lock_slowpath` | `six.c:543` | ✅ |
| `fn try_lock_read(&self)` | `__do_six_trylock(Read)` | `six.c:70` | ❓ |
```

### 覆盖状态说明

| 状态 | 含义 | 要求 |
|------|------|------|
| ✅ | 已验证对齐 bcachefs | 注释含 bcachefs 行号 |
| ⚠️ | 已知偏差（Rust 特有抽象导致） | 偏差原因必须说明 |
| ❓ | 未验证—没对照过 bcachefs | 下次改此模块时优先验证 |
| ➖ | 无 bcachefs 对应（纯 Rust 新增） | 简短说明为什么没有 |

### 治理规则

- 每个模块修改前必须读覆盖地图，**❓ 数量是技术债指标**
- 每次修改后更新对应条目的状态（❓ → ✅）
- 一个模块的 ❓ 清零后才能声称"此模块已完成 bcachefs 对齐"
- 覆盖地图维护在模块的 spec 中（如 `lock-concurrency.md`），而非 guide 中（避免过长）

---

## 常见误区（持续更新）

| 模块 | 错误假设 | 事实 | 证据 |
|---|---|---|---|
| lock | should_sleep 在入队前调 | 在 park 循环内调 | `six.c:637` |
| lock | push_waiter 外 trylock 就行 | 必须 wait_lock 内重试 | `six.c:584-611` |
| lock | `trylock_ip` → `try_lock_write()` 在 WRITE_BIT 预设后有效 | 无效—has_write_lock 始终返回 true | `six.c:573` 预设后 line 591 不检查 HELD_write |
| lock | WAITING bit 和入队之间无其他操作 | 在 WAITING bit 设置(line 590)和入队(line 598)之间有 __do_six_trylock(line 591) | `six.c:590-598` |
| lock | wakeup 路径也对齐了 bcachefs | wakeup_lock_type/__wakeup_lock_type 是从原实现继承的，从未对照 six_lock_wakeup/__six_lock_wakeup | `six.c:316-424` — 两个差异：(1)six_lock_wakeup 外层有 write+held_read 检查(line 416-417) (2)__six_lock_wakeup handoff 失败不走 WAITING bit 清除(line 380-383 vs 400-402) |
| btree | bcachefs 文件有 `btree_` 前缀 | 无前缀，`btree/iter.c` 而非 `btree_iter.c` | 实际 `ls fs/btree/` |
| btree | `btree_gc.c` 是独立文件 | GC 在 `btree/check.c` | 实际 `ls fs/btree/` |
| transaction | journal 写入在 btree 修改之前 | `bch2_trans_commit(): journal_res_get → btree modify → journal_add_entry → journal_res_put`（先保留→再修改→最后填充） | `journal.h:journal_res_get_fast` + `commit.c:bch2_trans_commit()` |
| transaction | 一次 trans_commit = 一次 journal_res_get | trans_commit 的 journal 条目（可能跨多个 btree 组）打包为一个 Jset 写入一次保留空间 | btree/commit.c 中 `journal_res_get` + `__bch2_trans_commit` 之间按需预留精确大小 |
| transaction | Volume 级 pin 是必需的 | 节点级 pin（在 `bch2_btree_node_write` 时注册）已覆盖全部语义，Volume 级 pin 是冗余（volmount 特有，bcachefs 无对应） | `reclaim.c:bch2_journal_pin_add()` 在 `bch2_btree_node_write` 路径调用 |
| transaction/bcachefs 对齐 | `__bch2_trans_commit()` 需要对所有路径按 `(btree_id, pos, -level)` 排序才能避免死锁（`bch2_trans_sort_locks`） | bcachefs 排序是因为早期路径预分配后顺序不确定；但写锁升级（`bch2_trans_lock_write_inlined`）实际按 `trans_for_each_update` 遍历 journal 条目，而非 sorted paths。我们的 `try_lock_all()` 直接按 journal 顺序，与 bcachefs 实际写锁路径一致 | `commit.c:141-159` — `bch2_trans_lock_write_inlined` 遍历 `trans_for_each_update` |
| transaction | `try_lock_read()` try-fail 模式适合遍历路径 | bcachefs `six_lock_read()` 是阻塞的。try-fail 模式在 volmount 中引入不必要的重启开销，与 bcachefs 不一致 | `six.c` — `six_lock_read()` 包含完整 slowpath（should_sleep / park），不是 trylock |
| transaction | `sort_locks()` 是 `__bch2_trans_commit` 的必要步骤 | bcachefs 的 `__bch2_trans_commit()` 入口处不调用 sort_locks，它由调用者在需要时（如跨事务锁获取）手动调用。锁升级路径走 `bch2_trans_lock_write_inlined` 无排序 | `commit.c:141-159` — 写锁升级函数体 |
| transaction | BtreeTrans 需要 sort_locks 来保证加锁顺序一致性 | bcachefs 的 `bch2_trans_relock()` 遍历 `trans_for_each_path()` 不排序；`bch2_trans_lock_write_inlined` 遍历 journal 不排序。排序仅在外部锁获取时按需调用 | `locking.c:1487-1517` — `bch2_trans_relock` + `locking.c:1059` — `bch2_trans_sort_locks` |
| transaction | BtreeTransEntry 必须包含完整 old_key/old_value | bcachefs 的 `verify_update_old_key()` 在 commit 流程中从 btree 实时查找 old_key，无需调用者提供 | `commit.c:56-130` — `verify_update_old_key` 查 `bch2_btree_path_peek_slot` |
| transaction | `bch2_trans_commit_run_triggers` 在 retry 循环内部执行 | bcachefs 中 transactional triggers 在 retry 循环之前执行（line 1405-1407），与 lock 路径无关 | `commit.c:1390-1420` — trigger 执行在 `retry:` label 之前 |

---

## 核心原则

> **"对齐"不是标签，是承诺。写之前读源码，写之后注行号。**
