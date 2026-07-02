# Btree 并发模型优化

## Goal

将 volmount btree 核心模块的并发模型完全对齐 bcachefs，消除在 Part A 调研中识别的所有 P0 级差距。

## Confirmed Facts

基于 4 份 bcachefs 对比调研文档，bcachefs 的无锁并发模型依赖三个核心基础设施：

1. **per-CPU 瞬态 LockGraph** — 不是持久化图，而是在 should_sleep_fn 中动态构建的 DFS 栈（8 帧深度），per-CPU 零竞争
2. **WaitFifo RCU 无锁遍历** — notify_waiters 和死锁检测都不需要 Mutex，通过 RCU 保护等待者生命周期
3. **should_sleep_fn 回调** — 死锁检测锁在 SixLock 内部，只在实际 schedule() 前触发，热路径零开销

volmount 当前实现在这三方面的差距：
- LockGraph 用 `Arc<Mutex<HashMap>>` 持久化，所有事务竞争同一把锁
- WaitFifo 用 `Mutex` 保护 snapshot，notify_waiters 和 remove 串行
- 死锁检测在 `try_lock_all` 中每次锁冲突都触发，不在 SixLock 内部

## Scope

### In Scope（完全对齐 bcachefs 的 4 项修复）

1. **LockGraph per-thread 瞬态化** — 移除持久化图，改用 thread_local DFS 栈
2. **WaitFifo RCU 化** — 引入 epoch-based reclamation，无锁遍历
3. **SixLock should_sleep_fn** — 死锁检测移入锁层，移除 try_lock_all 中的显式检测
4. **写锁饥饿防止** — SixLock 写锁 slowpath 预设 WRITE_BIT（完全对齐 bcachefs）

### In Scope（额外的基础设施）

5. **新增依赖** — `urcu` crate（liburcu Rust safe wrapper，LGPL-2.1+，已确认兼容）
6. **Path 共享** — get_iter 转发到 get_path（现有能力暴露）

### Out of Scope

- Journal 无锁 reservation（独立模块）
- Alloc freespace btree（独立模块）
- btree_path refcounting / 预分配 inline paths（path 层重构，下一步）
- NUMA 感知 slot 分配

## Acceptance Criteria

- [ ] cargo check + cargo test + cargo clippy 全部通过
- [ ] LockGraph 无 `Mutex`（改为 thread_local 瞬态栈）
- [ ] WaitFifo 无 `Mutex`（改为 urcu-guarded 无锁遍历）
- [ ] notify_waiters 无锁遍历 WaitFifo
- [ ] try_lock_read 在有 WRITE_BIT 时失败
- [ ] try_lock_all 移除显式 LockGraph 调用
- [ ] 新增并发测试验证写锁不饥饿
