# SixLock API 对齐审计：volmount-core vs bcachefs 内核

> 审计日期：2026-06-24  
> 范围：`volmount-core/src/lock/six.rs` vs `bcachefs-tools/fs/util/six.{h,c}`  
> 参考：`bcachefs-tools/fs/btree/locking.h`（btree 集成接口）

---

## 1. 类型定义对比

### 1.1 锁类型枚举

| 维度 | bcachefs C | volmount Rust | 对齐状态 |
|------|-----------|---------------|----------|
| 枚举名 | `enum six_lock_type` | `SixLockType` | ✅ 概念对齐 |
| 值定义 | `SIX_LOCK_read=0, SIX_LOCK_intent=1, SIX_LOCK_write=2` | `Read=0, Intent=1, Write=2` | ✅ 值一致 |
| 存储 | C enum（int 大小） | `#[repr(u8)]` | ✅ |
| 额外类型 | 无 | `SixLockResult { Acquired, Busy, Deadlock }` | 🔶 Rust 新增（用于 lock_* 返回值） |

**结论**：完全对齐。注意 `SixLockResult` 是 Rust 端的扩展，C 端用 `int` 返回值表示（0=success, 负=错误）。

### 1.2 主锁结构体

| 字段 | bcachefs `struct six_lock` | volmount `SixLock` | 对齐 |
|------|---------------------------|---------------------|------|
| `state` | `atomic_t` (u32) | `AtomicU32` | ✅ |
| `seq` | `u32` | `AtomicU64` | ⚠️ **P2**: 类型不同 (u32 vs u64) |
| `readers` | `unsigned __percpu *` | `Option<Box<[AtomicU32]>>` | ✅ 概念对齐，实现不同 |
| `intent_lock_recurse` | `unsigned` | `UnsafeCell<u32>` | ✅ |
| `write_lock_recurse` | `unsigned` | `UnsafeCell<u32>` | ✅ |
| `owner` | `struct task_struct *` | 拆分为 `intent_owner` + `write_owner` | ⚠️ **P3**: Rust 扩展了 owner 追踪到写锁 |
| `wait_lock` | `raw_spinlock_t` | 无（由 RcuBox 内部处理） | ⚠️ **P2**: 同步策略不同 |
| `wait_fifo` | `struct six_lock_wait_fifo __rcu *` | `WaitFifo` | ✅ 概念对齐 |
| `inline_fifo` + `inline_fifo_data` | 内联 8 槽位 | 无 inline 预分配 | ⚠️ **P3**: Rust 无内联小队列优化 |
| `dep_map`（lockdep） | `struct lockdep_map` | 无 | 🔶 Rust 无 lockdep |
| `should_sleep_fn` | 不存储在结构体中（函数参数传递） | `Option<Arc<dyn Fn() -> bool + Send + Sync>>` | ⚠️ **P1**: 签名和存储方式差异大 |

**结论**：结构体字段大致对齐，但有若干重要差异。

### 1.3 等待者 / 队列结构体

| 类型 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 等待者 | `struct six_lock_waiter`（栈上分配，包含 `trans_start_time`, `task`, `lock_want`, `lock_acquired`, `slot_idx`） | `WaiterBox`（堆上 `Box`，包含 `trans_id`, `lock_type`, `seq`, `thread`） | ⚠️ **P1**: 设计差异显著 |
| 槽位 | `struct six_lock_wait_slot { w, start_time }` | `RcuBox<Option<Box<WaiterBox>>>` | ⚠️ 实现不同 |
| 队列 | `struct six_lock_wait_fifo`（RCU 指针，可扩容，tombstone 压缩） | `WaitFifo`（固定槽位 `Box<[RcuBox<...>]>`，resize 仅全量迁移） | ⚠️ **P2**: 扩容策略不同 |

**关键差异**：

1. **bcachefs 的 waiter 在栈上分配**：调用者提供 `struct six_lock_waiter wait`，嵌入到事务/路径结构中。Rust 版 waiter 在 `RcuBox` 堆上分配。
2. **bcachefs 有 `lock_acquired` 标志**：通过 `smp_store_release` / `smp_load_acquire` 与等待线程通信。Rust 版无此标志，通过 `thread::unpark()` / `thread::park()` 隐式同步。
3. **bcachefs 用 `trans_start_time` 实现严格的时间序排队**：相同 `lock_want` 的等待者按事务开始时间排序，最老的先被唤醒。Rust 版无时间序——`notify_waiters` 遍历所有匹配槽位，不保证顺序。
4. **bcachefs 的 slot 封装了 `start_time + lock_want` 的 packed 编码**：唤醒扫描时无需 deref waiter 即可过滤和排序。Rust 版每次扫描都要读 `WaiterBox` 的完整内容。

---

## 2. 函数签名对比

### 2.1 核心锁获取函数

| 语义 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 读锁（阻塞） | `six_lock_read()` → `int` | `lock_read()` → `bool` | ⚠️ **P3**: 返回类型不同 |
| 意图锁（阻塞） | `six_lock_intent()` → `int` | `lock_intent()` → `bool` | ⚠️ **P3** |
| 写锁（阻塞） | `six_lock_write()` → `int` | `lock_write()` → `bool` | ⚠️ **P3** |
| 读锁（try） | `six_trylock_read()` → `bool` | `try_lock_read()` → `bool` | ✅ |
| 意图锁（try） | `six_trylock_intent()` → `bool` | `try_lock_intent()` → `bool` | ✅ |
| 写锁（try） | `six_trylock_write()` → `bool` | `try_lock_write()` → `bool` | ✅ |
| 读解锁 | `six_unlock_read()` → `void` | `unlock_read()` → `void` | ✅ |
| 意图解锁 | `six_unlock_intent()` → `void` | `unlock_intent()` → `void` | ✅ |
| 写解锁 | `six_unlock_write()` → `void` | `unlock_write()` → `void` | ✅ |

### 2.2 通用参数化版本

| 语义 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 通用锁获取 | `six_lock_type(lock, type)` → `int` | 无（需手动 dispatch） | ⚠️ **P3** |
| 通用 trylock | `six_trylock_type(lock, type)` → `bool` | 无 | ⚠️ **P3** |
| 通用 relock | `six_relock_type(lock, type, seq)` → `bool` | 无（只有 `try_relock_read/intent`） | ⚠️ **P3** |
| 通用解锁 | `six_unlock_type(lock, type)` → `void` | 无（需手动 dispatch） | ⚠️ **P3** |

### 2.3 升级/降级操作

| 语义 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| intent→read 降级 | `six_lock_downgrade()` → `void` | `downgrade_intent_to_read()` → `void` | ✅ 概念对齐 |
| read→intent 升级 | `six_lock_tryupgrade()` → `bool` | `try_upgrade_read_to_intent()` → `bool` | ✅ 概念对齐 |
| 通用转换 | `six_trylock_convert(from, to)` → `bool` | 无 | ⚠️ **P2** |
| intent→write 升级 | 无独立函数（由 `__btree_node_lock_write` 实现） | `try_upgrade_intent_to_write()` + `upgrade_intent_to_write()` | 🔶 Rust 新增 |
| write→intent 降级 | 无独立函数 | `downgrade_write_to_intent()` | 🔶 Rust 新增 |
| 带自旋的升级 | 无对应函数 | `upgrade_read_to_intent()` / `upgrade_intent_to_write()` | 🔶 Rust 新增 |

### 2.4 重锁（Relock）API

| 语义 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 通用 relock | `six_relock_type(lock, type, seq)` → `bool` | 无 | ⚠️ **P3** |
| relock_read | `six_relock_read(lock, seq)` → `bool` | `try_relock_read(expected_seq)` → `bool` | ✅ |
| relock_intent | `six_relock_intent(lock, seq)` → `bool` | `try_relock_intent(expected_seq)` → `bool` | ✅ |
| relock_write | `six_relock_write(lock, seq)` → `bool` | 无 | ⚠️ **P2** |

### 2.5 Waitlist / 死锁检测接口

| 语义 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 带 waiter 的完全获取 | `six_lock_ip_waiter(lock, type, wait, should_sleep_fn, ip)` → `int` | 无（`lock_read/intent/write` 内部自建 waiter） | ⚠️ **P1** |
| 跳过初始 trylock | `six_lock_contended(lock, type, wait, should_sleep_fn, ip)` → `int` | 无 | ⚠️ **P1** |
| should_sleep_fn 签名 | `int (*)(struct six_lock *, struct six_lock_waiter *)` | `Fn() -> bool`（无参数） | ⚠️ **P1**: 缺少 lock/waiter 上下文 |

### 2.6 元操作接口

| 功能 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 初始化 | `six_lock_init(lock, flags, gfp)` | `SixLock::new()` / `with_percpu(n)` | ✅ |
| 销毁 | `six_lock_exit(lock)` | 无（Rust Drop 自动处理） | ✅ |
| 获取 seq | `six_lock_seq(lock)` → `u32` | `seq()` → `u64` | ⚠️ **P2**: 返回类型不同 |
| 增加持有计数 | `six_lock_increment(lock, type)` | 无（通过重入隐式支持） | ⚠️ **P2** |
| 读计数 | `six_lock_counts(lock)` → `struct six_lock_count { n[3] }` | `reader_count()` → `u32` | ⚠️ **P2**: 缺少 intent/write 计数 |
| 调整读者数 | `six_lock_readers_add(lock, nr)` | 无 | ⚠️ **P2** |
| 唤醒所有 | `six_lock_wakeup_all(lock)` | 无 | ⚠️ **P2** |
| 调试状态 | 无 | `debug_state()` → String | 🔶 Rust 新增 |

---

## 3. 调用约定对比

### 3.1 try → spin → sleep 三级等待

| 等级 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| Level 1: try | `try_lock → 成功直接返回` | `try_lock_* → 成功直接返回` | ✅ |
| Level 2: spin | `six_optimistic_spin() ~10μs (可选, 需CONFIG)` | `spin_lock_*_internal() ~1024次 + yield_now()` | ⚠️ **P2**: C 版仅在 waitlist 唯一时自旋，有严格超时；Rust 版无条件自旋固定次数 |
| Level 3: sleep | `push_waiter + schedule() / six_lock_slowpath` | `push_waiter + thread::park()` | ✅ 概念对齐 |

### 3.2 CAS 重试策略

| 方面 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| try_lock_read | CAS in loop (cmpxchg_acquire) | CAS in loop (compare_exchange_weak + spin_loop) | ✅ |
| CP 暂停指令 | `cpu_relax()` | `std::hint::spin_loop()` | ✅ |
| yield 后重试 | C 不走 yield，直接 schedule | Rust spin→yield→retry 作为 spin→sleep 的过渡 | ⚠️ **P2**: Rust 在 spin 和 sleep 之间加了 yield 阶梯 |
| nospin bit 检查 | `six_optimistic_spin` 中检查 | 所有 `spin_lock_*_internal` 入口检查 | ✅ |

### 3.3 写锁 sleep 路径的 WAITING_WRITE_BIT 预设

| 方面 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 预设时机 | `__six_lock_slowpath` 中先 `atomic_add(WRITE)` 再设置 WAITING bits | `lock_write()` 中 spin 失败后 `fetch_or(WAITING_WRITE_BIT)` | ✅ 概念对齐 |
| 预设后双检 | C 在 wait_lock 内 retry trylock | Rust 双检 `try_lock_write()` | ✅ |
| intent 等待位 | C 写者用 `SIX_LOCK_WAITING_write`，意图等复用 | Rust `set_waiting_bit(Intent)` 也设置 `WAITING_WRITE_BIT` | ✅ 一致 |

---

## 4. 等待位语义

| 位 | bcachefs C 定义 | volmount Rust 定义 | 对齐 |
|---|----------------|-------------------|------|
| read_count bits | `[0:25]` (26 bits) `SIX_LOCK_HELD_read` | `bits [0:25] = READ_COUNT_MASK (0x03FF_FFFF)` | ✅ |
| intent bit | `bit 26 = SIX_LOCK_HELD_intent` | `bit 26 = INTENT_BIT (0x0400_0000)` | ✅ |
| write bit | `bit 27 = SIX_LOCK_HELD_write` | `bit 27 = WRITE_BIT (0x0800_0000)` | ✅ |
| waiting_read | `bit 28 = SIX_LOCK_WAITING_read` | `bit 28 = WAITING_READ_BIT (0x1000_0000)` | ✅ |
| waiting_write | `bit 29 = SIX_LOCK_WAITING_write` | `bit 29 = WAITING_WRITE_BIT (0x2000_0000)` | ✅ |
| nospin | `bit 31 = SIX_LOCK_NOSPIN` | `bit 31 = NOSPIN_BIT (0x8000_0000)` | ✅ |
| waiting_intent | C 也使用 `SIX_LOCK_WAITING_write`（读者/意图者分开） | Rust 意图写者统一用 `WAITING_WRITE_BIT` | ✅ 一致 |

**结论**：位布局完全一致。这是一个重要的基础设施对齐——如果位布局不同，跨语言移植就不可能。

---

## 5. 死锁检测接口

| 方面 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 检测器触发点 | `bch2_six_check_for_deadlock(lock, waiter)` → `should_sleep_fn` | `should_sleep_fn` 闭包 || 参数 | 接收 `six_lock *` 和 `six_lock_waiter *`，可遍历 waitlist | 无参数，闭包无法访问锁内部状态 | ⚠️ **P1** |
| DFS 遍历 | 通过 `lock_graph`（per-CPU 固定深度 8 栈帧）+ RCU snapshots | `DeadlockDetector`（per-thread DFS，最大深度 64） | 🔶 设计差异 |
| 等待者收集 | 在 should_sleep_fn 内通过 RCU 遍历锁的 wait_fifo，拍下冲突事务快照 | 需要调用者预先收集 `WaiterInfo` 列表 | ⚠️ **P2** |
| 等待者信息 | `trans_waiting_for_lock`（路径、锁类型、层级全记录） | `WaiterInfo { trans_id, lock_id, waiting_for_trans_id }` | ⚠️ **P2** |

**关键差异**：

1. **C 版死锁检测与锁获取紧密耦合**：`should_sleep_fn` 接收 `(lock, waiter)` 指针，可在 schedule 前直接遍历 wait_fifo，获取当前最精确的锁依赖图。
2. **Rust 版检测器是解耦的**：`should_sleep_fn` 是无参闭包 `Fn() -> bool`，无法访问锁状态。死锁检测需要在 park 循环外部独立完成，将 `WaiterInfo` 列表传入。
3. **C 版使用 per-CPU lock_graph**（固定 8 层 DFS，RCU snapshot 防止并发变动），**Rust 版使用 per-thread DeadlockDetector**（动态 Vec 栈，最大 64 层）。
4. **C 版有 `six_lock_wakeup_all()`** 用于调试/唤醒被死锁检测器误判的等待者。Rust 版没有此设施。

---

## 6. Per-CPU Reader 快速路径

| 方面 | bcachefs C | volmount Rust | 对齐 |
|------|-----------|---------------|------|
| 启用方式 | `six_lock_init` 传入 `SIX_LOCK_INIT_PCPU` 标志 | `SixLock::with_percpu(num_slots)` | ✅ |
| 读者计数 | `unsigned __percpu *readers`（内核 percpu 变量） | `Box<[AtomicU32]>` 模拟 percpu | ⚠️ **P2**: 模拟而非真 percpu |
| 读锁快速路径 | `this_cpu_inc → smp_mb → check write bit` | `readers[slot].fetch_add + fence(Acquire) → check write bit` | ✅ 概念对齐 |
| 写锁路径 | `atomic_add(WRITE) → smp_mb → pcpu_read_count` | `CAS set WRITE_BIT → drain other slots` | ⚠️ **P2**: CAS vs atomic_add |
| 读锁回滚 | `this_cpu_dec + smp_mb + check WAITING_WRITE` | `readers[slot].fetch_sub(Relaxed)` | ✅ |
| 多线程 percpu | 每个 CPU 一个独立变量 | 线程 ID 取模到固定槽位 | ⚠️ **P2**: 可能伪共享 |

**关键差异**：

1. **C 版使用真实 percpu 变量**：每个 CPU 核都有独立计数器，完全无伪共享。Rust 版用固定大小的 `Box<[AtomicU32]>` 数组，线程通过 `NEXT_THREAD_SLOT` 分配槽位，多线程可能映射到同一槽位（取模）。
2. **C 版写锁路径先用 `atomic_add(WRITE)` 再 `smp_mb()` 检查 readers**。如果失败则 `atomic_sub_return` 回滚。Rust 版先用 CAS 设置 `WRITE_BIT`，再逐槽 drain。
3. **C 版在 write 回滚时可能返回需要触发 wakeup 的类型**（当设置了 WAITING 位时）。Rust 版直接返回 false。

---

## 7. 丢失的功能（完全不存在于 volmount）

| 功能 | bcachefs 函数 | 严重性 | 影响 |
|------|-------------|--------|------|
| 通用 waiter 接口（栈分配 waiter + 注入 should_sleep_fn） | `six_lock_ip_waiter` | **P1** | 无法实现 btree 锁的死锁周期检测 |
| 跳过 trylock 的 contended 路径 | `six_lock_contended` | **P1** | btree 路径在 trylock 失败后多一次 CAS |
| 通用锁转换 | `six_trylock_convert(from, to)` | **P2** | 缺少 read↔intent 的通用转换 |
| 锁持有计数查询 | `six_lock_counts` | **P2** | 无法获取 intent/write 持有数量 |
| 写锁自死锁避免 | `six_lock_readers_add` | **P2** | 无法在写锁获取前临时排除自身读者 |
| 唤醒所有等待者 | `six_lock_wakeup_all` | **P2** | 调试/异常恢复设施缺失 |
| 持有计数增加 | `six_lock_increment` | **P2** | 上层重入追踪需额外实现 |
| lockdep 集成 | `dep_map` + `lock_acquire/release` | **P3** | 无内核 lockdep 等价物 |
| owner 栈追踪（debug） | `owner_stack` (bch_stacktrace) | **P3** | 调试能力缺失 |
| ip 参数（调用点追踪） | `_THIS_IP_` 参数 | **P3** | 无锁获取调用点追踪 |

---

## 8. Rust 新增功能（C 端没有）

| 功能 | Rust 方法 | 说明 |
|------|----------|------|
| intent→write 升级 | `try_upgrade_intent_to_write()` | C 端在 btree 层实现，非 six 核心 API |
| write→intent 降级 | `downgrade_write_to_intent()` | C 端无对应函数 |
| 带自旋的升级 | `upgrade_read_to_intent()` / `upgrade_intent_to_write()` | C 端只有 try_ 版本，不自旋 |
| nospin 显式控制 | `set_nospin()` / `clear_nospin()` / `is_nospin()` | C 端 nospin 位由乐观自旋逻辑隐式控制 |
| 调试状态字符串 | `debug_state()` | C 端通常用 tracing/printk |
| `Default` trait | `SixLock::default()` | Rust 风格便利实现 |
| 线程持有检测 | `is_write_locked_by_current()` / `is_intent_locked_by_current()` | C 端通过 `lock->owner == current` 隐式检查 |

---

## 9. "基本对齐" 声明的验证

**设计文档的声明：**
> 对应 bcachefs fs/util/six.h + six.c。  
> "SIX" 不是 6 个状态，而是 6 个操作（lock/unlock × read/intent/write）。

**评估**：✅ **大致正确，但有重要偏离**

### 对齐良好的方面
1. 三种锁类型（Read/Intent/Write）的语义完全一致
2. 原子状态位布局完全一致（26 bit read count + 3 flag bits + 2 waiting bits + 1 nospin bit）
3. try_lock/unlock 对读/意图/写三者的 API 签名基本一致
4. try → spin → sleep 三级等待策略概念一致
5. Percpu reader 模式的概念和基本流程一致
6. Relock 模式（seq 验证重入）一致
7. 等待位写者饥饿保护一致

### 重要偏离
1. **Waitlist 接口大幅简化**：C 端的 `six_lock_waiter` + `should_sleep_fn(lock, waiter)` 接口是完整的死锁检测基础。Rust 端将其简化为无参闭包，无法承载 btree 级别的周期检测。
2. **缺少 `six_lock_contended`**：这个函数在 bcachefs btrees 的慢路径中被使用来避免不必要的第二次 trylock CAS。缺失会影响高争用下的性能。
3. **Percpu reader 是模拟的**：`Box<[AtomicU32]>` 而非真 percpu 变量，在密集并发下会有伪共享和 cacheline 颠簸。
4. **锁升级操作集不一致**：Rust 端增加了 intent→write 升级、write→intent 降级和带自旋的升级——这些是 C 端 six 层没有的，需要额外维护验证。
5. **seq 类型差异 (u32 vs u64)**：在长时间运行的系统中可能影响 relock 语义。

### "基本对齐" 适用于
- 对 lock/unlock API 的最基本使用场景
- 三种锁类型的互斥语义
- 非 percpu 模式的原子 CAS 路径
- 位域布局

### "基本对齐" 不适用于
- 死锁检测集成（Rust 端需要上层提供不同的集成模式）
- 高争用场景下的性能特性（模拟 percpu 有差异）
- btree 锁的 waiter 协议（Rust 端缺少关键基础设施）

---

## 10. 汇总表与优先级

| 区域 | 严重性 | 差异项 | 影响 |
|------|--------|--------|------|
| **P1 — 需要立即关注** | | | |
| Waiter 接口 | P1 | 无 `six_lock_waiter` 栈分配，should_sleep_fn 无参数 | 无法实现 btree 死锁周期检测 |
| Contended 路径 | P1 | 无 `six_lock_contended()` 跳过初始 trylock | 高争用时多一次 CAS |
| 死锁检测集成 | P1 | should_sleep_fn 无法访问锁/waiter | 解耦导致复杂度转移到上层 |
| **P2 — 中等** | | | |
| 通用锁转换 | P2 | 无 `trylock_convert(from, to)` | 缺少 read↔intent 通用设施 |
| 锁持有计数 | P2 | `reader_count()` 仅返回读者数 | 无 intent/write 计数 |
| 写锁自死锁 | P2 | 无 `six_lock_readers_add()` | 写锁获取前无法排除自身读锁 |
| seq 类型 | P2 | u64 vs u32 | 兼容性/溢出行为不同 |
| Percpu 模拟 | P2 | `Box<[AtomicU32]>` 而非真 percpu | 伪共享风险 |
| 唤醒所有 | P2 | 无 `six_lock_wakeup_all()` | 调试/恢复受限 |
| 持有计数增加 | P2 | 无 `six_lock_increment()` | 重入追踪需额外实现 |
| 等待者时间序 | P2 | Rust 不保证 FIFO 顺序 | bcachefs 保证最老事务优先 |
| **P3 — 低优先级** | | | |
| 返回类型 | P3 | `bool` vs `int` | C 端可返回 `-ENOMEM` 等错误 |
| 通用参数化 API | P3 | 无 `six_lock_type(lock, type)` | 需手动 dispatch |
| relock_write | P3 | 无写重锁 API | 较少使用 |
| owner 扩展追踪 | P3 | Rust 追踪 write_owner | C 端仅追踪 intent 所有 |
| lockdep | P3 | 无 | Rust 环境通常无等价物 |
| 内联小队列 | P3 | 无 inline_fifo (8 slot) | 小负载场景下性能差异 |
| 调试栈追踪 | P3 | 无 owner_stack | 调试能力受限 |

---

## 附录：btree 集成接口

bcachefs `locking.h` 在 six lock 之上增加了以下 btree 特定功能（volmount 当前无对应实现）：

| 接口 | 用途 |
|------|------|
| `bch2_six_check_for_deadlock(lock, waiter)` | should_sleep_fn 回调，触发 DFS 周期检测 |
| `btree_node_lock()` / `btree_node_lock_slowpath()` | btree 节点锁获取的快速/慢速路径 |
| `__btree_node_lock_write()` | intent→write 升级（先标记后获取） |
| `bch2_btree_node_relock()` | 事务重启后检查并重锁节点 |
| `bch2_btree_path_upgrade()` | 路径锁升级（增加 locks_want） |
| `lock_graph` | per-CPU DFS 栈，用于周期检测遍历 |

这些 btree 层接口全部依赖 `six_lock_ip_waiter` 和 `six_lock_contended` 的完整 waiter 协议——这是 Rust 版无法直接支持的 P1 级缺失。

---

*审计结束*
