//! SixLock — 3 状态读写锁（atomic bitfield + percpu reader）
//!
//! 对应 bcachefs fs/util/six.h + six.c。
//! "SIX" 不是 6 个状态，而是 6 个操作（lock/unlock × read/intent/write）。
//! 实际只有 3 种锁类型：
//!
//! - **Read**: 多个可共享，阻塞写锁
//! - **Intent**: 意向锁，意向之间互斥，但不阻塞读
//! - **Write**: 完全独占
//!
//! ## state 位布局 (AtomicU32)
//!
//! ```text
//! bit [0:25]  read_lock_count   (26 bits) — 当前持有读锁的线程数
//! bit [26]    intent_lock       (1 bit)   — 是否有意向锁被持有
//! bit [27]    write_lock        (1 bit)   — 是否有写锁被持有
//! bit [28]    waiting_read      (1 bit)   — 有线程在等待读锁
//! bit [29]    waiting_intent    (1 bit)   — 有线程在等待意向锁
//! bit [30]    waiting_write     (1 bit)   — 有线程在等待写锁
//! bit [31]    nospin            (1 bit)   — 禁止自旋，直接睡眠
//! ```
//!
//! ## Percpu Reader 模式
//!
//! 读者计数不通过原子操作更新 state，而是通过 percpu 变量：
//! 1. percpu_reader++（无锁）
//! 2. 全屏障 (Acquire fence)
//! 3. 检查 state.write_lock：
//!    - 无写锁 → 成功返回
//!    - 有写锁 → 回滚 percpu_reader，走慢路径（atomic CAS）

use std::cell::{OnceCell, UnsafeCell};
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use spin::Mutex;

use urcu::{Rcu, RcuRSCS, RcuThread};

use super::wait_fifo::{WaitFifo, WaiterBox};

// 当前线程的读锁计数（用于 try_lock_write 排除自身读锁）
// 仅在非 percpu 路径（readers.is_none()）时使用。
// try_lock_read 成功时 +1，unlock_read 时 -1。
// try_lock_write 校验时从总 reader count 中减去此值，
// 使得持有读锁的线程可以升级到写锁。
thread_local! {
    static THREAD_READ_CNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// RCU 句柄（每个线程首次访问时延迟初始化）
// 每个线程只需注册一次。`with_rcu` 提供安全的闭包式访问。
thread_local! {
    static RCU_HANDLE: OnceCell<(Rcu, RcuThread)> = const { OnceCell::new() };
}

// 在当前线程的 RCU read-side critical section 内执行闭包
// 自动初始化 RCU 库并注册当前线程（仅一次）。
pub(crate) fn with_rcu<F, T>(f: F) -> T
where
    F: FnOnce(&Rcu, &RcuThread) -> T,
{
    RCU_HANDLE.with(|cell| {
        let (rcu, thread) = cell.get_or_init(|| {
            let rcu = Rcu::init();
            let thread = RcuThread::register(&rcu);
            (rcu, thread)
        });
        f(rcu, thread)
    })
}

// ─── 位域常量 ───────────────────────────────────────────────

const READ_COUNT_MASK: u32 = 0x03FF_FFFF; // bits 0-25 (26 bits)
const INTENT_BIT: u32 = 0x0400_0000; // bit 26
const WRITE_BIT: u32 = 0x0800_0000; // bit 27
const WAITING_READ_BIT: u32 = 0x1000_0000; // bit 28
const WAITING_INTENT_BIT: u32 = 0x2000_0000; // bit 29; 新增，对应 bcachefs SIX_LOCK_WAITING_intent
const WAITING_WRITE_BIT: u32 = 0x4000_0000; // bit 30; 对应 C SIX_LOCK_WAITING_write = 1U << (28 + SIX_LOCK_write) 其中 SIX_LOCK_write=2
const NOSPIN_BIT: u32 = 0x8000_0000; // bit 31

/// 自旋重试次数（对应 bcachefs six_lock_spin() 的 ~1024 PAUSE 循环）
const SPIN_COUNT: u32 = 1024;

/// 锁类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SixLockType {
    Read = 0,
    Intent = 1,
    Write = 2,
}

/// 锁获取结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SixLockResult {
    /// 成功获取锁
    Acquired,
    /// 获取失败（trylock 路径）
    Busy,
    /// 死锁检测触发（需要事务重启）
    Deadlock,
}

/// 锁冲突矩阵 — 对应 bcachefs lock_type_conflicts()
///
/// ```text
/// read(0) + read(0) = 0    → 不冲突
/// read(0) + intent(1) = 1  → 不冲突
/// intent(1) + intent(1) = 2 → 冲突
/// intent(1) + write(2) = 3 → 冲突
/// write(2) + write(2) = 4  → 冲突
/// ```
pub fn lock_conflicts(held: SixLockType, want: SixLockType) -> bool {
    (held as u8 + want as u8) > 1
}

// ─── SixLock 相关类型（对应 bcachefs six.h） ─────────────────

/// 等待者条目 —— 对应 bcachefs struct six_lock_waiter
///
/// 嵌入上层的事务/锁跟踪结构体中，作为 lock waitlist 入口。
/// `trans_start_time` 用于 waitlist 排序（最早的事务先获取锁）。
#[derive(Debug)]
#[repr(C)]
pub struct SixLockWaiter {
    /// 事务开始时间戳（用于 waitlist 排序和死锁检测游标）
    pub trans_start_time: u64,
    /// 等待线程句柄
    pub thread: Option<thread::Thread>,
    /// 期望获取的锁类型
    pub lock_want: SixLockType,
    /// 锁是否已获取 —— 由唤醒方通过 Release 屏障设置
    pub lock_acquired: bool,
    /// 在 wait_fifo 中的槽位索引（用于 O(1) 自移除）
    pub slot_idx: u16,
}

/// 锁持有计数 —— 对应 bcachefs struct six_lock_count
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct SixLockCount {
    /// n[0]=read, n[1]=intent, n[2]=write 的持有计数
    pub n: [u32; 3],
}

/// `should_sleep_fn` 回调签名
///
/// 对应 bcachefs:
/// ```c
/// typedef int (*six_lock_should_sleep_fn)(struct six_lock *, struct six_lock_waiter *);
/// ```
///
/// 参数: `&SixLock`, `&SixLockWaiter`
/// 返回: 0 = 允许 sleep 继续等待；非 0 = 错误码，中止加锁并返回该值
pub type SixLockShouldSleepFn = dyn Fn(&SixLock, &SixLockWaiter) -> i32 + Send + Sync;

// ─── 线程 ID 分配（用于 percpu reader slot） ────────────────

static NEXT_THREAD_SLOT: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static THREAD_SLOT: u32 = NEXT_THREAD_SLOT.fetch_add(1, Ordering::Relaxed);
}

fn current_thread_slot() -> u32 {
    THREAD_SLOT.with(|&s| s)
}

// ─── SixLock ────────────────────────────────────────────────

/// SixLock 主结构
///
/// 三种锁状态编码在单个 AtomicU32 位域中。
/// 可选 percpu reader 模式：读者用独立槽位计数，避免原子操作。
pub struct SixLock {
    state: AtomicU32,
    seq: AtomicU64,
    // Percpu reader 模式
    // 当启用时，读者通过 readers[slot] 计数，不操作 state.read_count
    readers: Option<Box<[AtomicU32]>>,
    // 持有 intent 锁的线程 id（用于重入检测）
    intent_owner: UnsafeCell<Option<thread::ThreadId>>,
    intent_recurse: UnsafeCell<u32>,
    // 持有写锁的线程 id
    write_owner: UnsafeCell<Option<thread::ThreadId>>,
    write_recurse: UnsafeCell<u32>,
    // 等待队列（Phase C1: spin/sleep 路径使用，push 等待者 + 设置 waiting bit）
    // 使用 RcuBox 而非 Mutex 提供遍历保护
    wait_fifo: WaitFifo,
    // 等待队列自旋锁 — 对应 bcachefs raw_spinlock_t wait_lock
    // push_waiter / wakeup_lock_type / remove_self_from_fifo 三者共用，
    // 确保 FIFO push/remove 与 WAITING bit 管理的原子性。
    wait_lock: Mutex<()>,
}

// SixLock 的 Send/Sync: AtomicU32 是 Send+Sync，UnsafeCell 是 !Sync 但被保护
unsafe impl Send for SixLock {}
unsafe impl Sync for SixLock {}

impl SixLock {
    /// 创建新的 SixLock（标准模式）
    pub fn new() -> Self {
        let rcu = Rcu::init();
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            readers: None,
            intent_owner: UnsafeCell::new(None),
            intent_recurse: UnsafeCell::new(0),
            write_owner: UnsafeCell::new(None),
            write_recurse: UnsafeCell::new(0),
            wait_fifo: WaitFifo::new(16, &rcu),
            wait_lock: Mutex::new(()),
        }
    }

    /// 创建支持 percpu reader 的 SixLock
    ///
    /// `num_slots` = 预估的最大并发读者数（建议 >= CPU 核数 * 2）
    pub fn with_percpu(num_slots: u32) -> Self {
        assert!(num_slots > 0, "num_slots must be > 0");
        let readers: Vec<AtomicU32> = (0..num_slots).map(|_| AtomicU32::new(0)).collect();
        let rcu = Rcu::init();
        Self {
            state: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            readers: Some(readers.into_boxed_slice()),
            intent_owner: UnsafeCell::new(None),
            intent_recurse: UnsafeCell::new(0),
            write_owner: UnsafeCell::new(None),
            write_recurse: UnsafeCell::new(0),
            wait_fifo: WaitFifo::new(16, &rcu),
            wait_lock: Mutex::new(()),
        }
    }

    /// 当前序列号（每次写锁释放时递增，用于 relock 验证）
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    // ── 内部操作 ──

    /// 读取当前 state
    fn read_state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    /// 判断是否有写锁被持有
    fn has_write_lock(&self, state: u32) -> bool {
        state & WRITE_BIT != 0
    }

    /// 判断是否有 intent 锁被持有
    fn has_intent_lock(&self, state: u32) -> bool {
        state & INTENT_BIT != 0
    }

    /// 读取锁持有者计数
    fn read_count(&self, state: u32) -> u32 {
        state & READ_COUNT_MASK
    }

    // ── 读锁 ──

    /// 尝试获取读锁（快速路径）
    ///
    /// Percpu 模式：
    ///   1. percpu_reader[slot]++
    ///   2. Acquire fence
    ///   3. 检查 write_lock bit
    ///
    /// 标准模式：
    ///   1. CAS state.read_count + 1
    ///   2. 检查 write_lock bit（CAS 时会失败如果有写锁）
    ///
    /// 对应 bcachefs __do_six_trylock(Read) six.c:122-214, read 分支 six.c:159-185。
    /// 扩展: WAITING_WRITE_BIT 检查（D4 防止写锁饥饿）为 volmount 新增。
    pub fn try_lock_read(&self) -> bool {
        if let Some(ref readers) = self.readers {
            // Percpu 快速路径
            let slot = current_thread_slot() as usize % readers.len();
            readers[slot].fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
            let state = self.read_state();
            // D4: 检查是否有写锁或写者在等待（防止写锁饥饿）
            // bcachefs 在写者进入 sleep 前预设 WAITING_WRITE_BIT，
            // 新读者应避免在此窗口进入
            if !self.has_write_lock(state) && (state & WAITING_WRITE_BIT) == 0 {
                return true;
            }
            // 有写锁或写者在等待，回滚
            readers[slot].fetch_sub(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
            // S1: spurious wakeup — 临时增加的 percpu 读者计数可能让写者 drain 失败，
            // 回滚后需要唤醒已入队的写者
            let after = self.read_state();
            if after & WAITING_WRITE_BIT != 0 {
                self.wakeup_lock_type(after, SixLockType::Write);
            }
            false
        } else {
            // 标准原子路径
            loop {
                let state = self.read_state();
                // D4: 写锁持有 或 有写者在等待时拒绝新读者
                if self.has_write_lock(state) || (state & WAITING_WRITE_BIT) != 0 {
                    return false;
                }
                if self
                    .state
                    .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                    return true;
                }
                // CAS 失败，重试
                std::hint::spin_loop();
            }
        }
    }

    /// 释放读锁
    ///
    /// 对应 bcachefs do_six_unlock_type(Read) six.c:771-795（read 分支 six.c:778-783）。
    pub fn unlock_read(&self) {
        if let Some(ref readers) = self.readers {
            let slot = current_thread_slot() as usize % readers.len();
            readers[slot].fetch_sub(1, Ordering::Release);
        } else {
            self.state.fetch_sub(1, Ordering::Release);
            THREAD_READ_CNT.with(|c| c.set(c.get() - 1));
        }
        // 读锁释放 → 可能可以唤醒 write waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Write);
    }

    // ── Intent 锁 ──

    /// 尝试获取 intent 锁（intent 之间互斥，但不阻塞读）
    ///
    /// 使用 CAS 循环（对应 C __do_six_trylock() 的 atomic_try_cmpxchg_acquire 循环）：
    /// 1. 读 state
    /// 2. 检查 INTENT_BIT/WRITE_BIT 冲突
    /// 3. CAS 尝试设置 INTENT_BIT
    /// 4. 如果 CAS 失败（并发 state 变化），回退到步骤 1 重试
    ///
    /// 注意：不使用 fetch_or + 回滚模式，因为回滚可能错误清除其他线程已设置的 INTENT_BIT。
    ///
    /// 对应 bcachefs __do_six_trylock(Intent) six.c:122-214, intent 分支 six.c:159-169。
    pub fn try_lock_intent(&self) -> bool {
        // 先检查重入：当前线程已持有 intent 锁（通过 intent_owner 判断）
        let owner = unsafe { *self.intent_owner.get() };
        if owner == Some(thread::current().id()) {
            unsafe {
                *self.intent_recurse.get() += 1;
            }
            return true;
        }

        let mut state = self.read_state();
        loop {
            // 冲突检查：intent 或 write 已持有则失败
            if state & (INTENT_BIT | WRITE_BIT) != 0 {
                return false;
            }
            // CAS 原子设置 INTENT_BIT，仅当 state 未变化时成功
            match self.state.compare_exchange_weak(
                state,
                state | INTENT_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    unsafe {
                        *self.intent_owner.get() = Some(thread::current().id());
                    }
                    return true;
                }
                Err(current) => {
                    state = current;
                    // 重试循环：state 被其他线程修改（如 read_count 变化），
                    // 重新检查冲突并重试 CAS
                }
            }
        }
    }

    /// 释放 intent 锁
    ///
    /// 对应 bcachefs do_six_unlock_type(Intent) six.c:771-795（intent 分支 six.c:775-776, 783-791）。
    /// 重入处理对应 six_unlock_ip six.c:823-827。
    pub fn unlock_intent(&self) {
        let recurse = unsafe { *self.intent_recurse.get() };
        if recurse > 0 {
            unsafe {
                *self.intent_recurse.get() = recurse - 1;
            }
            return;
        }
        unsafe {
            *self.intent_owner.get() = None;
        }
        self.state.fetch_and(!INTENT_BIT, Ordering::Release);
        // Intent 释放 → 可能可以唤醒 intent waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Intent);
    }

    // ── 写锁 ──

    /// 尝试获取写锁（独占，必须 read_count == 0）
    ///
    /// Percpu 模式对齐 bcachefs：
    /// 1. CAS 前检查其他 slot 的读者（排除自身 slot）
    /// 2. CAS 成功后只 drain 其他 slot，跳过自身 slot
    ///    自身 percpu 读者是该线程持有的读锁，不阻塞写锁升级。
    ///
    /// 对应 bcachefs __do_six_trylock(Write) six.c:122-214, write 分支 six.c:186-205。
    /// 差异: bcachefs 用 atomic_add 预设 WRITE_BIT 再 pcpu_read_count，
    ///       Rust 先查 readers 再 CAS（drain 循环兜底，语义等价）。
    /// 差异: bcachefs write lock_fail=HELD_read 不检查 INTENT_BIT，
    ///       Rust 此函数检查 INTENT_BIT（intent→write 升级请用 try_upgrade_intent_to_write）。
    pub fn try_lock_write(&self) -> bool {
        let state = self.read_state();
        if self.has_write_lock(state) {
            // 重入检测
            let owner = unsafe { *self.write_owner.get() };
            if owner == Some(thread::current().id()) {
                unsafe {
                    *self.write_recurse.get() += 1;
                }
                return true;
            }
            return false;
        }
        // intent/write 锁冲突检查
        if (state & (INTENT_BIT | WRITE_BIT)) != 0 {
            return false;
        }

        if let Some(ref readers) = self.readers {
            // Percpu 模式：检查其他 slot 是否有读者
            let my_slot = current_thread_slot() as usize % readers.len();
            let has_other_readers = readers
                .iter()
                .enumerate()
                .any(|(i, r)| i != my_slot && r.load(Ordering::Relaxed) > 0);
            if has_other_readers {
                return false;
            }
            // CAS 设置 WRITE_BIT
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | WRITE_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                // drain 其他 slot，跳过自身 slot（自身读者不阻塞同一线程升级）
                for (i, reader) in readers.iter().enumerate() {
                    if i == my_slot {
                        continue;
                    }
                    while reader.load(Ordering::Acquire) > 0 {
                        std::hint::spin_loop();
                    }
                }
                unsafe {
                    *self.write_owner.get() = Some(thread::current().id());
                }
            }
            ok
        } else {
            // 非 percpu 模式：用 THREAD_READ_CNT 排除自身读锁
            let my_reads = THREAD_READ_CNT.with(|c| c.get());
            let total_reads = state & READ_COUNT_MASK;
            let other_reads = total_reads.saturating_sub(my_reads);
            if other_reads != 0 {
                return false;
            }
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | WRITE_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                unsafe {
                    *self.write_owner.get() = Some(thread::current().id());
                }
            }
            ok
        }
    }

    /// 在 WRITE_BIT 已预设的慢路径中使用的 trylock。
    ///
    /// 对应 bcachefs `__do_six_trylock(..., try=false)` 在 write 锁上的行为：
    /// 不检查 WRITE_BIT（因为 slowpath 已预设），只检查读者计数是否为零。
    /// WRITE_BIT 已预设意味着 writer 已在等待队列中，此时只需确认读者已全部退出。
    fn try_lock_write_preset(&self) -> bool {
        debug_assert!(self.has_write_lock(self.read_state()));

        if let Some(ref readers) = self.readers {
            let my_slot = current_thread_slot() as usize % readers.len();
            // 检查其他 percpu slot 是否有读者
            let has_other_readers = readers
                .iter()
                .enumerate()
                .any(|(i, r)| i != my_slot && r.load(Ordering::Relaxed) > 0);
            if has_other_readers {
                return false;
            }
            // drain 其他 slot，跳过自身 slot
            for (i, reader) in readers.iter().enumerate() {
                if i == my_slot {
                    continue;
                }
                while reader.load(Ordering::Acquire) > 0 {
                    std::hint::spin_loop();
                }
            }
        } else {
            let my_reads = THREAD_READ_CNT.with(|c| c.get());
            let state = self.read_state();
            let total_reads = state & READ_COUNT_MASK;
            if total_reads.saturating_sub(my_reads) != 0 {
                return false;
            }
        }
        // 无读者冲突，设置 owner 完成获取
        unsafe {
            *self.write_owner.get() = Some(thread::current().id());
        }
        true
    }

    /// Waker 替 write waiter 声明 write lock
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_write, waiter->task, false) six.c:163-165, 186-205。
    /// WRITE_BIT 已被慢路径预设。只检查读者计数是否为零，设 write_owner 为 waiter。
    /// C 中 !try 路径跳过 atomic_add（WRITE_BIT 已预设），smp_mb 后检查 pcpu_read_count。
    fn try_lock_write_preset_for(&self, tid: thread::ThreadId) -> bool {
        debug_assert!(self.has_write_lock(self.read_state()));

        let state = self.read_state();
        if self.read_count(state) > 0 {
            return false;
        }

        // percpu reader check
        if let Some(ref readers) = self.readers {
            for r in readers.iter() {
                if r.load(Ordering::Relaxed) > 0 {
                    return false;
                }
            }
        }

        // 设 owner 为 WAITER，不是 waker
        unsafe {
            *self.write_owner.get() = Some(tid);
        }
        true
    }

    /// Waker 替 intent waiter 声明 intent lock
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_intent, waiter->task, false)。
    /// 通过 CAS 设置 INTENT_BIT，设 owner 为 waiter 的 tid。
    fn try_lock_intent_for(&self, tid: thread::ThreadId) -> bool {
        let mut state = self.read_state();
        loop {
            if state & INTENT_BIT != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | INTENT_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    unsafe {
                        *self.intent_owner.get() = Some(tid);
                    }
                    return true;
                }
                Err(current) => state = current,
            }
        }
    }

    /// Waker 替 read waiter 声明 read lock（按 waiter 的 percpu slot）
    ///
    /// 对应 bcachefs __do_six_trylock(lock, SIX_LOCK_read, waiter->task, false)。
    /// 非 percpu：state.fetch_add(1)；percpu：readers[waiter_slot].fetch_add(1)。
    /// 不检查 write/intent 状态——wakeup_lock_type 的快速检查已确保写锁不会在此
    /// 期间被获取（或 reader 不会处于 WAITING 状态）。
    fn try_lock_read_for(&self, slot_idx: u32) -> bool {
        if let Some(ref readers) = self.readers {
            let idx = slot_idx as usize % readers.len();
            readers[idx].fetch_add(1, Ordering::Relaxed);
            fence(Ordering::Acquire);
        } else {
            self.state.fetch_add(1, Ordering::Acquire);
        }
        true
    }

    // ── 自旋方法 ──

    /// 内部自旋等待读锁可用（检查 nospin bit，循环 SPIN_COUNT 次）
    ///
    /// 先检查 nospin bit：如果置位则立即返回 false。
    /// 然后循环调用 try_lock_read()，每次迭代间用 spin_loop() 减少 busy-wait 开销。
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_read_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_lock_read() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.try_lock_read() {
            return true;
        }
        false
    }

    /// 内部自旋等待 intent 锁可用
    ///
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_intent_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_lock_intent() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.try_lock_intent() {
            return true;
        }
        false
    }

    /// 内部自旋等待写锁可用（要求 read_count == 0, intent == 0, write == 0）
    ///
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    fn spin_lock_write_internal(&self) -> bool {
        if self.read_state() & NOSPIN_BIT != 0 {
            return false;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_lock_write() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU，让 OS 调度其他线程
        std::thread::yield_now();
        if self.try_lock_write() {
            return true;
        }
        false
    }

    // ── 阻塞/等待锁方法（try → spin → sleep） ──

    /// 获取读锁（try → spin → sleep 分级等待）
    ///
    /// 1. try_lock_read 快速路径
    /// 2. spin_lock_read_internal 自旋 SPIN_COUNT 次
    /// 3. sleep：入队 WaitFifo + thread::park() 阻塞
    ///    wake 后尝试获取锁，成功则移除自己并返回 true。
    pub fn lock_read(&self) -> bool {
        if self.try_lock_read() {
            return true;
        }
        if self.spin_lock_read_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径（对齐 bcachefs __six_lock_slowpath）
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Read,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Read, &mut wait, None) == 0
    }

    /// 获取 intent 锁（try → spin → sleep 分级等待）
    pub fn lock_intent(&self) -> bool {
        if self.try_lock_intent() {
            return true;
        }
        if self.spin_lock_intent_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Intent,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Intent, &mut wait, None) == 0
    }

    /// 获取写锁（try → spin → sleep 分级等待，完全独占）
    ///
    /// Sleep 路径委托 lock_slowpath 统一慢路径。WRITE_BIT 预设 + WAITING_WRITE_BIT 预设
    /// + trylock 重试都在 lock_slowpath 内部，对齐 bcachefs __six_lock_slowpath:
    /// atomic_add(HELD_write) → WAITING_write → trylock。
    pub fn lock_write(&self) -> bool {
        if self.try_lock_write() {
            return true;
        }
        if self.spin_lock_write_internal() {
            return true;
        }
        // Sleep 路径：委托 lock_slowpath 统一慢路径
        let mut wait = SixLockWaiter {
            trans_start_time: 0,
            thread: None,
            lock_want: SixLockType::Write,
            lock_acquired: false,
            slot_idx: 0,
        };
        self.lock_slowpath(SixLockType::Write, &mut wait, None) == 0
    }

    /// 释放写锁
    ///
    /// 对应 bcachefs six_unlock_ip(Write) six.c:812-839 + do_six_unlock_type(Write) six.c:771-795。
    /// seq++ 对齐 six_unlock_ip six.c:835-836。
    /// 唤醒 Read 路径：do_six_unlock_type 调用 six_lock_wakeup(lock, state, SIX_LOCK_read) six.c:794。
    pub fn unlock_write(&self) {
        let recurse = unsafe { *self.write_recurse.get() };
        if recurse > 0 {
            unsafe {
                *self.write_recurse.get() = recurse - 1;
            }
            return;
        }
        unsafe {
            *self.write_owner.get() = None;
        }
        self.seq.fetch_add(1, Ordering::Release);
        self.state.fetch_and(!WRITE_BIT, Ordering::Release);
        // 写锁释放 → 可能可以唤醒 read / intent waiter
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Read);
    }

    // ── 升级操作 ──

    /// 尝试将 intent 锁升级为写锁
    ///
    /// 调用者必须已持有 intent 锁。
    /// 升级要求：read_count == 0（无活跃读者）。
    ///
    /// volmount 扩展: bcachefs 没有独立的 intent→write 升级函数。
    /// bcachefs 中六锁写入的 lock_fail=HELD_read（six.c:63），
    /// 因此 six_trylock_write() 可在持有 INTENT_BIT 时直接成功。
    /// Rust 的 try_lock_write 检查 INTENT_BIT（额外约束），
    /// 故需要此函数做 intent→write 升级。
    pub fn try_upgrade_intent_to_write(&self) -> bool {
        debug_assert!(self.is_intent_locked_by_current(), "must hold intent lock");

        // 先标记 write bit（见 locking.h 的"先标记后加锁"模式）
        // 但此处先尝试 CAS 升级
        let state = self.read_state();
        if self.read_count(state) > 0 {
            return false; // 有读者存在，不能升级
        }
        // intent 锁已持有（intent bit = 1），只需设置 write bit
        // 要求: read_count == 0, write == 0, intent == 1（我们持有）
        if self.has_write_lock(state) {
            return false;
        }

        // 检查 percpu readers
        if let Some(ref readers) = self.readers {
            for reader in readers.iter() {
                if reader.load(Ordering::Acquire) > 0 {
                    return false;
                }
            }
        }

        let ok = self
            .state
            .compare_exchange(
                state,
                state | WRITE_BIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok();
        if ok {
            unsafe {
                *self.write_owner.get() = Some(thread::current().id());
            }
        }
        ok
    }

    /// 将写锁降级为 intent 锁
    pub fn downgrade_write_to_intent(&self) {
        debug_assert!(self.is_write_locked_by_current(), "must hold write lock");
        unsafe {
            *self.write_owner.get() = None;
        }
        // 递增 seq 使等待者能检测到锁状态变化（handoff 协议需要）
        self.seq.fetch_add(1, Ordering::Release);
        self.state.fetch_and(!WRITE_BIT, Ordering::Release);
        // intent 锁继续持有（intent bit 保持不变）
        // 只唤醒 reader（writer/intent waiter 仍被 intent 锁阻塞）
        let state = self.read_state();
        self.wakeup_lock_type(state, SixLockType::Read);
    }

    /// 将 intent 锁降级为读锁
    pub fn downgrade_intent_to_read(&self) {
        debug_assert!(self.is_intent_locked_by_current(), "must hold intent lock");
        // 释放 intent bit，但增加 read_count
        self.state.fetch_add(1, Ordering::Acquire); // read_count += 1
        self.state.fetch_and(!INTENT_BIT, Ordering::Release);
        unsafe {
            *self.intent_owner.get() = None;
        }
        // 降级后调用者持有读锁，递增 THREAD_READ_CNT
        // 使得后续 unlock_read() 不会下溢
        if self.readers.is_none() {
            THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
        }
    }

    /// 尝试将读锁升级为 intent 锁
    ///
    /// 调用者必须已持有读锁。这是一个"maybe upgrade"操作，
    /// 用于 B 树遍历优化：从根向下先加读锁遍历，遇到需要修改的
    /// 节点时尝试升级为 intent 锁。
    ///
    /// 对应 bcachefs six_lock_tryupgrade() / six_trylock_convert(read→intent)。
    pub fn try_upgrade_read_to_intent(&self) -> bool {
        // SIX 锁不追踪每个线程的读锁持有情况，因此无法在这里验证
        // 调用者需要确保自己持有读锁。
        // 如果当前线程不持有读锁，此方法会优雅返回 false。

        if let Some(ref readers) = self.readers {
            // Percpu 模式：decrement percpu reader, CAS set intent bit
            let slot = current_thread_slot() as usize % readers.len();
            debug_assert!(
                readers[slot].load(Ordering::Relaxed) > 0,
                "current thread must have a percpu reader"
            );
            readers[slot].fetch_sub(1, Ordering::Relaxed);
            fence(Ordering::Acquire);

            let state = self.read_state();
            if state & (INTENT_BIT | WRITE_BIT) != 0 {
                // 有人持有 intent 或 write → 回滚
                readers[slot].fetch_add(1, Ordering::Relaxed);
                return false;
            }

            // 设置 intent bit
            let ok = self
                .state
                .compare_exchange(
                    state,
                    state | INTENT_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();
            if ok {
                unsafe {
                    *self.intent_owner.get() = Some(thread::current().id());
                }
                return true;
            }
            // CAS 失败 → 回滚 percpu reader
            readers[slot].fetch_add(1, Ordering::Relaxed);
            false
        } else {
            // 标准模式：CAS read_count - 1, set intent bit
            loop {
                let state = self.read_state();
                if state & (INTENT_BIT | WRITE_BIT) != 0 {
                    return false;
                }
                if self.read_count(state) == 0 {
                    return false; // 没有读者（说明我们不持有读锁）
                }
                if self
                    .state
                    .compare_exchange_weak(
                        state,
                        (state - 1) | INTENT_BIT, // read_count -= 1, intent_bit = 1
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    unsafe {
                        *self.intent_owner.get() = Some(thread::current().id());
                    }
                    // 读锁已释放（state.read_count - 1），同步递减 THREAD_READ_CNT
                    // 防止 try_lock_write 在排除自身读者时错误地忽略其他线程的读者
                    THREAD_READ_CNT.with(|c| c.set(c.get() - 1));
                    return true;
                }
                std::hint::spin_loop();
            }
        }
    }

    // ── 升级自旋方法 ──

    /// 自旋等待从读锁升级为 intent 锁
    ///
    /// 要求调用者已持有读锁。自旋等待其他 intent/write 持有者释放。
    /// 比 try_upgrade_read_to_intent 多一次 SPIN_COUNT 重试。
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    pub fn upgrade_read_to_intent(&self) -> bool {
        if self.try_upgrade_read_to_intent() {
            return true;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_upgrade_read_to_intent() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU
        std::thread::yield_now();
        if self.try_upgrade_read_to_intent() {
            return true;
        }
        false
    }

    /// 自旋等待从 intent 锁升级为写锁
    ///
    /// 要求调用者已持有 intent 锁。自旋等待所有读者释放。
    /// 比 try_upgrade_intent_to_write 多一次 SPIN_COUNT 重试。
    /// 超过 SPIN_COUNT 次后让步 CPU（thread::yield_now），减少 CPU busy-wait 消耗。
    pub fn upgrade_intent_to_write(&self) -> bool {
        if self.try_upgrade_intent_to_write() {
            return true;
        }
        for _ in 0..SPIN_COUNT {
            if self.try_upgrade_intent_to_write() {
                return true;
            }
            std::hint::spin_loop();
        }
        // Phase C1: 自旋超时后让步 CPU
        std::thread::yield_now();
        if self.try_upgrade_intent_to_write() {
            return true;
        }
        false
    }

    // ── Relock API（序列号验证重入） ──

    /// 尝试重入读锁，验证序列号未变化
    ///
    /// 对应 bcachefs six_relock_read()（宏展开为 six_relock_ip six.c:470-482）。
    /// 锁的序列号在写锁释放时递增。如果当前序列号与 `expected_seq` 一致，
    /// 说明自解锁以来没有写操作，数据仍然有效。
    ///
    /// 用法：
    /// ```text
    /// let seq = lock.seq();
    /// lock.unlock_read();
    /// some_blocking_operation();
    /// if lock.relock_read(seq) {
    ///     // 数据未变，安全继续
    /// }
    /// ```
    pub fn relock_read(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Read, expected_seq, 0)
    }

    /// 尝试重入 intent 锁，验证序列号未变化
    ///
    /// 对应 bcachefs six_relock_intent()。
    /// 语义同 relock_read，但使用 intent 锁。
    pub fn relock_intent(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Intent, expected_seq, 0)
    }

    /// 尝试重入写锁，验证序列号未变化（对应 bcachefs six_relock_write）
    ///
    /// 写锁重入在 C 中也有定义，但实践中写锁不会轻易释放后重入。
    pub fn relock_write(&self, expected_seq: u64) -> bool {
        self.relock_ip(SixLockType::Write, expected_seq, 0)
    }

    // ── 状态查询 ──

    /// 当前是否有写锁被持有
    pub fn is_write_locked(&self) -> bool {
        self.read_state() & WRITE_BIT != 0
    }

    /// 当前是否有 intent 锁被持有
    pub fn is_intent_locked(&self) -> bool {
        self.read_state() & INTENT_BIT != 0
    }

    /// 当前线程是否持有写锁
    pub fn is_write_locked_by_current(&self) -> bool {
        unsafe { *self.write_owner.get() == Some(thread::current().id()) }
    }

    /// 当前线程是否持有 intent 锁
    pub fn is_intent_locked_by_current(&self) -> bool {
        unsafe { *self.intent_owner.get() == Some(thread::current().id()) }
    }

    /// 当前读者数量
    pub fn reader_count(&self) -> u32 {
        let state = self.read_state();
        let atomic_count = self.read_count(state);
        if let Some(ref readers) = self.readers {
            let percpu_count: u32 = readers.iter().map(|r| r.load(Ordering::Relaxed)).sum();
            atomic_count + percpu_count
        } else {
            atomic_count
        }
    }

    // ── 销毁 / 清理 ──

    /// 释放锁占用的资源（对应 bcachefs six_lock_exit）
    ///
    /// 释放 percpu readers 和 wait_fifo 中的等待者。
    /// 在 Rust 中，Drop 会处理大部分清理工作，此方法提供显式控制。
    pub fn destroy(&mut self) {
        self.readers = None;
        // wait_fifo 的空槽清理在其 Drop 中完成
    }

    // ── 通用 trylock（对应 bcachefs six_trylock_ip） ──

    /// 通用 trylock —— 按类型尝试获取锁（对应 bcachefs six_trylock_ip）
    ///
    /// `ip` 参数用于 lockdep，在 Rust 中保留以匹配 C API 签名但未使用。
    pub fn trylock_ip(&self, type_: SixLockType, _ip: usize) -> bool {
        match type_ {
            SixLockType::Read => self.try_lock_read(),
            SixLockType::Intent => self.try_lock_intent(),
            SixLockType::Write => self.try_lock_write(),
        }
    }

    // ── 通用 unlock（对应 bcachefs six_unlock_ip） ──

    /// 通用 unlock —— 按类型释放锁（对应 bcachefs six_unlock_ip six.c:812-839）
    ///
    /// `ip` 参数用于 lockdep，在 Rust 中保留以匹配 C API 签名但未使用。
    /// 注意：C 版有 recurse 检查和 seq++（写锁），分别由各 unlock_* 实现。
    pub fn unlock_ip(&self, type_: SixLockType, _ip: usize) {
        match type_ {
            SixLockType::Read => self.unlock_read(),
            SixLockType::Intent => self.unlock_intent(),
            SixLockType::Write => self.unlock_write(),
        }
    }

    // ── 通用 relock（对应 bcachefs six_relock_ip） ──

    /// 通用 relock —— 验证序列号后重新加锁（对应 bcachefs six_relock_ip six.c:470-482）
    ///
    /// 返回 true 表示加锁成功且序列号未变化，false 表示序列号已变化或加锁失败。
    pub fn relock_ip(&self, type_: SixLockType, seq: u64, ip: usize) -> bool {
        if self.seq() != seq {
            return false;
        }
        if !self.trylock_ip(type_, ip) {
            return false;
        }
        // 双检：获取锁后再次验证 seq
        if self.seq() != seq {
            self.unlock_ip(type_, ip);
            return false;
        }
        true
    }

    // ── 通用阻塞加锁（对应 bcachefs six_lock_ip_waiter） ──

    /// 最通用的阻塞加锁函数（对应 bcachefs six_lock_ip_waiter）
    ///
    /// 完整的 try → spin → sleep 三级等待。
    /// `wait` 是栈上分配的 SixLockWaiter，需要由调用者提供。
    /// `should_sleep` 是可选的回调，在 park 前调用；返回 0=继续等待，非 0=中止。
    ///
    /// 返回 0 表示加锁成功，非 0 表示 `should_sleep` 返回的错误码。
    pub fn lock_ip_waiter(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
        _ip: usize,
    ) -> i32 {
        // 快速路径：trylock
        if self.trylock_ip(type_, 0) {
            return 0;
        }
        // 自旋路径
        match type_ {
            SixLockType::Read => {
                if self.spin_lock_read_internal() {
                    return 0;
                }
            }
            SixLockType::Intent => {
                if self.spin_lock_intent_internal() {
                    return 0;
                }
            }
            SixLockType::Write => {
                if self.spin_lock_write_internal() {
                    return 0;
                }
            }
        }
        // Sleep 路径
        self.lock_slowpath(type_, wait, should_sleep)
    }

    /// 跳过初始 trylock，直接进入加锁慢路径（对应 bcachefs six_lock_contended）
    ///
    /// 调用者已在外部做过 trylock 并观测到锁被争用。
    /// 避免在已知锁被争用时浪费一次 CAS 操作。
    pub fn lock_contended(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
        _ip: usize,
    ) -> i32 {
        self.lock_slowpath(type_, wait, should_sleep)
    }

    /// 加锁慢路径 —— push waiter + park/wake 循环
    fn lock_slowpath(
        &self,
        type_: SixLockType,
        wait: &mut SixLockWaiter,
        should_sleep: Option<&SixLockShouldSleepFn>,
    ) -> i32 {
        wait.thread = Some(thread::current());
        wait.lock_want = type_;
        wait.lock_acquired = false;

        // 写锁需要预设 WRITE_BIT + WAITING_WRITE_BIT 防止读者饥饿
        if type_ == SixLockType::Write {
            // S4: 先预设 WRITE_BIT（匹配 bcachefs 和 lock_write 行为）
            self.state.fetch_or(WRITE_BIT, Ordering::SeqCst);
            self.state.fetch_or(WAITING_WRITE_BIT, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            // S6: 双检用 try_lock_write_preset 而非 trylock_ip（try_lock_write 会检查
            // WRITE_BIT 是否已设置，但我们自己刚设了 WRITE_BIT，导致 try_lock_write 总是
            // 返回 false）。try_lock_write_preset 只检查读者计数，与 bcachefs
            // __do_six_trylock(try=false) 对齐。
            let ok = self.try_lock_write_preset();
            if ok {
                self.clear_waiting_bit(type_);
                wait.lock_acquired = true;
                return 0;
            }
        }

        // 创建带外 handoff 信号
        let flag = Arc::new(AtomicBool::new(false));
        // 入队 WaitFifo
        let waiter_box = WaiterBox {
            trans_id: wait.trans_start_time,
            lock_type: type_,
            seq: self.seq.load(Ordering::Relaxed),
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: Some(flag.clone()),
            percpu_slot: current_thread_slot(),
        };
        // push_waiter_with_recheck 在 wait_lock 内设 WAITING bit → trylock 重试 → 入队
        // 注意：should_sleep_fn 在入队之后、park 循环内调用（对齐 bcachefs __six_lock_slowpath）
        // 内置的 trylock 替代了之前的独立 trylock_ip + push_waiter 两步，闭合 C1 竞态窗口
        if self.push_waiter_with_recheck(&waiter_box) {
            // push_waiter_with_recheck 内 try_lock_read 增了 state.read_count
            // 但未增线程本地 THREAD_READ_CNT，此处补偿
            if type_ == SixLockType::Read && self.readers.is_none() {
                THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
            }
            wait.lock_acquired = true;
            return 0;
        }

        loop {
            // park 前调用 should_sleep_fn
            if let Some(ref sleep_fn) = should_sleep {
                let ret = sleep_fn(self, wait);
                if ret != 0 {
                    // S5: 对应 bcachefs __six_lock_slowpath should_sleep 错误路径
                    // wait_lock 保护下原子检查 waker 是否已替我们声明锁
                    let _lock = self.wait_lock.lock();
                    let acquired = flag.load(Ordering::Acquire);
                    if !acquired {
                        self.wait_fifo.remove_by_thread(thread::current().id());
                        if self.wait_fifo.is_empty() {
                            if self.read_state() & WAITING_READ_BIT != 0 {
                                self.state.fetch_and(!WAITING_READ_BIT, Ordering::Release);
                            }
                            if self.read_state() & WAITING_INTENT_BIT != 0 {
                                self.state.fetch_and(!WAITING_INTENT_BIT, Ordering::Release);
                            }
                            if self.read_state() & WAITING_WRITE_BIT != 0 {
                                self.state.fetch_and(!WAITING_WRITE_BIT, Ordering::Release);
                            }
                        }
                    }
                    drop(_lock);
                    if acquired {
                        self.unlock_ip(type_, 0);
                    } else if type_ == SixLockType::Write {
                        self.state.fetch_and(!WRITE_BIT, Ordering::Release);
                        let s = self.read_state();
                        self.wakeup_lock_type(s, SixLockType::Read);
                    }
                    wait.lock_acquired = false;
                    return ret;
                }
            }
            thread::park();
            // O(1) 带外检查：waker 已替我们声明锁（通过 lock_acquired_flag）
            if flag.load(Ordering::Acquire) {
                // waker 已替我们声明锁，跳过 trylock
                // 读锁路径：waker 增了 state 但未增线程本地计数 THREAD_READ_CNT
                if type_ == SixLockType::Read && self.readers.is_none() {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
                self.remove_self_from_fifo();
                wait.lock_acquired = true;
                return 0;
            }
            // 非 handoff wake：使用 per-type 正确 trylock（对齐 bcachefs __six_lock_slowpath
            // 的 trylock 行为）。Write 路径必须用 try_lock_write_preset（因为 WRITE_BIT 已预设，
            // try_lock_write 会错误地认为写锁已被其他线程持有）。
            let try_ok = match type_ {
                SixLockType::Read => self.try_lock_read(),
                SixLockType::Intent => self.try_lock_intent(),
                SixLockType::Write => self.try_lock_write_preset(),
            };
            if try_ok {
                if type_ == SixLockType::Read && self.readers.is_none() {
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
                self.remove_self_from_fifo();
                wait.lock_acquired = true;
                return 0;
            }
        }
    }

    // ── Restart API（对应 bcachefs six_lock_restart） ──

    /// 重新加锁 — 释放当前持有的锁，以新类型重新加锁
    ///
    /// 按 Write > Intent > Read 优先级检测当前锁类型，解锁后重新加锁。
    /// 调用者需注意 unlock → lock 之间有窗口期，其他线程可能在此期间获得锁。
    /// btree 事务重启循环会处理此情况。
    ///
    /// 如果当前未持有任何锁，等同于直接 `lock(new_type)`。
    ///
    /// volmount 扩展: bcachefs 中没有 six_lock_restart 函数。
    /// bcachefs 的等价模式是在事务重启时调用 six_unlock_type() 然后 six_lock_type()。
    pub fn lock_restart(&self, new_type: SixLockType) -> bool {
        // 按 写 → intent → 读 优先级检测并释放（最互斥的最先检测）
        if self.is_write_locked_by_current() {
            self.unlock_write();
        } else if self.is_intent_locked_by_current() {
            self.unlock_intent();
        } else {
            // 检测当前线程是否持有读锁
            let has_read = if let Some(ref readers) = self.readers {
                let slot = current_thread_slot() as usize % readers.len();
                readers[slot].load(Ordering::Relaxed) > 0
            } else {
                THREAD_READ_CNT.with(|c| c.get() > 0)
            };
            if has_read {
                self.unlock_read();
            }
            // 未持有任何锁时不执行 unlock 操作
        }
        match new_type {
            SixLockType::Read => self.lock_read(),
            SixLockType::Intent => self.lock_intent(),
            SixLockType::Write => self.lock_write(),
        }
    }

    // ── Read→Write 升级（对应 bcachefs six_lock_read_to_write） ──

    /// 尝试将读锁直接升级为写锁（对应 bcachefs six_lock_tryread_to_write）
    ///
    /// 快速路径：移除当前线程的 reader 计数，检查无其他读者/写者后直接设置 write bit。
    /// 失败时恢复 reader 计数并返回 false。
    pub fn try_lock_read_to_write(&self) -> bool {
        // 1. 移除当前线程的 reader 计数
        self.lock_readers_add(-1);
        fence(Ordering::Acquire);

        // 2. 检查在 percpu 模式中是否还有其他读者
        let has_other_readers = if let Some(ref readers) = self.readers {
            let my_slot = current_thread_slot() as usize % readers.len();
            readers
                .iter()
                .enumerate()
                .any(|(i, r)| i != my_slot && r.load(Ordering::Relaxed) > 0)
        } else {
            false
        };

        // 3. 检查 state：无其他读者、无 intent、无 write
        let state = self.state.load(Ordering::Relaxed);
        let read_count = self.read_count(state);
        if !has_other_readers && read_count == 0 && (state & (INTENT_BIT | WRITE_BIT)) == 0 {
            // 无竞争：设置 write bit + intent bit（write 隐含 intent 语义）
            let new_state = (state & !READ_COUNT_MASK) | WRITE_BIT | INTENT_BIT;
            if self
                .state
                .compare_exchange_weak(state, new_state, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                unsafe {
                    *self.write_owner.get() = Some(thread::current().id());
                }
                return true;
            }
        }

        // 4. 失败：恢复 reader 计数
        self.lock_readers_add(1);
        false
    }

    /// 将读锁升级为写锁（对应 bcachefs six_lock_read_to_write）
    ///
    /// 先尝试快速升级（try_lock_read_to_write），失败时：
    /// 1. 释放读锁（unlock_read）
    /// 2. 以标准方式获取写锁（lock_write）
    pub fn lock_read_to_write(&self) -> bool {
        if self.try_lock_read_to_write() {
            return true;
        }
        self.unlock_read();
        self.lock_write()
    }

    // ── 锁转换 API（对应 bcachefs six_lock_downgrade 等） ──

    /// 将 intent 锁降级为读锁（对应 bcachefs six_lock_downgrade）
    ///
    /// 调用者必须已持有 intent 锁。
    /// 降级后调用者持有读锁。
    pub fn lock_downgrade(&self) {
        // 对应 C: six_lock_increment(lock, SIX_LOCK_read) + six_unlock_intent(lock)
        self.lock_increment(SixLockType::Read);
        self.unlock_intent();
    }

    /// 尝试将读锁升级为 intent 锁（对应 bcachefs six_lock_tryupgrade）
    ///
    /// 调用者必须已持有读锁。
    /// 返回 true 表示升级成功，调用者现在持有 intent 锁。
    pub fn lock_tryupgrade(&self) -> bool {
        self.try_upgrade_read_to_intent()
    }

    /// 通用锁类型转换（对应 bcachefs six_trylock_convert）
    ///
    /// 支持 read↔intent 之间的转换（不含 write）。
    /// `from` 和 `to` 必须不同且均不为 write。
    pub fn trylock_convert(&self, from: SixLockType, to: SixLockType) -> bool {
        debug_assert!(
            from != SixLockType::Write && to != SixLockType::Write,
            "trylock_convert does not support write locks"
        );
        if to == from {
            return true;
        }
        if to == SixLockType::Read {
            // intent → read
            self.lock_downgrade();
            true
        } else {
            // read → intent
            self.lock_tryupgrade()
        }
    }

    // ── 重入计数 API（对应 bcachefs six_lock_increment） ──

    /// 增加已持有锁的引用计数（对应 bcachefs six_lock_increment）
    ///
    /// 用于上层提供重入语义：当已知锁已被当前线程以 `type_` 类型持有时，
    /// 调此方法增加计数，后续需要相应次数的 unlock 才能完全释放。
    ///
    /// 对于 Read：增加 reader count（percpu 或 atomic）
    /// 对于 Intent：增加 intent_recurse
    /// 对于 Write：增加 write_recurse
    pub fn lock_increment(&self, type_: SixLockType) {
        match type_ {
            SixLockType::Read => {
                if let Some(ref readers) = self.readers {
                    let slot = current_thread_slot() as usize % readers.len();
                    readers[slot].fetch_add(1, Ordering::Relaxed);
                } else {
                    self.state.fetch_add(1, Ordering::Relaxed);
                    THREAD_READ_CNT.with(|c| c.set(c.get() + 1));
                }
            }
            SixLockType::Intent => unsafe {
                *self.intent_recurse.get() += 1;
            },
            SixLockType::Write => unsafe {
                *self.write_recurse.get() += 1;
            },
        }
    }

    // ── 等待者管理 API ──

    /// 唤醒指定类型等待者（对应 bcachefs __six_lock_wakeup）
    ///
    /// 核心架构变更：waker 替 waiter 调 trylock（_for 函数），按 index 删除 FIFO（O(1)），
    /// 通过 lock_acquired_flag（Arc<AtomicBool>）做带外 handoff。
    ///
    /// 流程：
    /// 1. 快速检查 state & waiting_bit — 无等待者则返回
    /// 2. wait_lock.lock() — 获取自旋锁
    /// 3. 双检 waiting_bit（取锁后重新确认）
    /// 4. with_rcu 下遍历 FIFO slot
    /// 5. 根据 lock_type 分支：
    ///    - Read: 遍历所有 read waiter；对每个调 try_lock_read_for；成功则 remove_by_index → flag.store → unpark
    ///    - Write/Intent: 统计 n_matches，找最老 waiter；调 trylock_for(waiter.tid)；成功则 remove → flag → unpark
    /// 6. 根据 n_matches 结果清除 WAITING bit
    ///
    /// 内部唤醒逻辑 — 在 wait_lock 持有 + RCU 临界区内调用
    ///
    /// 对应 bcachefs __six_lock_wakeup()。支持级联唤醒：Read 路径
    /// 在 try_lock_read_for 失败时 cascade 到 Write 唤醒（在同一临界区内）。
    fn __wakeup_lock_type(&self, lock_type: SixLockType, rscs: &RcuRSCS) {
        match lock_type {
            SixLockType::Read => {
                let mut all_woke = true;
                let mut cascade_to_write = false;

                for (i, slot) in self.wait_fifo.slots().iter().enumerate() {
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        if !matches!(waiter.lock_type, SixLockType::Read) {
                            continue;
                        }
                        if self.try_lock_read_for(waiter.percpu_slot) {
                            let flag = waiter.lock_acquired_flag.clone();
                            let thread = waiter.thread.clone();
                            let _ = opt;
                            self.wait_fifo.remove_by_index(i);
                            if let Some(ref f) = flag {
                                f.store(true, Ordering::Release);
                            }
                            if let Some(ref t) = thread {
                                t.unpark();
                            }
                        } else {
                            all_woke = false;
                            // S2: cascading — try_lock_read_for 失败时
                            //（写锁持有），若有写者在等待则级联到 write 唤醒
                            if !cascade_to_write && self.read_state() & WAITING_WRITE_BIT != 0 {
                                cascade_to_write = true;
                            }
                        }
                    }
                }

                if all_woke {
                    self.state.fetch_and(!WAITING_READ_BIT, Ordering::Release);
                }

                // S2: 在同一 RCU 临界区内做 write 级联唤醒
                if cascade_to_write {
                    self.__wakeup_lock_type(SixLockType::Write, rscs);
                }
            }
            SixLockType::Intent | SixLockType::Write => {
                let mut oldest_idx = None;
                let mut oldest_trans_id = u64::MAX;
                let mut n_matches = 0;

                for (i, slot) in self.wait_fifo.slots().iter().enumerate() {
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        if waiter.lock_type != lock_type {
                            continue;
                        }
                        n_matches += 1;
                        if waiter.trans_id < oldest_trans_id {
                            oldest_trans_id = waiter.trans_id;
                            oldest_idx = Some(i);
                        }
                    }
                }

                if let Some(idx) = oldest_idx {
                    let slot = &self.wait_fifo.slots()[idx];
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        let tid = waiter.thread.as_ref().map(|t| t.id()).unwrap();
                        let flag = waiter.lock_acquired_flag.clone();
                        let thread = waiter.thread.clone();
                        let _ = opt;

                        let acquired = match lock_type {
                            SixLockType::Write => self.try_lock_write_preset_for(tid),
                            SixLockType::Intent => self.try_lock_intent_for(tid),
                            _ => unreachable!(),
                        };

                        if acquired {
                            self.wait_fifo.remove_by_index(idx);
                            if let Some(ref f) = flag {
                                f.store(true, Ordering::Release);
                            }
                            if let Some(ref t) = thread {
                                t.unpark();
                            }
                            // 成功移除一个 waiter：递减计数
                            // 对应 bcachefs __six_lock_wakeup: handoff 成功后
                            // n_matches > 1 保留 WAITING bit 供后续唤醒
                            // n_matches == 1 且已移除 → 下面清 WAITING bit
                            n_matches -= 1;
                        }
                        // handoff 失败 (acquired = false)：waiter 仍在 FIFO
                        // n_matches 不变 → 不清理 WAITING bit
                        // 对应 bcachefs __six_lock_wakeup: ret <= 0 goto out 跳过 six_clear_bitmask
                    }
                }

                // 只有 FIFO 中确实无剩余 waiter 时才清 WAITING bit
                // 对应 bcachefs __six_lock_wakeup: !oldest || (handoff成功 && n_matches==0)
                if n_matches == 0 {
                    let bit = match lock_type {
                        SixLockType::Write => WAITING_WRITE_BIT,
                        SixLockType::Intent => WAITING_INTENT_BIT,
                        _ => unreachable!(),
                    };
                    self.state.fetch_and(!bit, Ordering::Release);
                }
            }
        }
    }

    /// 唤醒指定类型等待者（对应 bcachefs six_lock_wakeup）
    ///
    /// 封装 wait_lock 获取 + 双检 + RCU 临界区，委派到 __wakeup_lock_type。
    fn wakeup_lock_type(&self, state: u32, lock_type: SixLockType) {
        let waiting_bit = match lock_type {
            SixLockType::Read => WAITING_READ_BIT,
            SixLockType::Intent => WAITING_INTENT_BIT,
            SixLockType::Write => WAITING_WRITE_BIT,
        };

        if state & waiting_bit == 0 {
            return;
        }

        // 对应 bcachefs six_lock_wakeup six.c:416-417:
        // 写锁唤醒时若读者仍活跃，直接跳过（reader 最终释放时会再次触发唤醒）
        // 防止 wait_lock 内 try_lock_write_preset_for 因读者活跃失败后错误清 WAITING bit
        if lock_type == SixLockType::Write && (state & READ_COUNT_MASK) != 0 {
            return;
        }

        let _lock = self.wait_lock.lock();

        if self.read_state() & waiting_bit == 0 {
            return;
        }

        with_rcu(|_rcu, thread| {
            thread.rscs(|rscs| {
                self.__wakeup_lock_type(lock_type, rscs);
            })
        });
    }

    /// 唤醒所有等待者（对应 bcachefs six_lock_wakeup_all six.c:969-995）
    ///
    /// 1. 逐个类型唤醒（每个独立获取/释放 wait_lock）
    /// 2. 对剩余 waiter（trylock 失败的）做无条件 unpark
    pub fn lock_wakeup_all(&self) {
        // 1. 逐个类型唤醒 (每个独立获取/释放 wait_lock)
        self.wakeup_lock_type(self.read_state(), SixLockType::Read);
        self.wakeup_lock_type(self.read_state(), SixLockType::Intent);
        self.wakeup_lock_type(self.read_state(), SixLockType::Write);

        // 2. 对剩余 waiter（trylock 失败的）做无条件 unpark
        //    这些 waiter 醒来后 flag 为 false，会回到 park 循环
        let _lock = self.wait_lock.lock();
        with_rcu(|_rcu, thread| {
            thread.rscs(|rscs| {
                for slot in self.wait_fifo.slots() {
                    let opt = slot.read(rscs);
                    if let Some(Some(ref waiter)) = opt.as_ref() {
                        if let Some(ref t) = waiter.thread {
                            t.unpark();
                        }
                    }
                }
            })
        });
    }

    /// 返回各锁类型的当前持有计数（对应 bcachefs six_lock_counts six.c:1004-1016）
    pub fn lock_counts(&self) -> SixLockCount {
        let state = self.read_state();
        SixLockCount {
            n: [
                if self.readers.is_some() {
                    // percpu 模式：从所有 slot 汇总
                    self.reader_count()
                } else {
                    // 标准模式：直接从 state 读取
                    self.read_count(state)
                },
                if self.has_intent_lock(state) { 1 } else { 0 }
                    + unsafe { *self.intent_recurse.get() },
                if self.has_write_lock(state) { 1 } else { 0 },
            ],
        }
    }

    /// 直接操作读者计数（对应 bcachefs six_lock_readers_add six.c:1039-1048）
    ///
    /// 用于上层实现重入：当同时持有读锁和 intent 锁时，
    /// 写锁获取需要暂时减去自身读锁计数。
    /// 调用者需确保计数不会变为负数。
    pub fn lock_readers_add(&self, nr: i32) {
        if let Some(ref readers) = self.readers {
            let slot = current_thread_slot() as usize % readers.len();
            if nr >= 0 {
                readers[slot].fetch_add(nr as u32, Ordering::Relaxed);
            } else {
                readers[slot].fetch_sub((-nr) as u32, Ordering::Relaxed);
            }
        } else {
            // atomic_add 支持有符号加法（负数值通过 wrapping 实现）
            self.state.fetch_add(nr as u32, Ordering::Relaxed);
            THREAD_READ_CNT.with(|c| c.set(c.get().wrapping_add(nr as u32)));
        }
    }

    // ── 类型化包装方法 ──

    // trylock_ip 类型化包装

    pub fn trylock_ip_read(&self, ip: usize) -> bool {
        self.trylock_ip(SixLockType::Read, ip)
    }

    pub fn trylock_ip_intent(&self, ip: usize) -> bool {
        self.trylock_ip(SixLockType::Intent, ip)
    }

    pub fn trylock_ip_write(&self, ip: usize) -> bool {
        self.trylock_ip(SixLockType::Write, ip)
    }

    // relock_ip 类型化包装

    pub fn relock_ip_read(&self, seq: u64, ip: usize) -> bool {
        self.relock_ip(SixLockType::Read, seq, ip)
    }

    pub fn relock_ip_intent(&self, seq: u64, ip: usize) -> bool {
        self.relock_ip(SixLockType::Intent, seq, ip)
    }

    pub fn relock_ip_write(&self, seq: u64, ip: usize) -> bool {
        self.relock_ip(SixLockType::Write, seq, ip)
    }

    // unlock_ip 类型化包装

    pub fn unlock_ip_read(&self, ip: usize) {
        self.unlock_ip(SixLockType::Read, ip);
    }

    pub fn unlock_ip_intent(&self, ip: usize) {
        self.unlock_ip(SixLockType::Intent, ip);
    }

    pub fn unlock_ip_write(&self, ip: usize) {
        self.unlock_ip(SixLockType::Write, ip);
    }

    /// nospin 标志是否已设置
    pub fn is_nospin(&self) -> bool {
        self.read_state() & NOSPIN_BIT != 0
    }

    /// 设置 nospin bit（跳过自旋，直接休眠）
    pub fn set_nospin(&self) {
        self.state.fetch_or(NOSPIN_BIT, Ordering::Relaxed);
    }

    /// 清除 nospin bit
    pub fn clear_nospin(&self) {
        self.state.fetch_and(!NOSPIN_BIT, Ordering::Relaxed);
    }

    /// 设置对应的等待标志位
    fn set_waiting_bit(&self, lock_type: SixLockType) {
        match lock_type {
            SixLockType::Read => {
                self.state.fetch_or(WAITING_READ_BIT, Ordering::Relaxed);
            }
            SixLockType::Intent => {
                self.state.fetch_or(WAITING_INTENT_BIT, Ordering::Relaxed);
            }
            SixLockType::Write => {
                self.state.fetch_or(WAITING_WRITE_BIT, Ordering::Relaxed);
            }
        }
    }

    /// 清除对应的等待标志位
    fn clear_waiting_bit(&self, lock_type: SixLockType) {
        match lock_type {
            SixLockType::Read => {
                self.state.fetch_and(!WAITING_READ_BIT, Ordering::Release);
            }
            SixLockType::Intent => {
                self.state.fetch_and(!WAITING_INTENT_BIT, Ordering::Release);
            }
            SixLockType::Write => {
                self.state.fetch_and(!WAITING_WRITE_BIT, Ordering::Release);
            }
        }
    }

    /// 推送等待者到 WaitFifo（带 wait_lock 保护的 trylock 重试）
    ///
    /// 对应 bcachefs `__six_lock_slowpath` 的 wait_lock 内重试协议（C1 fix）：
    ///
    /// 1. 持 `wait_lock` 设 WAITING bit
    /// 2. 在 wait_lock 内 trylock 重试（关闭 unlock → push_waiter 间的竞态窗口）
    /// 3. 若重试成功：清 WAITING bit，返回 `true`（锁已获取，未入队）
    /// 4. 若重试失败：入 FIFO，返回 `false`（等待者已入队，WAITING bit 已设）
    ///
    /// 调用者职责：
    /// - 返回 `true`：锁已获取，不应再 park（如读锁路径需同步 THREAD_READ_CNT）
    /// - 返回 `false`：等待者已入队，应进入 park+flag 循环
    fn push_waiter_with_recheck(&self, waiter: &WaiterBox) -> bool {
        let _lock = self.wait_lock.lock();

        // Step 1: 先设 WAITING bit（bcachefs 协议：在 wait_lock 内设，防止 unlock 漏唤醒）
        self.set_waiting_bit(waiter.lock_type);

        // Step 2: wait_lock 内 trylock 重试（对应 bcachefs __do_six_trylock(try=false)）
        // 检查锁是否在初始 trylock 失败后已被释放
        let acquired = match waiter.lock_type {
            SixLockType::Read => self.try_lock_read(),
            SixLockType::Intent => self.try_lock_intent(),
            SixLockType::Write => self.try_lock_write_preset(),
        };

        if acquired {
            // Step 3: 锁已可用，无需入队
            self.clear_waiting_bit(waiter.lock_type);
            return true;
        }

        // Step 4: 入 FIFO（WAITING bit 保持设置）
        if self
            .wait_fifo
            .push(
                waiter.trans_id,
                waiter.lock_type,
                waiter.seq,
                waiter.thread.clone(),
                waiter.percpu_slot,
                waiter.lock_acquired_flag.clone(),
            )
            .is_none()
        {
            // FIFO 满（不应发生），清 WAITING bit
            self.clear_waiting_bit(waiter.lock_type);
        }
        false
    }

    /// 推送等待者到 WaitFifo（无 trylock 重试，仅用于 FIFO 行为测试）
    ///
    /// 与 `push_waiter_with_recheck` 的区别：本方法不尝试 wait_lock 内重试，
    /// 直接将 waiter 入队并设 WAITING bit。仅用于 FIFO 测试用例验证入队/出队逻辑。
    #[cfg(test)]
    fn push_waiter_test(&self, waiter: &WaiterBox) -> bool {
        let _lock = self.wait_lock.lock();
        let pushed = self
            .wait_fifo
            .push(
                waiter.trans_id,
                waiter.lock_type,
                waiter.seq,
                waiter.thread.clone(),
                waiter.percpu_slot,
                waiter.lock_acquired_flag.clone(),
            )
            .is_some();
        if pushed {
            self.set_waiting_bit(waiter.lock_type);
        }
        pushed
    }

    /// 从 WaitFifo 中移除当前线程
    ///
    /// 在 park+loop 成功获取锁后调用，清理 fifo 中的等待记录。
    /// wait_lock 保护 FIFO remove 与 WAITING bit 清理的原子性。
    fn remove_self_from_fifo(&self) {
        let _lock = self.wait_lock.lock();
        self.wait_fifo.remove_by_thread(thread::current().id());
        if self.wait_fifo.is_empty() {
            if self.read_state() & WAITING_READ_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Read);
            }
            if self.read_state() & WAITING_INTENT_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Intent);
            }
            if self.read_state() & WAITING_WRITE_BIT != 0 {
                self.clear_waiting_bit(SixLockType::Write);
            }
        }
    }

    /// 当前锁状态的调试描述
    pub fn debug_state(&self) -> String {
        let state = self.read_state();
        format!(
            "SixLock{{ readers={}, intent={}, write={}, waiting_r={}, waiting_i={}, waiting_w={}, nospin={} }}",
            self.read_count(state),
            self.has_intent_lock(state),
            self.has_write_lock(state),
            (state & WAITING_READ_BIT) != 0,
            (state & WAITING_INTENT_BIT) != 0,
            (state & WAITING_WRITE_BIT) != 0,
            (state & NOSPIN_BIT) != 0,
        )
    }
}

impl Default for SixLock {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SixLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SixLock")
            .field("state", &self.read_state())
            .field("seq", &self.seq())
            .field("readers_count", &self.reader_count())
            .finish()
    }
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    // ── 基本功能测试 ──

    #[test]
    fn test_read_lock_basic() {
        let lock = SixLock::new();
        assert!(lock.try_lock_read());
        assert!(lock.try_lock_read()); // 读锁可共享
                                       // bcachefs 锁支持重入：同线程持有读锁时可升级到写锁
        assert!(lock.try_lock_write()); // 自身读锁可排除，写锁成功
        lock.unlock_write();
        lock.unlock_read();
        lock.unlock_read();
        assert!(lock.try_lock_write()); // 读者释放后写锁成功
        lock.unlock_write();
    }

    #[test]
    fn test_write_lock_exclusive() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        assert!(!lock.try_lock_read()); // 写锁阻塞读（同线程）

        // 从另一个线程测试写锁排他
        let same = Arc::new(lock);
        let l = same.clone();
        let h = thread::spawn(move || {
            assert!(
                !l.try_lock_write(),
                "other thread should not get write lock"
            );
        });
        h.join().unwrap();
        same.unlock_write();
        assert!(same.try_lock_read()); // 写释放后可读
        same.unlock_read();
    }

    #[test]
    fn test_intent_lock() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.try_lock_intent());
        assert!(lock.try_lock_read()); // intent 不阻塞读
        lock.unlock_read();

        // 从另一个线程测试 intent 之间互斥
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(
                !l.try_lock_intent(),
                "other thread should not get intent lock"
            );
        });
        h.join().unwrap();

        assert!(!lock.try_lock_write()); // intent 阻塞写
        lock.unlock_intent();
        assert!(lock.try_lock_write()); // intent 释放后写成功
        lock.unlock_write();
    }

    #[test]
    fn test_intent_reentrant() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        assert!(lock.try_lock_intent()); // 同线程重入
        lock.unlock_intent();
        assert!(lock.is_intent_locked()); // 还有一层
        lock.unlock_intent();
        assert!(!lock.is_intent_locked());
    }

    #[test]
    fn test_write_reentrant() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        assert!(lock.try_lock_write()); // 同线程写锁重入
        lock.unlock_write();
        assert!(lock.is_write_locked()); // 还有一层
        lock.unlock_write();
        assert!(!lock.is_write_locked());
    }

    // ── 升级/降级测试 ──

    #[test]
    fn test_upgrade_intent_to_write() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());

        // 有读者时不能升级
        let r1 = lock.try_lock_read();
        assert!(r1);
        assert!(!lock.try_upgrade_intent_to_write());
        lock.unlock_read();

        // 无读者时可以升级
        assert!(lock.try_upgrade_intent_to_write());
        assert!(lock.is_write_locked());
        lock.unlock_write();

        // 写释放后 intent 还在
        assert!(lock.is_intent_locked());
        lock.unlock_intent();
    }

    #[test]
    fn test_downgrade_write_to_intent() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        assert!(lock.try_upgrade_intent_to_write());
        lock.downgrade_write_to_intent();
        assert!(lock.is_intent_locked());
        assert!(!lock.is_write_locked());
        // 降级后读锁可获取
        assert!(lock.try_lock_read());
        lock.unlock_read();
        lock.unlock_intent();
    }

    #[test]
    fn test_downgrade_intent_to_read() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        lock.downgrade_intent_to_read();
        assert!(!lock.is_intent_locked());
        // 现在持有读锁，可以和其他读者共享
        let r1 = lock.try_lock_read();
        assert!(r1);
        lock.unlock_read();
        lock.unlock_read();
    }

    // ── 锁冲突矩阵测试 ──

    #[test]
    fn test_lock_conflict_matrix() {
        assert!(!lock_conflicts(SixLockType::Read, SixLockType::Read));
        assert!(!lock_conflicts(SixLockType::Read, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Read, SixLockType::Write));
        assert!(!lock_conflicts(SixLockType::Intent, SixLockType::Read));
        assert!(lock_conflicts(SixLockType::Intent, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Intent, SixLockType::Write));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Read));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Intent));
        assert!(lock_conflicts(SixLockType::Write, SixLockType::Write));
    }

    // ── Percpu reader 测试 ──

    #[test]
    fn test_percpu_read_lock() {
        let lock = SixLock::with_percpu(8);
        assert!(lock.try_lock_read());
        // percpu 模式下，read_count 应该反映 percpu + atomic
        assert!(lock.reader_count() > 0);
        lock.unlock_read();
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn test_percpu_write_drain() {
        let lock = SixLock::with_percpu(4);
        assert!(lock.try_lock_read());
        lock.unlock_read();
        assert!(lock.try_lock_write()); // percpu readers drained
        lock.unlock_write();
    }

    /// 验证 percpu 模式下的 read→write 升级（bcachefs 重入语义）
    #[test]
    fn test_percpu_read_to_write_upgrade() {
        let lock = SixLock::with_percpu(8);
        // 持有 percpu 读锁时获取写锁应成功
        assert!(lock.try_lock_read());
        assert!(
            lock.try_lock_write(),
            "percpu read→write upgrade should succeed"
        );
        // 此时同时持有读+写
        lock.unlock_write();
        lock.unlock_read();

        // 多线程场景：其他线程的读锁应阻止写锁
        let lock = Arc::new(SixLock::with_percpu(8));
        let l2 = lock.clone();
        let h = std::thread::spawn(move || {
            assert!(l2.try_lock_read()); // 另一个线程持有读锁
            std::thread::sleep(std::time::Duration::from_millis(50));
            l2.unlock_read();
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(lock.try_lock_read()); // 当前线程也持有读锁
                                       // 其他线程有读者 → try_lock_write 应失败
        assert!(
            !lock.try_lock_write(),
            "other thread's percpu reader should block write"
        );
        lock.unlock_read();
        h.join().unwrap();
        // 所有读者释放后写锁应成功
        assert!(lock.try_lock_write());
        lock.unlock_write();
    }

    // ── 并发测试 ──

    #[test]
    fn test_concurrent_readers() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    assert!(l.try_lock_read());
                    // 模拟一些工作
                    std::hint::spin_loop();
                    l.unlock_read();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // 验证没有持锁泄漏
        assert_eq!(lock.reader_count(), 0);
        assert!(lock.try_lock_write());
        lock.unlock_write();
    }

    #[test]
    fn test_read_write_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        // 一个写线程
        let l = lock.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                loop {
                    if l.try_lock_write() {
                        std::hint::spin_loop();
                        l.unlock_write();
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        }));

        // 多个读线程
        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    loop {
                        if l.try_lock_read() {
                            std::hint::spin_loop();
                            l.unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn test_seq_increment_on_write_unlock() {
        let lock = SixLock::new();
        let s1 = lock.seq();
        assert!(lock.try_lock_write());
        lock.unlock_write();
        let s2 = lock.seq();
        assert!(s2 > s1, "seq should increment after write unlock");
    }

    // ── 升级 API 测试 ──

    #[test]
    fn test_upgrade_read_to_intent() {
        let lock = SixLock::new();
        assert!(lock.try_lock_read());
        // 持有读锁时可以升级为 intent
        assert!(lock.try_upgrade_read_to_intent());
        assert!(lock.is_intent_locked_by_current());
        // intent 已持有，读锁已释放（read_count 已递减）
        // 写锁应该被 intent 阻塞
        assert!(!lock.try_lock_write());
        lock.unlock_intent();
    }

    #[test]
    fn test_upgrade_read_to_intent_fail_when_conflict() {
        let lock = Arc::new(SixLock::new());
        // 本线程持有读锁
        assert!(lock.try_lock_read());
        // 另一线程持有 intent 锁（应该阻止升级）
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.try_lock_intent());
        });
        h.join().unwrap();
        // 别人持有 intent，升级应该失败
        assert!(!lock.try_upgrade_read_to_intent());
        // 读锁仍然在
        lock.unlock_read();
        // 释放对方的 intent
        // （对方线程已结束，但 intent bit 还在——这是设计约束）
        // 实际使用中 intent 由持有者释放
    }

    #[test]
    fn test_upgrade_read_to_intent_percpu() {
        let lock = SixLock::with_percpu(8);
        assert!(lock.try_lock_read());
        assert!(lock.try_upgrade_read_to_intent());
        assert!(lock.is_intent_locked_by_current());
        lock.unlock_intent();
    }

    #[test]
    fn test_upgrade_read_to_intent_not_holding_read() {
        let lock = SixLock::new();
        // 没有持有读锁时，升级应该失败（但 debug_assert 会在 debug 模式 panic）
        // release 模式下返回 false（因为 read_count == 0）
        assert!(!lock.try_upgrade_read_to_intent());
    }

    // ── Relock API 测试 ──

    #[test]
    fn test_relock_read_success() {
        let lock = SixLock::new();
        assert!(lock.try_lock_read());
        let seq = lock.seq();
        lock.unlock_read();
        // 没有写操作，relock 应该成功
        assert!(lock.relock_read(seq));
        lock.unlock_read();
    }

    #[test]
    fn test_relock_read_fail_after_write() {
        let lock = SixLock::new();
        assert!(lock.try_lock_read());
        let seq = lock.seq();
        lock.unlock_read();

        // 中间发生写操作
        assert!(lock.try_lock_write());
        lock.unlock_write();

        // seq 已变化，relock 应该失败
        assert!(!lock.relock_read(seq));
    }

    #[test]
    fn test_relock_read_fail_with_wrong_seq() {
        let lock = SixLock::new();
        // 从未获取过锁，seq 为 0
        assert!(!lock.relock_read(42));
    }

    #[test]
    fn test_relock_intent_success() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        let seq = lock.seq();
        lock.unlock_intent();
        // 没有写操作，relock 应该成功
        assert!(lock.relock_intent(seq));
        lock.unlock_intent();
    }

    #[test]
    fn test_relock_intent_fail_after_write() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        let seq = lock.seq();
        lock.unlock_intent();

        // 中间发生写操作
        assert!(lock.try_lock_write());
        lock.unlock_write();

        assert!(!lock.relock_intent(seq));
    }

    #[test]
    fn test_relock_read_fail_when_lock_contended() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.try_lock_read());
        let seq = lock.seq();
        lock.unlock_read();

        // 另一线程获取写锁，导致 seq 变化
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.try_lock_write());
            l.unlock_write();
        });
        h.join().unwrap();

        assert!(!lock.relock_read(seq));
    }

    // ── DeadlockDetector 适配测试 ──

    fn make_waiters(pairs: &[(u64, u64, u64)]) -> Vec<crate::lock::deadlock::WaiterInfo> {
        pairs
            .iter()
            .map(|&(t, l, h)| crate::lock::deadlock::WaiterInfo {
                trans_id: t,
                lock_id: l,
                waiting_for_trans_id: h,
            })
            .collect()
    }

    #[test]
    fn test_detector_complex_cycle() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // 4 个事务形成环：T1→L2→T2→L3→T3→L4→T4→L1→T1
        let waiters = make_waiters(&[(1, 102, 2), (2, 103, 3), (3, 104, 4), (4, 101, 1)]);
        assert!(d.detect(1, 102, &waiters), "should detect 4-way deadlock");
    }

    #[test]
    fn test_detector_multi_cycle() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // 两个独立的死循环
        // Cycle 1: T1→L2→T2→L1→T1
        // Cycle 2: T3→L4→T4→L3→T3
        let waiters = make_waiters(&[(1, 102, 2), (2, 101, 1), (3, 104, 4), (4, 103, 3)]);
        assert!(d.detect(1, 102, &waiters), "first cycle");
        assert!(d.detect(3, 104, &waiters), "second cycle");
    }

    // ── 压力测试 ──

    #[test]
    fn stress_test_read_heavy_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..16 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..500 {
                    loop {
                        if l.try_lock_read() {
                            std::hint::spin_loop();
                            l.unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
        assert!(lock.try_lock_write());
        lock.unlock_write();
    }

    #[test]
    fn stress_test_write_heavy_contention() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    loop {
                        if l.try_lock_write() {
                            std::hint::spin_loop();
                            l.unlock_write();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_mixed_read_write_intent() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];

        // 4 个写线程
        for _ in 0..4 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..30 {
                    loop {
                        if l.try_lock_write() {
                            std::hint::spin_loop();
                            l.unlock_write();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 8 个读线程
        for _ in 0..8 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    loop {
                        if l.try_lock_read() {
                            std::hint::spin_loop();
                            l.unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 2 个 intent 线程
        for _ in 0..2 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..30 {
                    loop {
                        if l.try_lock_intent() {
                            if l.try_upgrade_intent_to_write() {
                                std::hint::spin_loop();
                                l.downgrade_write_to_intent();
                            }
                            l.unlock_intent();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_percpu_heavy_load() {
        let lock = Arc::new(SixLock::with_percpu(16));
        let mut handles = vec![];

        for _ in 0..16 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    loop {
                        if l.try_lock_read() {
                            std::hint::spin_loop();
                            l.unlock_read();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        // 2 个写线程穿插写入
        for _ in 0..2 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..20 {
                    loop {
                        if l.try_lock_write() {
                            std::hint::spin_loop();
                            l.unlock_write();
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(lock.reader_count(), 0);
    }

    #[test]
    fn stress_test_detector_integration() {
        use crate::lock::deadlock::DeadlockDetector;

        let mut d = DeadlockDetector::new();
        // Phase 1: T2→L1→T1 → no deadlock
        let waiters_phase1 = make_waiters(&[(2, 100, 1)]);
        assert!(!d.detect(2, 100, &waiters_phase1), "no cycle yet");
        // Phase 2: T2→L1→T1, T1→L2→T2 → AB-BA deadlock
        let waiters_phase2 = make_waiters(&[(2, 100, 1), (1, 200, 2)]);
        assert!(d.detect(2, 100, &waiters_phase2), "AB-BA deadlock detected");
    }

    // ══════════════════════════════════════════════════════════════════
    // Phase C1: 自旋/等待/通知 测试
    // ══════════════════════════════════════════════════════════════════

    /// S1: spin_read 在写锁释放后成功获取读锁（同线程，spin 适用于微秒级等待）
    #[test]
    fn test_spin_read_succeeds() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        lock.unlock_write();
        assert!(lock.spin_lock_read_internal());
        lock.unlock_read();
    }

    /// S2: spin_write 在只有自身读锁时成功（bcachefs 重入），有其他线程读锁时失败
    #[test]
    fn test_spin_write_fails_if_readers() {
        let lock = SixLock::new();
        // 同线程持有读锁 → spin_lock_write 由于重入语义成功
        assert!(lock.try_lock_read());
        assert!(lock.spin_lock_write_internal()); // 重入：自身读锁可排除
        lock.unlock_write();
        lock.unlock_read();

        // 无读者 → 成功
        assert!(lock.spin_lock_write_internal());
        lock.unlock_write();
    }

    /// S3: 自旋在 SPIN_COUNT 次后超时返回 false（锁被其他线程持续持有）
    #[test]
    fn test_spin_timeout() {
        let lock = Arc::new(SixLock::new());
        assert!(lock.try_lock_write());
        assert!(!lock.spin_lock_read_internal());
        lock.unlock_write();
    }

    /// S4: nospin bit 置位后自旋立即返回 false
    #[test]
    fn test_nospin_skips_spin() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        lock.set_nospin();
        assert!(lock.is_nospin());
        assert!(!lock.spin_lock_read_internal());
        assert!(!lock.spin_lock_intent_internal());
        assert!(!lock.spin_lock_write_internal());
        lock.clear_nospin();
        assert!(!lock.is_nospin());
        lock.unlock_write();
    }

    /// S5: lock_read 阻塞直到写锁释放后成功获取
    #[test]
    fn test_lock_read_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.try_lock_write());
            thread::sleep(std::time::Duration::from_millis(10));
            l.unlock_write();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_read 会在写锁释放后成功获取（阻塞等待）
        assert!(lock.lock_read());
        lock.unlock_read();
        h.join().unwrap();
    }

    /// S6: lock_write 独占（同线程读锁不能和写锁共存）
    #[test]
    fn test_lock_write_exclusive() {
        let lock = SixLock::new();
        assert!(lock.lock_write());
        assert!(!lock.try_lock_read());
        lock.unlock_write();
    }

    /// S7: upgrade_read_to_intent 同线程直接升级
    #[test]
    fn test_upgrade_read_to_intent_same_thread() {
        let lock = SixLock::new();
        assert!(lock.try_lock_read());
        assert!(lock.upgrade_read_to_intent());
        assert!(lock.is_intent_locked_by_current());
        lock.unlock_intent();
    }

    /// S8: upgrade_intent_to_write 同线程直接升级
    #[test]
    fn test_upgrade_intent_to_write_same_thread() {
        let lock = SixLock::new();
        assert!(lock.try_lock_intent());
        assert!(lock.upgrade_intent_to_write());
        assert!(lock.is_write_locked());
        lock.unlock_write();
        assert!(lock.is_intent_locked());
        lock.unlock_intent();
    }

    /// S9: push_waiter_test 直接入队 waiter 到 WaitFifo（无 trylock 重试）
    #[test]
    fn test_waiter_fifo_integration() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        assert!(lock.push_waiter_test(&WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        }));
        assert_eq!(
            lock.wait_fifo.len(),
            1,
            "push_waiter_test should add a waiter"
        );
        // 通过 remove_by_thread 验证 waiter 元数据
        let removed = lock.wait_fifo.remove_by_thread(thread::current().id());
        assert!(removed.is_some(), "waiter should be removable");
        assert_eq!(removed.unwrap().lock_type, SixLockType::Read);
        lock.unlock_write();
    }

    /// S10: 多重 push_waiter_test 累积多个 waiter
    #[test]
    fn test_waiter_fifo_multiple_pushes() {
        let lock = SixLock::new();
        assert!(lock.try_lock_write());
        let waiter = WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        };
        assert!(lock.push_waiter_test(&waiter));
        assert!(lock.push_waiter_test(&waiter));
        let len = lock.wait_fifo.len();
        assert!(len >= 2, "multiple pushes should add waiters (got {})", len);
        lock.unlock_write();
    }

    /// S11: wakeup_lock_type 在有等待者时不 panic
    #[test]
    fn test_wakeup_lock_type_no_panic() {
        let lock = SixLock::new();
        lock.wakeup_lock_type(lock.read_state(), SixLockType::Read);
        lock.wakeup_lock_type(lock.read_state(), SixLockType::Write);
        assert!(lock.try_lock_write());
        // 用 push_waiter_test 添加 waiter 后再 wakeup
        assert!(lock.push_waiter_test(&WaiterBox {
            trans_id: 0,
            lock_type: SixLockType::Read,
            seq: 0,
            thread: Some(thread::current()),
            lock_acquired: false,
            lock_acquired_flag: None,
            percpu_slot: 0,
        }));
        let state = lock.read_state();
        lock.wakeup_lock_type(state, SixLockType::Read); // should not panic
        lock.unlock_write();
        assert!(lock.lock_read());
        lock.unlock_read();
    }

    /// S12: 同线程 lock_write 重入
    #[test]
    fn test_lock_write_reentrant() {
        let lock = SixLock::new();
        assert!(lock.lock_write());
        assert!(lock.lock_write());
        assert!(lock.is_write_locked());
        lock.unlock_write();
        assert!(lock.is_write_locked());
        lock.unlock_write();
        assert!(!lock.is_write_locked());
    }

    /// S13: lock_intent 重入
    #[test]
    fn test_lock_intent_reentrant() {
        let lock = SixLock::new();
        assert!(lock.lock_intent());
        assert!(lock.lock_intent());
        lock.unlock_intent();
        assert!(lock.is_intent_locked());
        lock.unlock_intent();
        assert!(!lock.is_intent_locked());
    }

    /// S14: 写锁持有期间 waiting bit 被设置，释放后 lock_read 可获取
    #[test]
    fn test_waiting_bits_after_lock_release() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 另一个线程持写锁
        let h = thread::spawn(move || {
            assert!(l.try_lock_write());
            // 等待主线程 lock_read 阻塞，此时 waiting bit 应已设置
            thread::sleep(std::time::Duration::from_millis(10));
            let state = l.read_state();
            assert!(
                (state & WAITING_READ_BIT) != 0,
                "waiting_read bit should be set during lock_read contention"
            );
            l.unlock_write();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_read 阻塞直到写锁释放（内部自动 push waiter）
        assert!(lock.lock_read());
        assert_eq!(
            lock.wait_fifo.len(),
            0,
            "fifo should be empty after self-removal"
        );
        lock.unlock_read();
        h.join().unwrap();
    }

    /// S15: lock_write 阻塞直到读锁释放后成功获取
    #[test]
    fn test_lock_write_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 读线程持读锁 50ms
        let h = thread::spawn(move || {
            assert!(l.try_lock_read());
            thread::sleep(std::time::Duration::from_millis(50));
            l.unlock_read();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        // lock_write 应该阻塞直到读者释放
        assert!(lock.lock_write());
        lock.unlock_write();
        h.join().unwrap();
    }

    /// S16: lock_intent 阻塞直到 intent 释放后成功获取
    #[test]
    fn test_lock_intent_blocks_and_succeeds() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        let h = thread::spawn(move || {
            assert!(l.try_lock_intent());
            thread::sleep(std::time::Duration::from_millis(50));
            l.unlock_intent();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.lock_intent());
        lock.unlock_intent();
        h.join().unwrap();
    }

    /// S17: wakeup_lock_type 正确 unpark 等待的读线程
    #[test]
    fn test_wakeup_lock_type_wakes_reader() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 写线程持锁后释放，验证读线程被唤醒
        let h = thread::spawn(move || {
            assert!(l.try_lock_write());
            thread::sleep(std::time::Duration::from_millis(10));
            l.unlock_write();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.lock_read());
        lock.unlock_read();
        h.join().unwrap();
    }

    /// S18: wakeup_lock_type 正确 unpark 等待的写线程
    #[test]
    fn test_wakeup_lock_type_wakes_writer() {
        let lock = Arc::new(SixLock::new());
        let l = lock.clone();
        // 读者持锁后释放，验证写线程被唤醒
        let h = thread::spawn(move || {
            assert!(l.try_lock_read());
            thread::sleep(std::time::Duration::from_millis(10));
            l.unlock_read();
        });
        thread::sleep(std::time::Duration::from_millis(5));
        assert!(lock.lock_write());
        lock.unlock_write();
        h.join().unwrap();
    }

    /// D1: wakeup_lock_type 链式重入死锁检测
    ///
    /// 8 个读线程 + 2 个写线程同时用 blocking lock 路径争用同一把锁。
    /// 验证 wait_lock Mutex 在 wakeup_lock_type→unpark→acquire→unlock→wakeup_lock_type
    /// 链式调用中不会死锁。
    ///
    /// 关键路径：
    /// 1. 写线程 unlock_write → wakeup_lock_type(Read) → wait_lock.lock → snapshot → unlock
    /// 2. 读线程被 unpark → lock_read 成功 → unlock_read → wakeup_lock_type(Write) → wait_lock.lock
    /// 3. 若 wait_lock 在步骤 1 未释放，步骤 2 死锁——但我们先 unlock 再 unpark，所以安全
    #[test]
    fn stress_deadlock_read_write_chain() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(11)); // 10 workers + main

        // 8 个读线程：lock_read → unlock_read 循环
        for _ in 0..8 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait(); // 同步启动
                for _ in 0..100 {
                    assert!(l.lock_read(), "reader should acquire lock");
                    std::hint::spin_loop(); // 模拟短工作
                    l.unlock_read();
                }
            }));
        }

        // 2 个写线程：lock_write → unlock_write 循环
        for _ in 0..2 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..25 {
                    assert!(l.lock_write(), "writer should acquire lock");
                    std::hint::spin_loop();
                    l.unlock_write();
                }
            }));
        }

        ready.wait(); // 所有线程同时开始
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: thread did not finish within 10s");
            }
            h.join().unwrap();
        }
    }

    /// D2: 多写线程阻塞唤醒链死锁检测
    ///
    /// 4 个写线程同时争用写锁。锁只有一个，其他三个必须通过 sleep 路径
    /// park 等待。释放时 wakeup_lock_type 唤醒一个，该线程 unlock 后再次唤醒下一个。
    ///
    /// 验证 write→write 阻塞唤醒链不因 wait_lock 死锁。
    #[test]
    fn stress_deadlock_write_chain() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(5)); // 4 workers + main

        for _ in 0..4 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                for _ in 0..50 {
                    assert!(l.lock_write(), "writer should acquire lock");
                    std::hint::spin_loop();
                    l.unlock_write();
                }
            }));
        }

        ready.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: write chain did not finish within 10s");
            }
            h.join().unwrap();
        }
    }

    /// D3: wakeup_lock_type snapshot/remove 并发压力测试
    ///
    /// 1 个写线程持锁，8 个读线程全部在 fifo 中等待。
    /// 写线程释放时 wakeup_lock_type(Read) 对所有读线程 unpark。
    /// 8 个读线程同时 wake → try_lock_read → remove_self_from_fifo。
    /// 验证 wait_lock 保护下的并发 remove_by_thread 不会死锁或 panic。
    #[test]
    fn stress_deadlock_burst_wake() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        let ready = Arc::new(std::sync::Barrier::new(10)); // 8 readers + 1 writer + main

        // 8 个读线程
        for _ in 0..8 {
            let l = lock.clone();
            let b = ready.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                // lock_read 会阻塞直到写锁释放
                assert!(l.lock_read(), "reader should acquire lock after burst wake");
                // 微延迟避免所有读者同时 release
                thread::sleep(std::time::Duration::from_micros(100));
                l.unlock_read();
            }));
        }

        // 写线程：持锁，释放（触发 burst wake）
        //
        // 写线程先用 lock_write 确保获取锁（与读者 Barrier 同时启动，读者可能抢先）。
        // 获取后释放，触发所有在读等待者的 burst wake。
        let l = lock.clone();
        let b = ready.clone();
        let writer = thread::spawn(move || {
            b.wait();
            for _ in 0..20 {
                // 用 lock_write 阻塞获取（ready Barrier 后读者可能已抢先持锁）
                assert!(l.lock_write(), "writer should acquire lock");
                // 等读者全进 fifo
                thread::sleep(std::time::Duration::from_millis(5));
                l.unlock_write(); // ← burst wake: 所有在读等待者被 unpark
                thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        ready.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for h in handles {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("DEADLOCK DETECTED: burst wake did not finish within 10s");
            }
            h.join().unwrap();
        }
        writer.join().unwrap();
    }

    /// S19: 多个读者同时等待写锁释放后全部获取读锁
    #[test]
    fn test_multiple_readers_block_then_all_succeed() {
        let lock = Arc::new(SixLock::new());
        let mut handles = vec![];
        // 持写锁
        assert!(lock.try_lock_write());
        // 5 个读线程各调用 lock_read（都会阻塞）
        for _ in 0..5 {
            let l = lock.clone();
            handles.push(thread::spawn(move || {
                assert!(l.lock_read());
                l.unlock_read();
            }));
        }
        thread::sleep(std::time::Duration::from_millis(10));
        // 释放写锁 → 所有读线程应被唤醒
        lock.unlock_write();
        for h in handles {
            h.join().unwrap();
        }
    }
}
