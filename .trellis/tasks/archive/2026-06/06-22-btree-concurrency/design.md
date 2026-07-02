# Btree 并发模型优化 — 设计文档

完全对齐 bcachefs。引入 `urcu` crate（liburcu Rust safe wrapper）做 RCU 无锁遍历 +
内存安全回收。

---

## 总览：改动对比

| 原设计（4 个修复） | 新设计（7 个修复） | 变化 |
|---|---|---|
| D1: Path 共享缓存池 | D1: Path 共享缓存池 | 不变 |
| D2: LockGraph per-btree Mutex | **D6**: LockGraph per-thread 瞬态化 | 彻底替换，移除 Mutex |
| D3: Relock 自动恢复 | D3: Relock 自动恢复 | 不变 |
| D4: SixLock 写锁饥饿（has_waiting_write 检查） | **D4**: SixLock WRITE_BIT preset | 改方案，对齐 bcachefs |
| — | **D5**: WaitFifo URCU 化 | 新增，最底层 |
| — | **D7**: SixLock should_sleep_fn 回调 | 新增 |

**依赖链**：D5 → D6 → D7 → D4，D1 → D3

---

## DEP: urcu 依赖

```toml
# crates/volmount-core/Cargo.toml
urcu = "0.0.4"
```

系统需安装 liburcu：
```bash
# Ubuntu/Debian
apt install liburcu-dev
```

### urcu 各 variant 选择

| Variant | 机制 | 适用 |
|---------|------|------|
| `memb` (default) | `membarrier()` 系统调用 | ✓ 选此。Linux 4.3+，高效 |
| `mb` | memory barrier + signals | 跨平台 |
| `qsbr` | quiescent state | 需要应用端显式声明 quiescent point |
| `bp` (bulletproof) | 动态线程注册 | 线程频繁创建/销毁时使用 |

`memb` 最适合 btree worker 线程（常驻、Linux-only），性能最优。

---

## D5: WaitFifo URCU 化

### 现状

```rust
// six.rs — WaitFifo，notify_waiters 用 Mutex 保护
pub fn notify_waiters(&self, lock_type: SixLockType) {
    let snapshot = {
        let _guard = self.wait_lock.lock();  // ← Mutex
        self.wait_fifo.snapshot()
    };
    for entry in &snapshot { if entry.lock_type == lock_type { entry.unpark(); } }
}
```

`notify_waiters` 被 unlock_read/unlock_write/intent 调用（每个锁释放路径），高并发时 Mutex 竞争。
`remove_self_from_fifo` 也需要 wait_lock。

### 目标

WaitFifo 遍历和删除都无锁。使用 URCU 保护 WaiterBox 指针的生命周期。

### 设计

**WaitFifo 新定义**：

```rust
use urcu::{Rcu, RcuThread};
use urcu::boxed::RcuBox;

pub struct WaitFifo {
    slots: Box<[RcuBox<Option<Box<WaiterBox>>>]>,
    next_free: AtomicU16,
    size: u16,
    rcu: Rcu,
}
```

**RCU 线程注册**（BtreeEngine worker 线程创建时调用一次）：

```rust
// 通过 thread_local 存储，避免重复注册
thread_local! {
    static RCU_HANDLE: std::cell::OnceCell<(Rcu, RcuThread)> =
        std::cell::OnceCell::new();
}

fn ensure_rcu_thread() -> &'static (Rcu, RcuThread) {
    RCU_HANDLE.with(|cell| {
        cell.get_or_init(|| {
            let rcu = Rcu::init();
            let thread = RcuThread::register(&rcu);
            (rcu, thread)
        })
    })
}

// 或者更简单的：每个 RCU 操作时注册/注销（但 rcu.rscs() 需要 RcuThread）
// 对于 btree worker 线程，注册是常驻的，不会产生额外开销
```

**UcuBox 模式的 WaitFifo**：

每个 slot 是一个 `RcuBox<Option<Box<WaiterBox>>>`。`RcuBox` 的语义是：
- `update(new_val)` → 原子替换，返回旧值的 `RcuBox<T>` wrapper
- `read(rscs)` → 在 RCU 读侧临界区内返回 `RcuRef<T>`（借用了 rscs 的生命周期）
- 旧值 `RcuBox` 被 Drop 时，内部调用 `synchronize_rcu()` 确保所有读者完成后再 free

**插入（push_waiter）**：
```rust
fn push_waiter(&self, waiter: Box<WaiterBox>) -> Option<usize> {
    // CAS 分配 next_free slot（同现有逻辑）
    // 但替换为 RcuBox::update：
    let old = self.slots[idx].update(Some(waiter));
    // old 可能是 Some(old_waiter) 或 None
    // 当 old 变量被 drop 时：
    //   1. synchronize_rcu() — 等所有正在进行的 RCU 读者完成
    //   2. 如果里面有旧的 WaiterBox，free 它
    Some(idx)
}
```

**无锁遍历（read_waiters）**：
```rust
fn read_waiters<R>(&self, f: impl FnOnce(&[&WaiterBox]) -> R) -> R {
    let (_rcu, thread) = ensure_rcu_thread();
    thread.rscs(|rscs| {
        // 在 RCU 读侧临界区内，所有通过 read() 返回的引用都安全
        // 写者即使 CAS 清除了 slot，也不会立即 free 内存
        let mut waiters: Vec<&WaiterBox> = Vec::new();
        for slot in self.slots.iter() {
            let opt = slot.read(rscs);  // RcuRef<Option<Box<WaiterBox>>>
            if let Some(ref waiter) = *opt {
                waiters.push(waiter.as_ref());
            }
        }
        // rscs 保护范围 — 在此之后引用不再有效
        f(&waiters)
    })
}
```

**notify_waiters**：
```rust
fn notify_waiters(&self) {
    // 先检查 state bit — 没有等待者就跳过
    // ...（与现有相同）

    let (_rcu, thread) = ensure_rcu_thread();
    thread.rscs(|rscs| {
        for slot in self.slots.iter() {
            let opt = slot.read(rscs);
            if let Some(ref waiter) = *opt {
                // 检查 lock_type 并 unpark
                // 注意：unpark 本身不需要 RCU 保护
                waiter.thread.as_ref().map(|t| t.unpark());
            }
        }
        // rscs 结束 → 允许回收
    })
}
```

**删除（remove_by_thread）**：
```rust
fn remove_by_thread(&self, tid: ThreadId) {
    for slot in self.slots.iter() {
        let (_rcu, thread) = ensure_rcu_thread();
        thread.rscs(|rscs| {
            let current = slot.read(rscs);
            if let Some(ref waiter) = *current {
                if waiter.thread.as_ref().map(|t| t.id()) == Some(tid) {
                    // CAS 比较当前引用，替换为 None
                    // compare_and_update 确保在 slot 没被其他人改过时再更新
                    slot.compare_and_update(current, None, |_, _| {
                        ControlFlow::Continue(())
                    });
                    // 旧值会被 RcuBox 的 Drop 在 synchronize_rcu 后释放
                }
            }
        });
    }
}
```

### 移除的代码

- `wait_lock: Mutex<()>` — 不再需要
- `snapshot()` / `Vec<*mut WaiterBox>` 临时快照 — 不再需要
- `WaiterBox` 的 `next` 链表指针（如果现有）— 不再需要（per-slot FixedSize 不需要链表）

### 风险

- **RcuBox::Drop 语义**：`compare_and_update` 返回的旧值 `RcuBox` 被隐式丢弃。需验证 `RcuBox::Drop` 是延迟回收（`call_rcu`）而非同步等待（`synchronize_rcu`）。如果 `Drop` 内部同步等待，在 `rscs` 临界区内丢弃 `RcuBox` 会导致死锁。实现阶段需写一个快速验证测试。

### 验证

- `test_wait_fifo_push_pop_thread`：现有测试，URCU 化后等价行为
- `test_wait_fifo_message_passing`：现有测试
- 新增 `test_wait_fifo_rcu_traverse`：在并发 push/remove 中无锁遍历
- 新增 `test_rcu_box_drop_semantics`：验证 `RcuBox::Drop` 不阻塞（在 rscs 内安全）

---

## D6: LockGraph per-thread 瞬态化

### 现状

```rust
// deadlock.rs — 当前，持久化 HashMap 图
pub struct LockGraph {
    edges: HashMap<NodeId, Vec<(NodeId, LockEdgeType)>>,
    reverse: HashMap<NodeId, Vec<(NodeId, LockEdgeType)>>,
    nodes: HashMap<NodeId, LockNode>,
}
// BtreeTransaction 持有 Arc<Mutex<LockGraph>>，所有事务竞争
```

### 目标

完全移除 `Mutex<LockGraph>`。改为 bcachefs 风格：per-thread 瞬态 DFS 栈（`struct lock_graph { nodes[8]; nr; }`）。

### 设计

**新 DeadlockDetector**（bcachefs lock_graph 等价）：

```rust
/// 瞬态死锁检测 — bcachefs `struct lock_graph` 的 Rust 对应
///
/// 不在事务中持久化，每次检测时在栈上构建。
/// 通过 `thread_local!` 存储，不需要任何互斥。
pub struct DeadlockDetector {
    nodes: [DetectorNode; 8],  // 8 帧（对应 BTREE_MAX_DEPTH）
    nr: usize,
}

struct DetectorNode {
    trans_id: u64,
    lock_id: u64,
    lock_type: SixLockType,
    visited: bool,
}
```

**thread_local 持有**（等效 bcachefs `DEFINE_PER_CPU(struct lock_graph)`）：

```rust
thread_local! {
    static DEADLOCK_DETECTOR: std::cell::UnsafeCell<DeadlockDetector> =
        const { std::cell::UnsafeCell::new(DeadlockDetector::new()) };
}
```

**DFS 检测逻辑**（等效 `bch2_check_for_deadlock()`）：

```rust
impl DeadlockDetector {
    /// 从当前锁出发，构建等待图并检测环
    ///
    /// get_waiters(lock_id) → 该锁上正在等待的事务列表
    /// get_waiting_lock(trans_id) → 该事务正在等待的锁（如果正在等的话）
    pub fn detect(
        &mut self,
        start_trans_id: u64,
        start_lock_id: u64,
        start_lock_type: SixLockType,
        waitfifo: &WaitFifo,  // 通过 WaitFifo 获取等待信息
    ) -> bool {
        self.nr = 0;
        self.push(start_trans_id, start_lock_id, start_lock_type);

        while self.nr > 0 && self.nr < 8 {
            let node = &mut self.nodes[self.nr - 1];
            if node.visited {
                self.nr -= 1;
                continue;
            }
            node.visited = true;

            // 读取当前锁的 wait_fifo（RCU 无锁遍历）
            let waiters = waitfifo.collect_waiters_for_detector(node.lock_id);
            for (waiter_trans_id, waiting_lock_id) in &waiters {
                if *waiter_trans_id == start_trans_id {
                    return true;  // 发现环 → 死锁
                }
                // 检查等者是否已经在栈中
                if self.find_cycle(*waiter_trans_id) {
                    return true;
                }
                // 递归：等者在等哪个锁？
                if let Some(next_lock) = waiting_lock_id {
                    self.push(*waiter_trans_id, *next_lock, SixLockType::Read);
                }
            }
        }
        false  // 无死锁
    }
}
```

**检测触发入口** — 从 `try_lock_all` 移除，放在 `should_sleep_fn` 中：

```rust
fn should_sleep(&self, trans_id: u64, lock_id: u64, lock_type: SixLockType) -> bool {
    DEADLOCK_DETECTOR.with(|cell| {
        let detector = unsafe { &mut *cell.get() };
        !detector.detect(trans_id, lock_id, lock_type, &self.wait_fifo)
    })
}
```

### 与 bcachefs 对齐对照

| bcachefs | volmount 新设计 |
|----------|----------------|
| `DEFINE_PER_CPU(struct lock_graph)` | `thread_local! { UnsafeCell<DeadlockDetector> }` |
| `nodes[8]` 栈 | `[DetectorNode; 8]` |
| `preempt_disable` 保证独占 | `UnsafeCell` + thread_local 单线程访问 |
| `should_sleep_fn` 触发 | SixLock 回调触发（D7） |
| 通过 `waiter->trans` 指针追踪 | 通过 WaitFifo RCU 遍历收集（D5） |

### 移除的代码

- `LockGraph` 结构体（HashMap edges / nodes / reverse）
- `LockNode`, `LockEdgeType`, `DeadlockReport` 
- `BtreeTransaction::lock_graph` / `lock_graphs`
- `BtreeEngine::lock_graphs` 数组
- `with_lock_graph()` / `set_lock_graph()` / `from_engine()`
- `try_lock_all` 中的 register/add_waiting/add_held/detect 逻辑

---

## D7: SixLock should_sleep_fn 回调

### 现状

```rust
// six.rs — should_sleep 固定返回 true
fn should_sleep(&self) -> bool { true }
```

死锁检测在 `try_lock_all` 中显式调用，不在 SixLock 内部。

### 目标

SixLock 在 park 前调用死锁检测。如果检测到死锁环且当前事务是 victim，不 park 而返回 false。

### 设计

**SixLock 新增字段**：

```rust
use std::sync::Arc;

pub struct SixLock {
    // ... 现有字段 ...
    state: AtomicU32,
    wait_fifo: WaitFifo,  // 已存在，但改为 URCU 版本（D5）

    // 新增：死锁检测回调
    // Arc<dyn Fn> 而非 Box，因为 SixLock 需要 Sync
    should_sleep_fn: Option<Arc<dyn Fn(u64) -> bool + Send + Sync>>,
}

impl SixLock {
    pub fn set_should_sleep_fn<F: Fn(u64) -> bool + Send + Sync + 'static>(
        &mut self,
        f: F,
    ) {
        self.should_sleep_fn = Some(Arc::new(f));
    }

    // 内部调用
    fn should_sleep(&self, trans_id: u64) -> bool {
        match self.should_sleep_fn {
            Some(ref f) => f(trans_id),
            None => true,
        }
    }
}
```

**lock_write 慢路径**（WRITE_BIT preset + should_sleep）：

```rust
pub fn lock_write(&self, trans_id: u64) -> bool {
    if self.try_lock_write() { return true; }
    if self.spin_lock_write_internal() { return true; }

    // Phase 1: 预设 WRITE_BIT — 阻塞后续读者和 intent 者
    self.state.fetch_or(WRITE_BIT, Ordering::Relaxed);

    // Phase 2: 注册等待者到 RCU 保护的 WaitFifo
    self.wait_fifo.push_waiter(Box::new(WaiterBox::new(trans_id, SixLockType::Write)));

    loop {
        // Phase 3: 死锁检测 — 调用 should_sleep_fn
        if !self.should_sleep(trans_id) {
            // 检测到死锁，回滚预设
            self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
            self.wait_fifo.remove_self(trans_id);
            return false;  // 返回给调用者，触发事务重启
        }

        // Phase 4: park
        thread::park();

        if self.try_lock_write() {
            self.wait_fifo.remove_self(trans_id);
            // 清除 WRITE_BIT（本应在 unlock_write 中清除，但这里也清理一下）
            self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
            return true;
        }
    }
}
```

**lock_read / lock_intent 慢路径**：类似，只是不需要预设 WRITE_BIT。

### 三方模块集成

BtreeEngine 在创建 SixLock 时设置回调：

```rust
impl Btree {
    pub fn new(...) -> Self {
        let lock = SixLock::new();
        let graph = LockGraphHandle::new();
        let trans_id = graph.next_trans_id(); // &AtomicU64

        lock.set_should_sleep_fn(move |tid| {
            // 在 RCU 读侧临界区内执行 DFS
            // trans_id → 查找该锁对应的 detector
            // ...
            true // 暂时
        });
        // ...
    }
}
```

（具体集成在实现阶段细化。）

---

## D4: SixLock 写锁饥饿防止（WRITE_BIT preset）

### 现状

```rust
fn try_lock_read(&self) -> bool {
    // 只检查 has_write_lock，不查 WAITING_WRITE_BIT
    if !self.has_write_lock(state) { return true; }
}
```

高并发读场景，写锁等待时读者持续进入，写者永远拿不到锁。

### 目标

完全对齐 bcachefs：写锁进入 sleep 路径时预设 `WRITE_BIT`，阻塞后续读者。

### 设计

**WRITE_BIT 预设时机**（已在 D7 lock_write 中体现，此处强调设计要点）：

```rust
// try_lock_read (percpu 路径)
fn try_lock_read(&self) -> bool {
    // 写锁持有 或 有写者在等待（预设了 WRITE_BIT）
    if has_write_lock(state) || has_waiting_write(state) {
        return false;
    }
    // ...
}
```

**关键：先预设 WRITE_BIT 再 push_waiter**。确保写锁在 sleep 前就发出信号：

```rust
// lock_write 顺序（来自 D7）：
self.state.fetch_or(WRITE_BIT, Ordering::Relaxed);  // 1. 先预设
self.wait_fifo.push_waiter(writer);                   // 2. 再入队
// ... park ...
```

如果反过来（先入队再预设），读者可能在中间窗口进入。

**安全回滚**（检测到死锁时）：
```rust
self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
self.wait_fifo.remove_self(trans_id);
```

**不影响 unlock_write** — 释放写锁时会清除 WRITE_BIT（写锁 = bit 0，WRITE_BIT = bit 7，清除写锁不会自动清除 WRITE_BIT，所以 unlock_write 需要显式清除）。

实际上，看 bcachefs 的实现，`unlock_write` 的流程：
1. 写锁释放（clear bit 0）
2. 检查 WRITE_BIT 是否需要清除
3. notify_waiters

在 volmount 中，`unlock_write` 需要改为：
```rust
pub fn unlock_write(&self) {
    // 清除 write_lock bit 和 WRITE_BIT
    self.state.fetch_and(!(WRITE_BIT | WRITE_LOCK_BIT), Ordering::Release);
    self.wait_fifo.notify_waiters();  // URCU 无锁遍历
}
```

### 验证

- `test_write_starvation_prevention`：2 读者 + 1 写者并发，写者应在 <100ms 内获取锁
- `test_read_not_blocked_no_writer`：无写者时读者吞吐不受影响
- `test_write_bit_rollback_on_deadlock`：死锁检测时 WRITE_BIT 正确回滚

---

## D1: Path 共享缓存池

### 现状

（同原设计）

### 设计

`get_iter()` 重写为 `get_path()` 的直接转发：

```rust
pub fn get_iter(&mut self, root: &BtreeRoot, target: &BtreeKey,
                 intent: bool, btree_type: BtreeType) -> &mut BtreeIter {
    let idx = self.get_path(root, target, intent, btree_type);
    &mut self.iters[idx]
}
```

不变。39 个 `get_iter()` 调用点零修改。

---

## D3: Relock 自动恢复

### 现状

（同原设计）

### 设计

新增 `restart_with_relock()` 两阶段算法：

```
Phase 1 — 检查 + 尝试 relock：
  遍历所有 level，seq 变化时尝试 relock
Phase 2 — 全部成功则返回 None（保持锁），否则全量重启
```

不变。

---

## 模块依赖关系 & 实现顺序

```
D5 (WaitFifo URCU)
    ↓ (WaitFifo 可无锁遍历后)
D6 (LockGraph per-thread 瞬态化)
    ↓ (锁可调用死锁检测)
D7 (SixLock should_sleep_fn)
 ↓                         ↓
D4 (WRITE_BIT preset)    D1 (Path 共享)
                              ↓
                           D3 (Relock)
```

**推荐实现顺序**：D5 → D4（D5 完成后 D4 才安全）→ D6 → D7 → D1 → D3

实际可根据依赖解耦：D4 只需要 WaitFifo 的 URCU 能力（D5 已提供），而 D6/D7 是把 LockGraph 从 try_lock_all 移到 SixLock 内部，两者可以部分并行。

## 测试策略

- **D5**：并发 push/remove/traverse + URCU 安全回收
- **D6**：DeadlockDetector DFS 正确性（有环/无环）
- **D7**：should_sleep_fn 正确阻断 park
- **D4**：写锁在多读者线程下不饥饿
- **回归**：`cargo test -p volmount-core` 全量通过
- **集成**：并发 btree 读写（多个 BtreeTransaction 同时 commit）
