# Journal 模块 bcachefs 函数覆盖地图

> 状态：❓ 全部清除 — 129 个函数全部已验证
> 更新：2026-07-01
> 对应 bcachefs 路径：`/home/black/Documents/bcachefs-tools/fs/journal/`

---

## 统计总览

| 状态 | 数量 | 占比 |
|------|------|------|
| ✅ 已验证对齐 | 73 | 56.6% |
| ⚠️ 已知偏差 | 8 | 6.2% |
| ❓ 未验证 | 0 | 0% |
| ➖ 无 bcachefs 对应 | 48 | 37.5% |
| **合计** | **129** | **100%** |

### bcachefs 独有函数（volmount 无对应）

| 模块 | 数量 | 说明 |
|------|------|------|
| validate.c 余量 | ~32 | 不适用于 volmount 的 entry types（prio_ptrs/usage/clock/log/datetime/rewind 等）+ to_text 链 + static helpers |
| init.c 生命周期 | 10 | `_start/_exit/_init_early/_dev_*` 等（`set_replay_done` ✅，`_stop` 已对齐 → ✅） |
| write.c closure 链 | 0 | 已全部映射（5 ✅ 已对齐注释 / 11 ➖ 架构不适用） |
| sb.c | 2 | superblock 字段 ops |
| read.c 扫描/搜索 | 12 | `journal_peek_bucket`/`bsearch_head`/`walk_inuse` 等 |
| **总计** | ~**58** | |

---

## types.rs（62 个非测试函数）

### Part 1：自由函数

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 184 | `journal_error_check_stuck()` | `journal_error_check_stuck` | journal.c:209 | ✅ | 简化版（无 flags/ERO），语义等价 |
| 2419 | `extract_blacklist_entries()` | — | — | ➖ | volmount 特有工具函数 |

### Part 2：impl JournalBuf

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 296 | `fn free()` | — | — | ➖ | Rust 析构构造，bcachefs 中 bufs 是静态数组始终分配 |
| 310 | `fn reset_for_accepting()` | `__journal_entry_open_one` buf init | journal.c:391 | ✅ | 重置 buf 状态为 Accepting，语义等价 |
| 331 | `fn journal_buf_try_noflush()` | `journal_buf_try_noflush` | journal.h:191 | ⚠️ | 保守返回 false，因 tokio::Notify 无法检测 waiter |

### Part 3：impl JournalResState

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 394 | `fn new()` | `JOURNAL_ENTRY_CLOSED_VAL` | journal.h | ✅ | 初始化为关闭状态 |
| 401 | `fn read()` | `smp_load_acquire(&j->reservations.v)` | journal.c | ✅ | Acquire load — 语义等价 |
| 406 | `fn cur_entry_offset(v)` | `union journal_res_state.cur_entry_offset:22` | types.h:159 | ✅ | bits 0-21 提取 — 位布局一致 |
| 411 | `fn idx(v)` | `union journal_res_state.idx:2` | types.h:160 | ✅ | bits 22-23 提取 — 位布局一致 |
| 416 | `fn buf_count(v, idx)` | `journal_state_count` | journal.h:243 | ✅ | shift 公式一致（BUF0_COUNT_SHIFT=24, BUF_COUNT_BITS=10） |
| 426 | `fn try_reserve()` | `journal_res_get_fast` | journal.h:475 | ✅ | 核心 CAS 保留逻辑 |
| 465 | `fn release()` | `__bch2_journal_buf_put` | journal.h:395 | ✅ | atomic_sub 释放 |
| 480 | `fn close_entry()` | `__journal_entry_close_one` | journal.c:276 | ⚠️ | 功能已对齐；使用 loop CAS 而非 cmpxchg fallback |
| 499 | `fn open_entry()` | `__journal_entry_open_one` | journal.c:391 | ✅ | CAS open + 格式转换 |
| 528 | `fn align_idx_to_seq()` | 不变量 `idx ≡ (seq-1) & BUF_MASK` | journal.c | ✅ | bcachefs 不变量 |
| 537 | `fn is_closed()` | `__journal_entry_is_open` (相反) | journal.c:137 | ✅ | `offset >= CLOSED_VAL` = `!is_open()`，逻辑互补 |

### Part 4：impl BufArray

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 561 | `fn new()` | — | — | ➖ | Rust Vec 封装 |
| 569 | `fn get()` | — | — | ➖ | dead_code |
| 575 | `fn get_mut()` | — | — | ➖ | 内部访问器 |
| 581 | `fn get_all_mut()` | — | — | ➖ | dead_code |

### Part 5：impl JournalSlowpath

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 668 | `fn new()` | `bch2_fs_journal_alloc` slowpath 部分 | init.c:305 | ✅ | 初始化 bucket_seq/buckets 等慢路径字段 |
| 689 | `fn from_superblock()` | `bch2_fs_journal_init` 部分 | init.c:802 | ✅ | 从 sb 状态恢复慢路径字段 |

### Part 6：impl Journal — 构造函数 & 错误处理

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 863 | `fn new()` | `bch2_fs_journal_alloc` | init.c:305 | ✅ | 基础构造（Phase 4 已验证） |
| 897 | `fn create()` | `bch2_fs_journal_alloc` + `bch2_dev_journal_alloc` | init.c:305/263 | ✅ | 含 allocator 动态分配 |
| 915 | `fn from_superblock()` | `bch2_fs_journal_init` + `bch2_fs_journal_init_rw` | init.c:802/758 | ✅ | 从 sb 恢复（Phase 4 已验证） |
| 951 | `fn to_superblock_state()` | `bch2_journal_buckets_to_sb` | sb.c:176 | ✅ | 导出状态到 sb |
| 967 | `fn journal_error_set()` | `bch2_journal_error_set` | journal.c | ✅ | 已对齐 |
| 990 | `fn journal_error_check()` | `bch2_journal_error` | journal.h:365 | ✅ | 已对齐 |
| 1101 | `fn bch2_journal_error_set()` | `bch2_journal_set_error` | journal.c | ✅ | journal_error_set 的別名 |
| 1120 | `fn bch2_journal_error_check()` | `bch2_journal_error` | journal.h:365 | ✅ | 已对齐 |

### Part 6b：核心 Fastpath API

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1039 | `fn journal_res_get_fast()` | `journal_res_get_fast` | journal.h:475 | ✅ | 核心 CAS fastpath |
| 1083 | `fn bch2_journal_set_watermark()` | `bch2_journal_set_watermark` | reclaim.c:69 | ✅ | 已对齐 |
| 1091 | `fn watermark()` | — | — | ➖ | Rust 枚举 getter |
| 1140 | `fn journal_cur_seq()` | `journal_cur_seq` | journal.h:137 | ✅ | inline getter |
| 1149 | `fn add_entry()` | `bch2_journal_add_entry` | journal.h:338 | ✅ | 对齐 |
| 1170 | `fn journal_res_put()` | `bch2_journal_res_put` | journal.h:423 | ✅ | 对齐 |
| 1189 | `fn bch2_journal_set_commit_callback()` | — | — | ➖ | volmount 特有（tokio 异步回调） |
| 1199 | `fn bch2_journal_wake_up()` | `journal_wake` | journal.h:118 | ⚠️ | volmount 含 buf 状态转换（Closing→WriteSubmitted），bcachefs 仅 closure_wake_up |

### Part 6c：Flush flag 辅助

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1214 | `fn journal_set_needs_flush_write()` | `set_bit(JOURNAL_need_flush_write)` | init.c:628 | ✅ | AtomicBool:store(true, Release) 等价 set_bit |
| 1219 | `fn journal_clear_needs_flush_write()` | `clear_bit(JOURNAL_need_flush_write)` | write.c:1126 | ✅ | AtomicBool:store(false, Release) 等价 clear_bit |
| 1224 | `fn journal_needs_flush_write()` | `test_bit(JOURNAL_need_flush_write)` | write.c:848 | ✅ | AtomicBool:load(Acquire) 等价 test_bit |
| 1229 | `fn journal_update_flush_jiffies()` | `j->last_flush_write = jiffies` | init.c:610 | ✅ | 时间戳用 ms 记录，bcachefs 用 jiffies，均用于相对比较 |
| 1238 | `fn journal_last_flush_jiffies()` | `j->last_flush_write` (读) | — | ✅ | getter，语义等价 |
| 1248 | `fn bch2_journal_set_replay_done()` | `bch2_journal_set_replay_done` | init.c:619 | ✅ | 恢复→正常过渡（无 flags，设置 needs_flush_write） |
| 1260 | `fn bch2_fs_journal_stop()` | `bch2_fs_journal_stop` | init.c:438 | ✅ | 关闭 journal（flush + meta entry） |

### Part 6d：内部方法（private）

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1244 | `fn bch2_journal_update_last_seq()` | `bch2_journal_update_last_seq` | reclaim.c:422 | ✅ | 对齐 |
| 1262 | `fn journal_entry_open()` | `__journal_entry_open_one` | journal.c:391 | ✅ | 对齐 |
| 1300 | `fn journal_entry_close()` | `__journal_entry_close_one` | journal.c:276 | ✅ | 对齐 |
| 1314 | `fn wait_for_pending_drain()` | `journal_buf_wait` | journal.c:1034 | ⚠️ | 功能等价（等 refcount→0），volmount spin-wait vs bcachefs closure sleep-wait |
| 1334 | `fn find_free_buf()` | `__journal_entry_open_one` idx++ 模式 | journal.c:391 | ✅ | idx 推进 |

### Part 6e：Convenience API

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1373 | `fn append()` | — | — | ➖ | volmount 异步便利包装 |
| 1428 | `fn append_btree_root()` | — | — | ➖ | volmount 异步便利包装 |

### Part 6f：Bucket write / flush

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1568 | `fn bch2_journal_write()` | `bch2_journal_write` | write.c:819 | ✅ | 核心写入（async 替代 closure） |
| 1637 | `fn write_bufs_to_bucket()` | `bch2_journal_flush` 写入循环 | journal.c:1255 | ✅ | 写入循环部分 |
| 1699 | `fn bch2_journal_flush()` | `bch2_journal_flush` | journal.c:1255 | ✅ | 含 J2 flush data race fix |
| 1801 | `fn bch2_journal_flush_all()` | `bch2_journal_flush` | journal.c:1255 | ✅ | 委托 bch2_journal_flush，等价 bcachefs bch2_journal_flush |

### Part 6g：Read / Utilization / Bucket mgmt

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1726 | `fn utilization()` | — | — | ➖ | volmount 特有统计 |
| 1745 | `fn bch2_journal_read()` | `bch2_journal_read` | read.c:1156 | ✅ | 对齐（简化版） |
| 1788 | `fn bch2_journal_read_reverse()` | 反向扫描部分 `bch2_journal_read_device` | read.c:728-770 | ✅ | 对应 bcachefs 的"从末尾找最近有效 jset"阶段，volmount 拆分为独立函数（R6 rewind） |
| 1830 | `fn bch2_journal_entries_read()` | `bch2_journal_entries_read` | — | ✅ | 对齐 |
| 1937 | `fn update_bucket_seq()` | `ja->bucket_seq[cur_idx] = max(...)` | write.c:54,103,574 | ✅ | max() 确保多 entry 同 bucket 时取最高 seq |
| 1946 | `fn advance_dirty_idx()` | `bch2_journal_space_available` dirty_idx 推进 | reclaim.c:262,293-295 | ✅ | 使用回收完成后的 last_seq_ondisk 边界，避免过早回收 bucket |
| 1972 | `fn advance_dirty_idx_ondisk()` | `bch2_journal_update_last_seq_ondisk` | reclaim.c:453,297-299 | ✅ | 使用 last_seq_ondisk，对齐 |

### Part 6h：Reclaim + Slowpath

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 1918 | `fn journal_seq_to_flush()` | `journal_seq_to_flush` | reclaim.c:861 | ✅ | 对齐 |
| 1933 | `fn journal_reclaim_needed()` | `__bch2_journal_reclaim` 触发条件 | reclaim.c:1047 | ✅ | 对齐 |
| 1967 | `fn __bch2_journal_reclaim()` | `__bch2_journal_reclaim` | reclaim.c:1047 | ✅ | 对齐（async 简化版） |
| 2056 | `fn bch2_journal_reclaim()` | `bch2_journal_reclaim` | reclaim.c:1184 | ✅ | 前台入口 |
| 2074 | `fn bch2_journal_flush_pins()` | `bch2_journal_flush_pins` | reclaim.c:1399 | ✅ | 对齐 |
| 2087 | `fn bch2_journal_rotate_or_reclaim()` | `bch2_journal_rotate_or_reclaim` | — | ✅ | 对齐 |
| 2131 | `fn bch2_journal_seq_blacklist_add()` | `bch2_journal_seq_blacklist_add` | seq_blacklist.c:49 | ✅ | 对齐 |
| 2182 | `fn bch2_journal_space_available()` | `bch2_journal_space_available` | reclaim.c:262 | ✅ | 对齐 |
| 2287 | `fn journal_cycle_locked()` | `bch2_journal_cycle_locked` | journal.c:636 | ⚠️ | 无 flags 系统，始终 close+open vs bcachefs 条件性基于 flush_waiters 和 flags |
| 2232 | `fn journal_res_get_slowpath()` | `bch2_journal_res_get_slowpath` | journal.c:958 | ✅ | 三级 fallback |
| 2384 | `fn journal_res_get()` | `bch2_journal_res_get` | journal.h:521 | ✅ | fast→slow 路径，结构一致 |
| 2309 | `fn set_auto_flush_interval()` | — | — | ➖ | volmount 特有 |
| 2314 | `fn auto_flush_interval()` | — | — | ➖ | volmount 特有 |
| 2328 | `fn spawn_auto_flush_task()` | — | — | ➖ | volmount 特有（tokio） |
| 2469 | `fn spawn_background_reclaim_task()` | `bch2_journal_reclaim_thread` | reclaim.c:1216 | ✅ | tokio::spawn 版 kthread，核心逻辑一致 |

---

## reclaim.rs（38 个函数）

### 自由函数

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 64 | `journal_pin_type()` | `journal_pin_type` | reclaim.c:564 | ✅ | 对齐 |
| 70 | `btree_level_pin_type()` | `journal_pin_type` 的 level 分类来源 | reclaim.c:564-577 | ✅ | leaf(level=0)→Btree0，level>=3→Btree3 |
| 72 | `usize_to_pin_type()` | — | — | ➖ | 内部转换 |
| 669 | `journal_pin_active()` | `journal_pin_active` | reclaim.h:67 | ✅ | 对齐 |

### impl Link（侵入式链表节点）

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 101 | `fn new()` | `list_head` 初始化 | — | ➖ | Rust 侵入式链表 |
| 109 | `fn read_prev()` | `le_prev` 读取 | — | ➖ | 链表访问 |
| 114 | `fn read_next()` | `le_next` 读取 | — | ➖ | 链表访问 |
| 119 | `fn write_prev()` | 指针赋值 | — | ➖ | 链表操作 |
| 126 | `fn write_next()` | 指针赋值 | — | ➖ | 链表操作 |
| 133 | `fn remove()` | `list_del_init` | — | ✅ | 对齐 |
| 147 | `fn insert_after()` | `list_add` | — | ✅ | 对齐 |
| 160 | `fn append_to_tail()` | `list_add_tail` | — | ✅ | 对齐 |
| 173 | `fn is_linked()` | `!hlist_unhashed` | — | ➖ | 检查 |

### impl PinPtrExt + Iterator

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 184 | `fn link()` | — | — | ➖ | Rust 安全抽象 |
| 201-289 | `impl LinkedListHead`（8 个方法） | `list_head` 操作 | — | ➖ | Rust 链表封装 |
| 281 | `impl Iterator for LinkedListIter` | — | — | ➖ | Rust 迭代器 |

### impl JournalEntryPin / JournalEntryPinList

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 320 | `fn new()` | `journal_entry_pin` 初始化 | — | ➖ | 构造 |
| 329 | `fn is_active()` | `journal_pin_active` | reclaim.h:67 | ✅ | 对齐 |
| 390 | `fn new(count)` | `journal_pin_list_init` | reclaim.h:25 | ✅ | 对齐 |
| 412-431 | unflushed_ref/mut/flushed | `pin_list->unflushed[]/flushed` | — | ➖ | Rust 访问器 |

### impl PinListFifo

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 455 | `fn new()` | — | — | ➖ | Rust VecDeque 包装 |
| 464-516 | `len/is_empty/is_full/front/push_back/pop_front` | — | — | ➖ | FIFO 基本操作 |
| 521 | `fn entry_for_seq()` | `journal_seq_pin` | reclaim.h:72 | ✅ | 固定容量 FIFO 模索引等价 bcachefs fifo_entry |
| 533 | `fn entry_for_seq_mut()` | `journal_seq_pin` (mut) | reclaim.h:72 | ✅ | 可变版 |
| 548-633 | 过渡兼容 API（retain/drainable_*/drain_front/find_rev_idx/iter_all） | — | — | ➖ | 过渡期 API，待删除 |

### impl Journal（reclaim 方法）

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 688 | `fn bch2_journal_maybe_update_last_seq()` | `bch2_journal_maybe_update_last_seq` | reclaim.c:443 | ✅ | 对齐 |
| 705 | `fn pin_fifo_ref()` | — | — | ➖ | UnsafeCell 包装 |
| 710 | `fn journal_seq_pin()` | `journal_seq_pin` | reclaim.h:72 | ✅ | unwrap_or_else(panic) 等价 EBUG_ON |
| 717 | `fn maybe_seq_pin()` | `maybe_seq_pin` | reclaim.c:610 | ✅ | seq=0->None 等价 NULL |
| 737 | `fn journal_pin_drop_locked()` | `journal_pin_drop_locked` | reclaim.c:512 | ✅ | 对齐 |
| 782 | `fn bch2_journal_pin_drop()` | `bch2_journal_pin_drop` | reclaim.c:538 | ✅ | 对齐 |
| 820 | `fn journal_pin_set_locked()` | `bch2_journal_pin_set_locked` | reclaim.c:579 | ✅ | 对齐 |
| 856 | `fn bch2_journal_pin_set()` | `bch2_journal_pin_set` | reclaim.c:664 | ✅ | 对齐 |
| 910 | `fn bch2_journal_pin_copy()` | `bch2_journal_pin_copy` | reclaim.c:615 | ✅ | 对齐 |
| 967 | `fn bch2_journal_pin_add()` | `bch2_journal_pin_add` | reclaim.h:106 | ✅ | 对齐 |
| 985 | `fn bch2_journal_pin_update()` | `bch2_journal_pin_update` | reclaim.h:119 | ✅ | 对齐 |
| 1004 | `fn __bch2_journal_pin_put()` | `__bch2_journal_pin_put` | reclaim.h:93 | ✅ | 对齐 |
| 1023 | `fn journal_get_next_pin()` | `journal_get_next_pin` | reclaim.c:729 | ✅ | 对齐 |
| 1094 | `fn journal_flush_pins()` | `journal_flush_pins` | reclaim.c:774 | ✅ | 对齐（精简版） |
| 1147 | `fn bch2_journal_pin_flush()` | `bch2_journal_pin_flush` | reclaim.c:713 | ✅ | 对齐 |
| 1168 | `fn journal_reclaim_kick()` | `journal_reclaim_kick` | reclaim.h:10 | ✅ | 对齐 |

---

## replay.rs（17 个函数）

### impl JournalReplayer

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 58 | `fn new()` | — | — | ➖ | Rust 构造 |
| 69 | `fn from_jsets()` | — | — | ➖ | 测试用构造 |
| 84 | `fn get_jsets()` | — | — | ➖ | 内部获取 |
| 95 | `fn replayed_seqs()` | — | — | ➖ | getter |
| 105 | `fn replay_from()` | — | — | ➖ | 便利接口 |
| 121 | `fn replay_all()` | — | — | ➖ | 便利接口 |
| 129 | `fn replay_all_to_engine()` | — | — | ➖ | 便利接口 |
| 144 | `fn replay_accounting_to_engine()` | `bch2_journal_replay` Phase 1 | read.c:1156 | ✅ | 两阶段重放第一阶段 |
| 172 | `fn replay_data_to_engine()` | `bch2_journal_replay` Phase 2 | read.c:1156 | ✅ | 两阶段重放第二阶段 |
| 199 | `fn apply_accounting_entries()` | —（recovery.c `bch2_journal_replay` 一阶段） | — | ➖ | volmount 层 replay wrapper，直接调 BtreeEngine |
| 227 | `fn apply_data_entries()` | —（recovery.c `bch2_journal_replay` 二阶段） | — | ➖ | volmount 层 replay wrapper |
| 263 | `fn apply_jset_to_engine()` | — | — | ➖ | 旧接口，保留兼容 |
| 280 | `fn read_btree_roots()` | — | — | ➖ | volmount 特有：replay 前需先提取 roots |
| 304 | `fn parse_jset()` | — | — | ➖ | 内部数据转换到 ReplayedEntry |

---

## validate.rs（8 个函数）

### 自由函数

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 39 | `jset_validate()` | `bch2_jset_validate` | validate.c:694 | ✅ | 完整 Jset 校验（version/csum/seq/entry 循环） |
| 73 | `journal_entry_validate()` | `bch2_journal_entry_validate` | validate.c:639 | ✅ | 逐 entry dispatch |
| 87 | `btree_keys_validate()` | `journal_entry_btree_keys_validate` | validate.c:115 | ✅ | btree_keys 可反序列化 |
| 95 | `btree_root_validate()` | `journal_entry_btree_root_validate` | validate.c:168 | ✅ | 恰好 1 个 BtreeEntry |
| 107 | `blacklist_validate()` | `journal_entry_blacklist_validate` | validate.c:225 | ✅ | start_seq < end_seq |
| 118 | `overwrite_validate()` | `journal_entry_overwrite_validate` | validate.c:483 | ✅ | payload 非空 |
| 124 | `btree_node_rewrite_validate()` | `journal_entry_write_buffer_keys_validate` (适配) | validate.c:517 | ✅ | payload 非空 |

## jset.rs（9 个函数）

### impl Jset

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 176 | `fn new()` | — | — | ➖ | Rust struct 构造，bcachefs 中 jset 在预分配 buffer 中隐式创建 |
| 191 | `fn new_volatile()` | — | — | ➖ | volmount 特有魔数 |
| 210 | `fn verify()` | `bch2_jset_validate_early` | validate.c:748 | ⚠️ | 仅 CRC32C 校验，缺少逐 entry type validate 链 |
| 242 | `fn serialize_padded()` | `bch2_journal_write_checksum` + padding | write.c:736 | ⚠️ | CRC 计算等价，padding 非 bcachefs 直接对应 |
| 265 | `fn deserialize()` | — | — | ➖ | bincode 反序列化，bcachefs 用 struct 指针直接读 buffer |

### impl Crc32CHasher

| 行号 | volmount 函数 | bcachefs 对应 | bcachefs 位置 | 状态 | 说明 |
|------|--------------|---------------|--------------|------|------|
| 135 | `fn new()` | — | — | ➖ | Rust 哈希包装 |
| 142 | `fn update()` | `crc32c_le_bch` | — | ➖ | 增量计算 |
| 147 | `fn finalize()` | — | — | ➖ | 完成计算 |
| 152 | `fn hash()` | `crc32c_le_bch` | — | ➖ | 单次计算 |

---

## bcachefs 独有函数（volmount 无直接对应）

### validate.c — 全量 entry type validate 链（Phase 3 完成部分覆盖）

volmount 新增 `validate.rs`（对应 bcachefs validate.c），实现了核心校验函数：

| bcachefs 函数 | 行号 | volmount 对应 | 状态 | 说明 |
|--------------|------|--------------|------|------|
| `bch2_jset_validate_early` | validate.c:748 | `Jset::verify()` | ✅ | Phase 1 已对齐（CRC + magic） |
| `bch2_jset_validate` | validate.c:694 | `jset_validate()` | ✅ | Phase 3 新增（完整校验） |
| `jset_validate_entries` | validate.c:662 | `jset_validate()` 内联循环 | ✅ | Phase 3 新增 |
| `bch2_journal_entry_validate` | validate.c:639 | `journal_entry_validate()` | ✅ | Phase 3 新增 dispatch |
| `journal_entry_btree_keys_validate` | validate.c:115 | `btree_keys_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_btree_root_validate` | validate.c:168 | `btree_root_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_blacklist_validate` | validate.c:225 | `blacklist_validate()` | ✅ | Phase 3 新增 |
| `journal_entry_overwrite_validate` | validate.c:483 | `overwrite_validate()` | ✅ | Phase 3 新增 |

余下 ~32 个 validate.c 函数（to_text 链 + 不适用于 volmount 的 entry types）仍无对应：

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `journal_entry_prio_ptrs_validate` / `_to_text` | validate.c:210/220 | 优先级 ptrs — 无 allocator replicas |
| `journal_entry_usage_validate` | validate.c:294 | usage entry — 无计数统计 |
| `journal_entry_data_usage_validate` | validate.c:328 | data usage — 无统计 |
| `journal_entry_clock_validate` | validate.c:370 | clock entry — 无时钟同步 |
| `journal_entry_dev_usage_validate` | validate.c:410 | dev usage — 无统计 |
| `journal_entry_log_validate` | validate.c:466 | log entry — 无日志系统 |
| `journal_entry_write_buffer_keys_validate` | validate.c:517 | write_buffer_keys — 无 write buffer |
| `journal_entry_datetime_validate` | validate.c:533 | datetime entry — 无写时 datetime |
| `journal_entry_rewind_limit_validate` | validate.c:564 | rewind limit — 未实现 rewind |
| `journal_entry_rewind_validate` | validate.c:593 | rewind entry — 未实现 rewind |
| `bch2_journal_entry_to_text` | validate.c:651 | to_text 格式化 |
| 另含 ~17 个 `static` helpers | validate.c | 内部辅助 |

**总计：~32 个函数无 volmount 对应（不适用）—— 不再计划对齐**

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `journal_entry_btree_keys_validate` | validate.c:115 | 验证 btree_keys entry |
| `journal_entry_btree_keys_to_text` | validate.c:139 | 格式化输出 |
| `journal_entry_btree_root_validate` | validate.c:168 | 验证 btree_root entry |
| `journal_entry_btree_root_to_text` | validate.c:204 | 格式化输出 |
| `journal_entry_prio_ptrs_validate` | validate.c:210 | 验证 priority ptrs |
| `journal_entry_prio_ptrs_to_text` | validate.c:220 | 格式化输出 |
| `journal_entry_blacklist_validate` | validate.c:225 | 验证 blacklist entry |
| `journal_entry_blacklist_to_text` | validate.c:243 | 格式化输出 |
| `journal_entry_usage_validate` | validate.c:294 | 验证 usage entry |
| `journal_entry_data_usage_validate` | validate.c:328 | 验证 data_usage entry |
| `journal_entry_clock_validate` | validate.c:370 | 验证 clock entry |
| `journal_entry_dev_usage_validate` | validate.c:410 | 验证 dev_usage entry |
| `journal_entry_log_validate` | validate.c:466 | 验证 log entry |
| `journal_entry_overwrite_validate` | validate.c:483 | 验证 overwrite entry |
| `journal_entry_write_buffer_keys_validate` | validate.c:517 | 验证 write_buffer_keys |
| `journal_entry_datetime_validate` | validate.c:533 | 验证 datetime entry |
| `journal_entry_rewind_limit_validate` | validate.c:564 | 验证 rewind_limit |
| `journal_entry_rewind_validate` | validate.c:593 | 验证 rewind entry |
| `bch2_journal_entry_validate` | validate.c:639 | 全量调度的入口 |
| `bch2_journal_entry_to_text` | validate.c:651 | 通用 to_text 入口 |
| `bch2_jset_validate` | validate.c:694 | jset 整体验证 |
| `bch2_jset_validate_early` | validate.c:748 | 早期验证（CRC + magic） |
| 另加 ~17 个 `static` helpers | validate.c | 内部辅助 |

**总计：41 个函数。volmount 中无对应。——→ 计划 Phase 3 处理**

### init.c — 生命周期函数

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `bch2_set_nr_journal_buckets` | init.c:188 | 设置 journal 桶数 |
| `bch2_dev_journal_bucket_delete` | init.c:201 | 删除 journal 桶 |
| `bch2_dev_journal_alloc` | init.c:263 | 设备 journal 分配 |
| `bch2_fs_journal_alloc` | init.c:305 | FS journal 分配 |
| `bch2_journal_pin_fifo_resize` | init.c:344 | 调整 pin fifo |
| `bch2_dev_journal_stop` | init.c:432 | 停止设备 journal |
| `bch2_fs_journal_stop` | init.c:438 | 停止 FS journal |
| `bch2_fs_journal_start` | init.c:487 | 启动 FS journal |
| `bch2_journal_set_replay_done` | init.c:619 | 标记重放完成 |
| `bch2_dev_journal_exit` | init.c:635 | 设备 journal 退出 |
| `bch2_fs_journal_exit` | init.c:708 | FS journal 退出 |
| `bch2_fs_journal_init_early/rw/init` | init.c:738/758/802 | 初始化三阶段 |

**总计：16 个函数。volmount 中 partial（Journal::new/create/from_superblock）。——→ 计划 Phase 4 处理**

### write.c — Closure 回调链（Phase 2 已完成）

volmount 用 3 个 async 函数（`bch2_journal_write`、`write_bufs_to_bucket`、`bch2_journal_flush`）覆盖了 bcachefs write.c 的 16 个 closure 链函数。架构差异（async/await 替代 closure/bio、单设备替代多设备 replicas）导致多数函数无直接 1:1 对应。

| bcachefs 函数 | 行号 | volmount 映射 | 状态 | 说明 |
|--------------|------|--------------|------|------|
| `journal_advance_devs_to_next_bucket` | write.c:29 | `bch2_journal_write` Phase 2 注释 | ✅ | bucket 旋转逻辑等价（无需 per-device 推进） |
| `__journal_write_alloc` | write.c:59 | 合并入旋转逻辑 | ➖ | 多设备 extent 分配器，volmount 单设备 |
| `journal_write_alloc` | write.c:112 | 合并入旋转逻辑 | ➖ | 同上，入口封装 |
| `journal_buf_realloc` | write.c:161 | — | ➖ | volmount 预分配 BufArray，无需运行时重分配 |
| `replicas_refs_put` | write.c:191 | — | ➖ | 副本引用管理，volmount 单设备无 replicas |
| `last_uncompleted_write_seq` | write.c:224 | — | ➖ | closure 链状态跟踪，async 等效内联 |
| `journal_write_done` | write.c:234 | `bch2_journal_flush` cleanup 注释 | ✅ | cleanup 逻辑等价（bucket_seq 更新 + pin_put + reclaim kick） |
| `journal_write_done_flush` | write.c:468 | 合并入 done 路径 | ➖ | 分离 flush 后处理，volmount 统一路径 |
| `journal_write_endio` | write.c:490 | — | ➖ | bio 完成回调，async/await 替代 |
| `journal_write_submit` | write.c:513 | `bch2_journal_write` Phase 4 注释 | ✅ | block 提交逻辑等价（无 FUA/PREFLUSH 标志位） |
| `journal_write_preflush` | write.c:585 | 合并入 backend.flush() | ➖ | 分离预 flush 回调，volmount 直接 flush |
| `bch2_journal_write_prep` | write.c:621 | `write_bufs_to_bucket` Phase 1 注释 | ✅ | prep 逻辑等价（数据截断 + 回调触发 + 状态转换） |
| `bch2_journal_write_checksum` | write.c:736 | `Jset::serialize_padded()` | ➖ | checksum 在序列化时已完成，非写路径 |
| `bch2_journal_write` | write.c:819 | `bch2_journal_write` (types.rs:1568) | ✅ | 核心写入已对齐 |
| `bch2_journal_do_writes_locked` | write.c:1087 | — | ➖ | deferred work 调度，async 直接执行 |
| `bch2_journal_do_writes` | write.c:1164 | — | ➖ | 加锁版调度，volmount 直接调用 write_bufs_to_bucket |

**小计：5 ✅ / 11 ➖。全部 16 个函数已映射。**

### sb.c — Superblock 交互

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `bch2_journal_buckets_to_sb` | sb.c:176 | journal 桶→sb 序列化 |
| `bch2_sb_journal_sort` | sb.c:227 | sb 桶排序 |
| 另含 5 个 static 验证函数 | sb.c:22-171 | sb 字段验证 |

### seq_blacklist.c — 黑名单辅助

volmount 已实现 `bch2_journal_seq_blacklist_add`（✅），缺少：

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `bch2_journal_seq_next_blacklisted` | seq_blacklist.c:114 | 查找下一个黑名单 |
| `bch2_journal_seq_next_nonblacklisted` | seq_blacklist.c:132 | 查找下一个非黑名单 |
| `bch2_journal_seq_is_blacklisted` | seq_blacklist.c:152 | 检查是否被黑名单 |
| `bch2_journal_last_blacklisted_seq` | seq_blacklist.c:179 | 获取最后一个黑名单 |
| `bch2_blacklist_table_initialize` | seq_blacklist.c:189 | 初始化黑名单 |
| `bch2_blacklist_entries_gc` | seq_blacklist.c:276 | 黑名单 GC |

### read.c — 设备扫描/搜索

volmount `bch2_journal_read()` 是简化版（从指定 bucket_idx 读取），缺少全量设备扫描：

| bcachefs 函数 | 行号 | 说明 |
|--------------|------|------|
| `journal_read_bucket` | read.c:331 | 读取单个桶 |
| `journal_peek_bucket` | read.c:473 | 只 peek 第一个 block |
| `journal_anchor_bucket` | read.c:523 | 二分查找第一个非空桶 |
| `journal_bsearch_head` | read.c:561 | 二分搜索 write head |
| `journal_walk_inuse` | read.c:609 | 遍历活跃桶 |
| `journal_bsearch_collect` | read.c:663 | 二分搜索收集桶 |
| `bch2_journal_read_device` | read.c:724 | 读取设备 journal |
| `bch2_journal_reread_for_rewind` | read.c:1067 | 为 rewind 重读 |

---

## 阶段计划

| 阶段 | 聚焦 | ❓ 变化 |
|------|------|--------|
| Phase 1 | 覆盖地图 + 注释修复 + 首次小修复 | ❓ 47 → 47（基线） |
| Phase 2 | 写路径深对齐（write.c） — **已完成** | ❓ 47 → 47（16 个 write.c 函数全部映射：5 ✅ / 11 ➖） |
| Phase 3 | 校验路径对齐（validate.c） — **已完成** | ❓ 47 → 38（+9 个新对齐函数） |
| Phase 4 | 初始化/生命周期对齐（init.c） — **已完成** | ❓ 38 → ~26 |
| Phase 5 | Superblock 交互对齐（sb.c） — **已完成** | ❓ ~26 → ~24 |
| Phase 6 | ❓ 全量验证 — **已完成** | ❓ ~24 → 0 ✅ |

✅ **❓ 全部清除。所有 128 个函数已验证完毕。**

---

## 更新日志

| 日期 | 变更 | 原因 |
|------|------|------|
| 2026-06-30 | 初始创建 | Phase 1 基线 |
| 2026-06-30 | 写路径注释修正 + validate.rs 创建 | Phase 2（write.c 注释修正）+ Phase 3（validate.c 校验链）：新增 validate.rs（8 函数），接入读路径；coverage +9 ✅，❓ 47→38 |
| 2026-07-01 | write.c closure 链映射回补 + Part 6f 行号修正 | write.c 16 个函数全部映射：5 ✅ / 11 ➖；行号调整（Phase 4 漂移）；Part 6f 行号 1500→1568 等 4 处修正 |
| 2026-07-01 | ❓ 全量清除 | 全部 ~33 ❓ 验证完成：28 ✅ / ⚠️ 5 / ➖ 5 → ❓ 0 ✅ |
