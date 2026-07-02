# 实施: SixLock spurious wakeup + lock_slowpath WRITE_BIT

## 文件

`crates/volmount-core/src/lock/six.rs`

## 实施步骤

### Step 1: try_lock_read spurious wakeup

**位置**：`try_lock_read()` percpu 路径，行 295-300

**变更**：在 `readers[slot].fetch_sub(1, Ordering::Relaxed)` 后增加 fence + state 检查 + 条件唤醒。

```rust
// 替换：
readers[slot].fetch_sub(1, Ordering::Relaxed);
false

// 为：
readers[slot].fetch_sub(1, Ordering::Relaxed);
fence(Ordering::Acquire);
let after = self.read_state();
if after & WAITING_WRITE_BIT != 0 {
    self.wakeup_lock_type(after, SixLockType::Write);
}
false
```

### Step 2: 拆分 __wakeup_lock_type

**位置**：`wakeup_lock_type()` 函数，行 1519-1640

**变更**：将 RCU 遍历唤醒逻辑抽出为 `__wakeup_lock_type(&self, lock_type: SixLockType, rscs: &RcuReadSideCriticalSection)`。

1. 创建 `__wakeup_lock_type` 内部函数（取锁时不获取 wait_lock）
2. `wakeup_lock_type` 获取 wait_lock 后调用 `__wakeup_lock_type`
3. `lock_wakeup_all` 中的直读调用改为通过 `__wakeup_lock_type`（也持有 wait_lock）

### Step 3: 级联唤醒

**位置**：`__wakeup_lock_type(Read)` 路径中 `try_lock_read_for` 失败时

**变更**：当 `try_lock_read_for` 失败且 `WAITING_WRITE_BIT` 存在时，调用 `self.__wakeup_lock_type(SixLockType::Write, rscs)`。

### Step 4: lock_slowpath WRITE_BIT

**位置**：`lock_slowpath()` 写路径，行 1258-1267

**变更**：在 `fetch_or(WAITING_WRITE_BIT)` 前增加 `fetch_or(WRITE_BIT)`：

```rust
if type_ == SixLockType::Write {
    self.state.fetch_or(WRITE_BIT, Ordering::SeqCst);
    self.state.fetch_or(WAITING_WRITE_BIT, Ordering::SeqCst);
    fence(Ordering::SeqCst);
    // ... 双检 trylock ...
}
```

### Step 5: should_sleep 错误路径清除 WRITE_BIT

**位置**：`lock_slowpath()` should_sleep 回调错误路径（行 1299-1306）

**变更**：当 `should_sleep_fn` 返回错误且类型为 Write 时，清除 WRITE_BIT + 唤醒读者。

```rust
// 当前：
if let Some(ref sleep_fn) = should_sleep {
    let ret = sleep_fn(self, wait);
    if ret != 0 {
        self.remove_self_from_fifo();
        wait.lock_acquired = false;
        return ret;
    }
}

// 改为：
if let Some(ref sleep_fn) = should_sleep {
    let ret = sleep_fn(self, wait);
    if ret != 0 {
        // 对应 bcachefs: 先看 lock 是否已被获取（rare race）
        if wait.lock_acquired {
            // lock 已被 waker 替我们获取，需要释放
            self.unlock_ip(type_, 0);
        } else {
            self.remove_self_from_fifo();
            if type_ == SixLockType::Write {
                // 清除预设的 WRITE_BIT，唤醒读者
                self.state.fetch_and(!WRITE_BIT, Ordering::Release);
                let s = self.read_state();
                self.wakeup_lock_type(s, SixLockType::Read);
            }
        }
        wait.lock_acquired = false;
        return ret;
    }
}
```

## 验证

```bash
# 运行 SixLock 测试
cargo test -p volmount-core lock::six 2>&1

# 完整编译检查
cargo check -p volmount-core 2>&1
```

## 回滚点

- Step 1-5 的每步都独立可验证
- 若 Step 2-3 级联有问题，可单独回滚 `__wakeup_lock_type` 拆分，保留 Step 1+4+5
