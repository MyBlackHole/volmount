# Btree 并发模型优化 — 实施计划

总估计：~500 行新增/修改。每个子任务独立可验证。

---

## 前置依赖

```bash
# 安装 liburcu (系统级 C 库)
sudo apt install liburcu-dev
```

## 子任务结构

```
parent: btree-concurrency
├── step-1-d5-waitfifo-urcu        (lock/{wait_fifo,six}.rs)
├── step-2-d4-write-starvation      (lock/six.rs)
├── step-3-d6-lockgraph-per-thread  (lock/{deadlock,mod}.rs, btree/{transaction,mod}.rs)
├── step-4-d7-should-sleep-fn       (lock/six.rs, btree/transaction.rs)
├── step-5-d1-path-pooling          (btree/transaction.rs)
└── step-6-d3-relock-auto-recovery  (btree/transaction.rs)
```

依赖：D5 → D4, D5 → D6 → D7, D1 → D3

---

## Step 1: D5 — WaitFifo URCU 化

### 修改文件

| 文件 | 改动 |
|------|------|
| `crates/volmount-core/Cargo.toml` | 添加 `urcu = "0.0.4"` |
| `crates/volmount-core/src/lock/wait_fifo.rs` | WaitFifo 结构体改为 `Box<[RcuBox<Option<Box<WaiterBox>>>]>` |
| `crates/volmount-core/src/lock/six.rs` | 移除 `wait_lock: Mutex<()>`，`notify_waiters`/`remove_self_from_fifo` 改用 RCU RSCS |
| `crates/volmount-core/src/lock/mod.rs` | 公开新 API（如有需要） |

### WaitFifo 新定义

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

### 改动要点

1. **构造函数**：`Rcu::init()` + `RcuBox::empty()` 初始化每个 slot
2. **push_waiter**：`slot.update(Some(Box::new(waiter)))` → 返回旧值的 `RcuBox`
3. **notify_waiters**：移除 `wait_lock.lock()` + snapshot，改用：
   ```rust
   thread.rscs(|rscs| {
       for slot in &self.slots {
           if let Some(ref waiter) = *slot.read(rscs) {
               if waiter.lock_type == lock_type {
                   waiter.thread.as_ref().map(|t| t.unpark());
               }
           }
       }
   })
   ```
4. **remove_by_thread**：`compare_and_update` 替代 Mutex 保护
5. **线程注册**：通过 `thread_local! { OnceCell<(Rcu, RcuThread)> }` 管理

### 移除的字段

```rust
// six.rs 中移除：
wait_lock: Mutex<()>,  // 不再需要

// WaitFifo 中移除：
snapshot() 方法  // 不再需要
```

### 验证

```bash
cargo test -p volmount-core lock::wait_fifo::tests
cargo test -p volmount-core lock::six::tests
```

### 回滚

```bash
git checkout -- crates/volmount-core/Cargo.toml
git checkout -- crates/volmount-core/src/lock/
```

---

## Step 2: D4 — SixLock 写锁饥饿防止（WRITE_BIT preset）

### 修改文件

`crates/volmount-core/src/lock/six.rs`

### 改动

`lock_write` 慢路径入口预设 `WRITE_BIT`：

```rust
fn has_waiting_write(state: u32) -> bool {
    state & WRITE_BIT != 0
}
```

1. **`lock_write` sleep 路径**：
   ```rust
   // 在 push_waiter 前预设 WRITE_BIT
   self.state.fetch_or(WRITE_BIT, Ordering::Relaxed);
   self.wait_fifo.push_waiter(waiter);
   ```

2. **`try_lock_read`**（percpu 路径 + 原子路径）：
   ```rust
   // percpu: if !has_write_lock(state) && !has_waiting_write(state)
   // atom:   if has_write_lock(state) || has_waiting_write(state) → return false
   ```

3. **`unlock_write`**：释放时同时清除 WRITE_BIT
   ```rust
   self.state.fetch_and(!(WRITE_BIT | WRITE_LOCK_BIT), Ordering::Release);
   ```

4. **回滚路径**（死锁时）：
   ```rust
   self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
   self.wait_fifo.remove_self(trans_id);
   ```

### 验证

```bash
cargo test -p volmount-core lock::six::tests
```

### 回滚

```bash
git checkout -- crates/volmount-core/src/lock/six.rs
```

---

## Step 3: D6 — LockGraph per-thread 瞬态化

### 原则

完全替换 `LockGraph`。旧的 struct/hashmap 都删除，换成 bcachefs 风格的 DFS 栈。

### 修改文件

| 文件 | 改动 |
|------|------|
| `crates/volmount-core/src/lock/deadlock.rs` | 全量替换：LockGraph → DeadlockDetector |
| `crates/volmount-core/src/lock/mod.rs` | 移除旧 `LockGraph` 导出，新增 `DeadlockDetector` 导出 |
| `crates/volmount-core/src/btree/transaction.rs` | 移除 `lock_graph`/`lock_graphs` 字段，移除 `try_lock_all` 中的注册/检测 |
| `crates/volmount-core/src/btree/mod.rs` | 移除 `lock_graphs` 数组 |

### DeadlockDetector 实现

```rust
// lock/deadlock.rs — 新实现
pub struct DeadlockDetector {
    nodes: [DetectorNode; 8],
    nr: usize,
}

// thread_local 持有
thread_local! {
    pub(crate) static DEADLOCK_DETECTOR: std::cell::UnsafeCell<DeadlockDetector> =
        const { std::cell::UnsafeCell::new(DeadlockDetector::new()) };
}

impl DeadlockDetector {
    pub fn detect(
        &mut self,
        start_trans_id: u64,
        start_lock: u64,
        waitfifo: &WaitFifo,
    ) -> bool {
        // DFS 遍历 — 通过 WaitFifo 的 RCU 遍历收集等待链
        // WaitFifo.collect_waiters(lock) → Vec<(trans_id, waiting_lock)>
    }
}
```

### BtreeTransaction 移除

```rust
// 移除的字段：
lock_graph: Option<Arc<Mutex<LockGraph>>>,
lock_graphs: Option<Vec<Arc<Mutex<LockGraph>>>>,

// 移除的方法：
with_lock_graph(), set_lock_graph(), from_engine()

// try_lock_all 中移除：
// - register_lock()
// - add_waiting() / add_held()
// - detect()
// try_lock_all 只负责排序 + try_lock，不再参与死锁检测
```

### 现有测试适配

`test_with_lock_graph_creates_transaction` → 删除（概念不再存在）
`test_lock_graph_register_on_begin` → 删除
其余 deadlock 测试 → 重写为 DeadlockDetector 的 DFS 正确性测试

### 验证

```bash
cargo build -p volmount-core
cargo test -p volmount-core lock::deadlock::tests
```

### 回滚

```bash
git checkout -- crates/volmount-core/src/lock/deadlock.rs
git checkout -- crates/volmount-core/src/lock/mod.rs
git checkout -- crates/volmount-core/src/btree/transaction.rs
```

---

## Step 4: D7 — SixLock should_sleep_fn

### 修改文件

| 文件 | 改动 |
|------|------|
| `crates/volmount-core/src/lock/six.rs` | SixLock 新增 `should_sleep_fn: Option<Arc<dyn Fn(u64)->bool + Send + Sync>>`，setter，调用点 |
| `crates/volmount-core/src/btree/transaction.rs` | 在 BtreeTransaction 的 lock 路径传入 trans_id |
| `crates/volmount-core/src/btree/mod.rs` | BtreeEngine 初始化时设置回调 |

### SixLock 新增

```rust
pub struct SixLock {
    // ... 现有字段 ...
    should_sleep_fn: Option<Arc<dyn Fn(u64) -> bool + Send + Sync>>,
}

impl SixLock {
    pub fn set_should_sleep_fn<F: Fn(u64) -> bool + Send + Sync + 'static>(
        &mut self, f: F,
    ) {
        self.should_sleep_fn = Some(Arc::new(f));
    }

    fn should_sleep(&self, trans_id: u64) -> bool {
        match self.should_sleep_fn {
            Some(ref f) => f(trans_id),
            None => true,
        }
    }
}
```

### lock_write 慢路径集成

```rust
pub fn lock_write(&self, trans_id: u64) -> bool {
    if self.try_lock_write() { return true; }
    if self.spin_lock_write_internal() { return true; }

    self.state.fetch_or(WRITE_BIT, Ordering::Relaxed);
    self.wait_fifo.push_waiter(Box::new(WaiterBox::new(trans_id, SixLockType::Write)));

    loop {
        if !self.should_sleep(trans_id) {
            // 死锁检测触发，回滚
            self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
            self.wait_fifo.remove_self(trans_id);
            return false;
        }
        thread::park();
        if self.try_lock_write() {
            self.wait_fifo.remove_self(trans_id);
            self.state.fetch_and(!WRITE_BIT, Ordering::Relaxed);
            return true;
        }
    }
}
```

### BtreeEngine 初始化

```rust
impl BtreeEngine {
    fn init_lock(&self, lock: &SixLock, btree_type: BtreeType) {
        // 设置回调，用 Arc 共享状态
        let graph = self.deadlock_state.clone();
        lock.set_should_sleep_fn(move |trans_id| {
            DEADLOCK_DETECTOR.with(|cell| {
                let det = unsafe { &mut *cell.get() };
                let wait_fifo = graph.wait_fifo_for(btree_type);
                !det.detect(trans_id, 0, wait_fifo)
            })
        });
    }
}
```

### 验证

```bash
cargo build -p volmount-core
cargo test -p volmount-core
```

### 回滚

```bash
git checkout -- crates/volmount-core/src/lock/six.rs
git checkout -- crates/volmount-core/src/btree/
```

---

## Step 5: D1 — Path 共享缓存池

### 修改文件

`crates/volmount-core/src/btree/transaction.rs`

### 改动

```rust
pub fn get_iter(
    &mut self,
    root: &BtreeRoot,
    target: &BtreeKey,
    intent: bool,
    btree_type: BtreeType,
) -> &mut BtreeIter {
    let idx = self.get_path(root, target, intent, btree_type);
    &mut self.iters[idx]
}
```

`get_path()` 不变。39 个 `get_iter()` 调用点 0 修改。

### 验证

```bash
cargo test -p volmount-core btree::transaction::tests
```

---

## Step 6: D3 — Relock 自动恢复

### 修改文件

`crates/volmount-core/src/btree/transaction.rs`

### 新增方法

`restart_with_relock()` — 两阶段算法（同原设计）。

### 双解锁保护

在 unlock_all 前，将已解锁的 level 设为 `LockState::None`。

### 验证

```bash
cargo test -p volmount-core btree::transaction::tests
```

---

## 验证清单

```bash
# liburcu 系统依赖
sudo apt install liburcu-dev

# 编译验证
cargo check -p volmount-core

# 每步后
cargo test -p volmount-core
cargo clippy -p volmount-core -- -D warnings

# 整体回归
cargo test -p volmount-core
cargo clippy -p volmount-core -- -D warnings
```

## 回滚方案

```bash
# 全量回滚
git checkout -- crates/volmount-core/Cargo.toml
git checkout -- crates/volmount-core/src/lock/
git checkout -- crates/volmount-core/src/btree/
```
