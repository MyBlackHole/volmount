# SixLock: 修复 spurious wakeup / lock_slowpath WRITE_BIT

## Goal

对齐 bcachefs 参考实现，修复 SixLock 的两个 bug：

1. **Spurious wakeup** — try_lock 失败时可能让等待者永久睡眠
2. **lock_slowpath 写路径** — 通用慢路径不预设 WRITE_BIT，导致 debug_assert 可能触发

> should_sleep_fn 挂载拆出到独立 task（06-29-sixlock-should-sleep）。

## 已确认事实（来自代码分析）

### 1. Spurious wakeup 缺失

bcachefs `__do_six_trylock()` 在 trylock 失败且存在等待者时，返回负数编码等待者类型：
- **读 percpu 路径**：写锁持有 + `SIX_LOCK_WAITING_write` 存在 → 返回 `-1 - SIX_LOCK_write`
- **写 percpu 路径**：读者存在 + `SIX_LOCK_WAITING_read` 存在 → 返回 `-1 - SIX_LOCK_read`

外层 `do_six_trylock()` 接收负数后调用 `__six_lock_wakeup(lock, -ret - 1)` 唤醒受影响的等待者。

**volmount 现状**：
- `try_lock_read()`（行 285-321）：percpu 失败后直接 `return false`，无等待者检测
- `try_lock_write()`（行 412-489）：drain 失败回滚后直接 `return false`，无等待者检测

**级联唤醒也缺失**：bcachefs `__six_lock_wakeup` 在 trylock 返回负数时 fallthrough 到其他类型唤醒（`lock_type = -ret - 1; goto again`）。volmount `wakeup_lock_type` 中 `try_lock_*_for` 失败时只标记 `all_woke=false`，不级联。

### 2. lock_slowpath 写路径 WRITE_BIT

volmount 有两个写锁慢路径：
- `lock_write()`（行 780-835）：**正确**预设 WRITE_BIT（行 800）+ WAITING_WRITE_BIT（行 802）
- `lock_slowpath()`（行 1247-1336）：**只**设 WAITING_WRITE_BIT（行 1258），**不设** WRITE_BIT

bcachefs `__six_lock_slowpath`（行 571-575）先设 `SIX_LOCK_HELD_write` 再设 WAITING bit。

影响：`lock_ip_waiter(Write)` / `lock_contended(Write)` 路径不会预设 WRITE_BIT，导致 `try_lock_write_preset_for()`（行 537）的 `debug_assert!(self.has_write_lock(...))` 可能断言失败。

## Requirements

1. **Spurious wakeup**：`try_lock_read()` 和 `try_lock_write()` 失败时，检测受影响的等待者并触发对应类型的 `wakeup_lock_type` 调用
2. **级联唤醒**：`wakeup_lock_type` 中的 `try_lock_*_for` 失败时，级联到其他类型的唤醒
3. **lock_slowpath WRITE_BIT**：`lock_slowpath()` 写路径在设置 WAITING_WRITE_BIT 前先预设 WRITE_BIT

## Acceptance Criteria

- [ ] `try_lock_read` percpu 回滚后检测 `WAITING_WRITE_BIT`，调用 `wakeup_lock_type(self, SixLockType::Write)`
- [ ] `try_lock_write` percpu 回滚后检测 `WAITING_READ_BIT`，调用 `wakeup_lock_type(self, SixLockType::Read)`
- [ ] `wakeup_lock_type` 中 `try_lock_*_for` 失败时唤醒其他类型（cascading）
- [ ] `lock_slowpath(Write)` 在设置 WAITING_WRITE_BIT 前预设 WRITE_BIT
- [ ] 现有测试全部通过

## Out of Scope

- should_sleep_fn / DeadlockDetector 集成（拆到独立 task）
- 不修改 `WaiterBox` / `WaitFifo` 数据结构
- 不修改 bcachefs 参考实现

## Confirmed Facts from Codebase

| 检查项 | 结果 |
|--------|------|
| `try_lock_read` spurious wakeup | ❌ 缺失 |
| `try_lock_write` spurious wakeup | ❌ 缺失 |
| `wakeup_lock_type` cascading | ❌ 缺失 |
| `lock_slowpath(Write)` WRITE_BIT | ❌ 缺失 |
| `lock_write()` WRITE_BIT | ✓ 正确预设 |
