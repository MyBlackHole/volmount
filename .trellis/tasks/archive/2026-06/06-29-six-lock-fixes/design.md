# 设计: SixLock spurious wakeup + lock_slowpath WRITE_BIT

## 1. Spurious wakeup

### 问题

`try_lock_read()` 在 percpu 模式下：
1. `readers[slot].fetch_add(1)` — 临时增加读者计数
2. 发现写锁持有或有写者在等待 → `fetch_sub(1)` 回滚

临时增加的读者计数可能让正在 drain 的写者看到非零计数而放弃获取锁、进入等待队列。但回滚后没有唤醒受影响的写者。

### 方案

在 `try_lock_read()` percpu 回滚路径结束时，检查 `WAITING_WRITE_BIT`。若设置，调用 `self.wakeup_lock_type(state, SixLockType::Write)`。

```rust
// 当前：
readers[slot].fetch_sub(1, Ordering::Relaxed);
false

// 改为：
readers[slot].fetch_sub(1, Ordering::Relaxed);
fence(Ordering::Acquire);
let new_state = self.read_state();
if new_state & WAITING_WRITE_BIT != 0 {
    self.wakeup_lock_type(new_state, SixLockType::Write);
}
false
```

### 安全性

- `wakeup_lock_type` 内部获取 `wait_lock` — 调用时未持有任何锁，安全
- 双检机制：`wakeup_lock_type` 内部会重新读取 state 确认 WAITING 位仍在
- 不会死锁：非递归调用

## 2. 级联唤醒 (Cascading)

### 问题

`wakeup_lock_type(Read)` 中，`try_lock_read_for` 失败时（写锁持有），可能有写等待者被阻塞。当前只是 `all_woke = false`，不唤醒写者。

### 方案

将 `wakeup_lock_type` 拆分为：
- **`wakeup_lock_type`**（对外）：获取 `wait_lock` 后调 `__wakeup_lock_type`
- **`__wakeup_lock_type`**（内部）：假设 `wait_lock` 已持有

在 Read 唤醒路径中，当 `try_lock_read_for` 失败时，若 `WAITING_WRITE_BIT` 存在，退出 read 循环后调用 `__wakeup_lock_type(Write)`（在同一 `wait_lock` 临界区内）。

```rust
fn __wakeup_lock_type(&self, lock_type: SixLockType) {
    // 1. 无等待者 → 返回
    // 2. 读路径：遍历所有 read waiter
    //    - try_lock_read_for 成功 → remove + flag + unpark
    //    - 失败 → cascade: 若 WAITING_WRITE_BIT 存在，调 __wakeup_lock_type(Write)
    // 3. Write/Intent 路径：找最老 waiter, trylock, 成功则 remove + flag + unpark
    // 4. 清理 WAITING bit
}

fn wakeup_lock_type(&self, state: u32, lock_type: SixLockType) {
    if state & waiting_bit == 0 { return; }
    let _lock = self.wait_lock.lock();
    if self.read_state() & waiting_bit == 0 { return; }
    with_rcu(|_rcu, thread| thread.rscs(|rscs| {
        self.__wakeup_lock_type(lock_type, rscs);
    }));
}
```

## 3. lock_slowpath WRITE_BIT

### 问题

`lock_write()` 正确预设了 WRITE_BIT（行 800），但 `lock_slowpath()`（被 `lock_ip_waiter` 和 `lock_contended` 使用）只设了 WAITING_WRITE_BIT，不设 WRITE_BIT。这导致 `try_lock_write_preset_for()` 中的 `debug_assert!(self.has_write_lock(...))` 可能触发。

### 方案

`lock_slowpath` 的写分支在设置 WAITING_WRITE_BIT 前增加 WRITE_BIT 预设：

```rust
if type_ == SixLockType::Write {
    self.state.fetch_or(WRITE_BIT, Ordering::SeqCst);       // 新增
    self.state.fetch_or(WAITING_WRITE_BIT, Ordering::SeqCst); // 原 WAITING 行
    fence(Ordering::SeqCst);
    if self.trylock_ip(type_, 0) {
        self.clear_waiting_bit(type_);
        wait.lock_acquired = true;
        return 0;
    }
}
```

与 `lock_write()` 行 800-802 对齐。

## 影响分析

| 变更 | 影响范围 | 风险 |
|------|---------|------|
| `try_lock_read` spurious wakeup | 单函数，新增调用 `wakeup_lock_type` | 低 — wakeup_lock_type 有双检保护 |
| `__wakeup_lock_type` 重构 | `wakeup_lock_type` 内部重构 | 低 — 仅拆分，不改变逻辑 |
| 级联唤醒 | `__wakeup_lock_type(Read)` 路径 | 中 — 新增唤醒路径，需要测试覆盖 |
| `lock_slowpath` WRITE_BIT | 写慢路径 | 低 — 与 `lock_write()` 对齐 |
