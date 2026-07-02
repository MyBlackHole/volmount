//! Journal 类型定义 — Journal 实例 + JournalError
//!
//! Journal 是一组预分配的 bucket（循环缓冲区），
//! 每个 journal entry = Jset（含 btree update keys）。
//! 用作 crash recovery 的主机制。
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────────────────────────────────────────────┐
//!  │  JournalResState (AtomicU64)                         │
//!  │  ┌───────┬──────┬────────┬────────┬────────┬────────┐│
//!  │  │ offset│ idx  │buf0 cnt│buf1 cnt│buf2 cnt│buf3 cnt││
//!  │  │ 22bit │ 2bit │ 10bit  │ 10bit  │ 10bit  │ 10bit  ││
//!  │  └───────┴──────┴────────┴────────┴────────┴────────┘│
//!  │  CAS 循环 → 无锁保留                                    │
//!  └──────────────────────────────────────────────────────┘
//!
//!  buf[0..BUF_NR]   in_flight FIFO       ring[seq & mask]
//!  ┌────────────┐    ┌──────────┐     ┌──────────────────┐
//!  │ Accepting  │───→│ idx=1    │────→│ buf[1] + data    │
//!  │ Closing    │    │ idx=2    │     │ (reservation      │
//!  │ WriteDone  │    │ ...      │     │  fastpath cache)  │
//!  │ Free       │    └──────────┘     └──────────────────┘
//!  └────────────┘
//! ```
//!
//! # Overflow 策略
//!
//! - 每个 buf 容量满时关闭 → 等待所有 reservation 释放 → 写入 bucket
//! - 如果所有 bucket 都已使用 → 返回 `JournalError::Overflow`
//!
//! # bcachefs 对齐
//!
//! | 概念 | bcachefs 文件:行号 |
//! |------|-------------------|
//! | `union journal_res_state` | `fs/journal/types.h:142-174` |
//! | `struct journal_res` | `fs/journal/types.h:134-140` |
//! | `journal_res_get_fast()` | `fs/journal/journal.h:475-518` |
//! | `journal_state_inc()/dec()` | `fs/journal/journal.h` inline |
//! | `JOURNAL_STATE_BUF_NR` | `fs/journal/types.h:20-22` |
//! | `struct journal_buf` | `fs/journal/types.h:37-76` |
//! | `__journal_entry_open_one()` | `fs/journal/journal.c:391` |
//! | `__bch2_journal_buf_put_final()` | `fs/journal/journal.c:240-256` |

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use tokio::sync::Notify;

use serde::{Deserialize, Serialize};

use crate::alloc::{AllocRequest, BchAllocator, BchDataType, DedicatedWp, WritePointSpecifier};
use crate::block_device::BlockDevice;
use crate::btree::key::BtreeEntry;
use crate::btree::BtreeEngine;
use crate::btree::BtreeId;
use crate::types::{BlockAddr, StorageError, Watermark};

use super::jset::{BlacklistEntry, Jset, JsetEntryType, JsetHeader, RawJsetEntry, JSET_BLOCK_SIZE};
use super::reclaim::{JournalEntryPin, JournalEntryPinList, PinListFifo, PIN_FIFO_SIZE};

// ═══════════════════════════════════════════════════════════
// Part 1: Constants
// ═══════════════════════════════════════════════════════════

/// 预分配的 journal bucket 数量
///
/// Wave 1-2 期间 journal bucket 不回收，需足够避免 overflow。
/// 32 buckets × 256 blocks/bucket × 4KB/block = 32MB 元数据空间，
/// 每个 Jset ~4KB，约 8000 次事务写满。保守安全。
pub const DEFAULT_JOURNAL_BUCKETS: u32 = 32;

/// 每个 journal bucket 的 block 数（256 blocks = 1MB）
pub const BUCKET_BLOCKS: u32 = 256;

/// Overflow 警戒线 bytes
///
/// 当当前 bucket 剩余空间小于此值时触发 bucket 轮换。
/// 设为 JSET_BLOCK_SIZE（一个 block），因为每个 Jset 写入至少需要一个 block。
#[allow(dead_code)]
pub const OVERFLOW_MARGIN: u32 = JSET_BLOCK_SIZE;

// ─── Multi-buffer config ───

/// Journal buffer count (bcachefs JOURNAL_STATE_BUF_NR)
pub const JOURNAL_STATE_BUF_NR: usize = 4;
#[allow(dead_code)]
pub const JOURNAL_STATE_BUF_MASK: usize = JOURNAL_STATE_BUF_NR - 1;

/// Per-buffer staging area size (32KB = 4096 u64s)
pub const BUF_SIZE: usize = 32768;
pub const BUF_SIZE_U64S: u32 = (BUF_SIZE / 8) as u32; // 4096

// ─── Bit layout constants (bcachefs journal_res_state) ───

/// Bit layout of JournalResState:
///   [0..22)  cur_entry_offset — reserved u64s in current entry
///   [22..24) idx — current open journal buffer index
///   [24..34) buf0_count
///   [34..44) buf1_count
///   [44..54) buf2_count
///   [54..64) buf3_count
const CUR_ENTRY_OFFSET_BITS: u64 = 22;
const CUR_ENTRY_OFFSET_MASK: u64 = (1 << CUR_ENTRY_OFFSET_BITS) - 1;
const IDX_BITS: u64 = 2;
const IDX_SHIFT: u64 = CUR_ENTRY_OFFSET_BITS;
const IDX_MASK: u64 = (1 << IDX_BITS) - 1;
const BUF_COUNT_BITS: u64 = 10;
const BUF_COUNT_MAX: u64 = (1 << BUF_COUNT_BITS) - 1;
const BUF0_COUNT_SHIFT: u64 = IDX_SHIFT + IDX_BITS;

/// Sentinel values for cur_entry_offset (bcachefs JOURNAL_ENTRY_CLOSED_VAL etc.)
/// CLOSED_VAL = 0x3FFFFF - 1 = 4194302
pub const JOURNAL_ENTRY_CLOSED_VAL: u64 = CUR_ENTRY_OFFSET_MASK - 1;

/// Journal needs flush write flag — 标记 journal 有数据需要写入后端存储。
/// 对应 bcachefs `JOURNAL_NEEDS_FLUSH_WRITE` (journal.h)。
pub const JOURNAL_NEEDS_FLUSH_WRITE: u64 = 1 << 0;

// ═══════════════════════════════════════════════════════════
// Part 2: Error
// ═══════════════════════════════════════════════════════════

/// Journal 错误码（用于 journal_error AtomicU8）
pub const JE_NONE: u8 = 0;
pub const JE_OVERFLOW: u8 = 1;
pub const JE_CHECKSUM: u8 = 2;
pub const JE_IO: u8 = 3;
pub const JE_STUCK: u8 = 4;
pub const JE_FULL: u8 = 5;
pub const JE_PIN_FULL: u8 = 6;
pub const JE_BLOCKED: u8 = 7;

/// Journal 错误
#[derive(Debug)]
pub enum JournalError {
    /// Journal 写满（所有 bucket 已用尽且未回收）
    Overflow(String),
    /// CRC32 校验不匹配
    ChecksumMismatch,
    /// 底层存储 I/O 错误
    Io(StorageError),
    /// Journal reclaim 被卡住（pin 无法推进）
    Stuck(String),
    /// Journal 已满（空间不足，等待 reclaim）
    Full(String),
    /// Pin FIFO 已满（bcachefs `journal_pin_full`）
    PinFull(String),
    /// Journal 被阻塞（bcachefs `journal_blocked`）
    Blocked(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Overflow(msg) => write!(f, "journal overflow: {}", msg),
            JournalError::ChecksumMismatch => write!(f, "journal checksum mismatch"),
            JournalError::Io(e) => write!(f, "journal io error: {}", e),
            JournalError::Stuck(msg) => write!(f, "journal stuck: {}", msg),
            JournalError::Full(msg) => write!(f, "journal full: {}", msg),
            JournalError::PinFull(msg) => write!(f, "journal pin full: {}", msg),
            JournalError::Blocked(msg) => write!(f, "journal blocked: {}", msg),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<StorageError> for JournalError {
    fn from(e: StorageError) -> Self {
        JournalError::Io(e)
    }
}

/// 检测 journal 系统是否处于卡住状态（特定 seq 长时间未推进）。
///
/// 对应 bcachefs `bch2_journal_error_check_stuck()` 的简化版。
/// 如果 `last_seq_ondisk` 在给定超时时间内未推进超过 `threshold` 个 seq，
/// 返回 `JournalError::Stuck`。
pub fn journal_error_check_stuck(
    journal: &Journal,
    last_check_seq: &mut u64,
    last_check_time: &mut std::time::Instant,
    threshold: u64,
    timeout: std::time::Duration,
) -> Result<(), JournalError> {
    let cur_ondisk = journal
        .last_seq_ondisk
        .load(std::sync::atomic::Ordering::Acquire);
    let now = std::time::Instant::now();

    if cur_ondisk > *last_check_seq + threshold {
        // 有正常推进，重置计时
        *last_check_seq = cur_ondisk;
        *last_check_time = now;
        return Ok(());
    }

    if now.duration_since(*last_check_time) > timeout {
        return Err(JournalError::Stuck(format!(
            "last_seq_ondisk={} not advanced past {} for {:?}",
            cur_ondisk, last_check_seq, timeout,
        )));
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════
// Part 3: Bucket state (unchanged)
// ═══════════════════════════════════════════════════════════

/// bcachefs 对齐的 journal bucket 元数据（对应 bcachefs `journal_device`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalDevice {
    /// bucket 起始 block addr
    pub addr: u64,
    /// 该 bucket 中最大的 journal seq（用于回收判定）
    pub max_seq: u64,
    /// 是否包含未 flush 的条目
    pub dirty: bool,
}

/// Journal 状态快照 — Superblock 序列化用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalSuperblockState {
    /// 当前分配的 bucket 地址列表
    pub bucket_addrs: Vec<u64>,
    /// 最新分配的 seq
    pub last_seq: u64,
    /// 已落盘的最大 seq
    pub last_seq_ondisk: u64,
    /// 当前 bucket 索引
    pub last_bucket: u32,
    /// discard 索引
    pub discard_idx: u32,
    /// dirty 索引（内存中最旧脏 bucket）
    pub dirty_idx: u32,
    /// dirty ondisk 索引（已落盘的最旧脏 bucket）
    pub dirty_idx_ondisk: u32,
    /// 每个 bucket 的 max seq（用于回收）
    pub bucket_seq: Vec<u64>,
    /// 已回放的 seq（JournalReplayer 幂等用）
    pub replayed_seqs: Vec<u64>,
}

// ═══════════════════════════════════════════════════════════
// Part 4: New types — atomic reservation + multi-buffer
// ═══════════════════════════════════════════════════════════

/// Per-buffer state machine
///
/// 对应 bcachefs buf state：Free → Accepting → Closing → {Noflush →} WriteSubmitted → WriteDone
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufState {
    /// 可复用
    Free,
    /// 正在接收保留
    Accepting,
    /// 关闭中（不再接收新保留，等待 refcount 归零）
    Closing,
    /// noflush 后缀路径：buf 已关闭但延迟 flush（对应 bcachefs noflush 语义）
    Noflush,
    /// 已提交写入
    WriteSubmitted,
    /// 写入完成，等待回收
    WriteDone,
}

/// Journal buffer（对应 bcachefs `struct journal_buf`，types.h:37-76）
pub struct JournalBuf {
    /// 当前状态
    pub state: BufState,
    /// 缓冲区数据
    pub data: Vec<u8>,
    /// 此 buf 的起始 seq
    pub seq: u64,
    /// buf 中实际数据的字节数（BUF_SIZE 以下的已使用长度）
    pub data_end: usize,
    /// 写入完成通知
    pub notify: Arc<Notify>,
    /// 此 buf 中是否包含必须立即 flush 的 reservation
    pub has_must_flush: bool,
    /// Noflush 标志 — 若设置，写入时不需要 REQ_FUA/preflush。
    /// 对应 bcachefs `BUF_JOURNAL_NOFLUSH` (journal.h:181)。
    pub no_flush: bool,
    /// P2-6: 写入完成回调队列（在 buf → WriteDone 时触发）
    pub write_done_callbacks: Vec<Option<Box<dyn FnOnce() + Send>>>,
}

impl JournalBuf {
    fn free() -> Self {
        Self {
            state: BufState::Free,
            data: Vec::new(),
            seq: 0,
            data_end: 0,
            notify: Arc::new(Notify::new()),
            has_must_flush: false,
            no_flush: false,
            write_done_callbacks: Vec::new(),
        }
    }

    /// Reset buf for reuse as the accepting buf
    fn reset_for_accepting(&mut self, new_seq: u64) {
        self.data.resize(BUF_SIZE, 0);
        self.data.fill(0);
        self.seq = new_seq;
        self.data_end = 0;
        self.state = BufState::Accepting;
        self.has_must_flush = false;
        self.no_flush = false;
        self.write_done_callbacks.clear();
    }

    /// 尝试标记 buf 为 noflush（跳过 FUA/preflush）。
    ///
    /// 对应 bcachefs `journal_buf_try_noflush()` (journal.h:191-203)。
    /// 如果 buf 有等待者（notify 上有 waiters），不能标记 noflush，
    /// 因为等待者依赖写入完成后被唤醒。
    ///
    /// 调用时机：在 `should_flush` 判定中，若 buf 应 flush 但无等待者，
    /// 可以标记 noflush 来优化写性能（跳过 REQ_FUA 等待 fsync 周期自然落盘）。
    ///
    /// 返回 true 表示成功标记为 noflush。
    pub fn journal_buf_try_noflush(&self) -> bool {
        // 已在 no_flush 状态 → 返回 true
        if self.no_flush {
            return true;
        }
        // 检查是否有等待者（notify 上注册了 notified future 表示有人在等待）
        // 注意：tokio::Notify 不提供 poll_count()，无法直接检查等待者数量。
        // 这里使用保守策略：直接返回 false（不走 noflush 优化）。
        // 后续可引入 AtomicBool has_waiters 字段来跟踪。
        //
        // bcachefs 通过检查 buf->wait.list.first 来判断是否有等待者：
        // - first == NOFLUSH (1): 已标记 noflush
        // - first == NULL: 无等待者 → CAS 设置 NOFLUSH
        // - first 为正常指针: 有等待者 → 不可 noflush
        false
    }
}

/// Journal 保留结果（对应 bcachefs `struct journal_res`，types.h:134-140）
///
/// uninit → reserved → committed/freed
///
/// # Seq 设计
///
/// - `seq`: per-reservation 序列号（向后兼容，当前与 entry_seq 相同）
/// - `entry_seq`: entry 级别 seq（同 entry 内所有 reservation 共享）
///
/// 在 bcachefs 对齐中，seq 按 entry 分配而非按 reservation。
/// 当前实现中 `seq` 与 `entry_seq` 值相同，保持 `seq` 字段以供旧代码编译。
pub struct JournalRes {
    /// Journal sequence number（向后兼容，与 entry_seq 相同）
    pub seq: u64,
    /// entry 级别 seq — 同 entry 内所有 reservation 共享此值
    pub entry_seq: u64,
    /// 在 buf.data 中的偏移（字节）
    pub offset: u32,
    /// 保留的 u64 数
    pub u64s: u32,
    /// 目标 journal buffer 索引
    pub buf_idx: u32,
    /// 此 reservation 是否需要立即 flush 到后端存储（保证持久化）
    pub must_flush: bool,
}

/// 64-bit 原子保留状态（对应 bcachefs `union journal_res_state`，types.h:142-174）
///
/// 位域布局（与 bcachefs 一致）：
///   [0..22)  cur_entry_offset — 当前 entry 中已保留的 u64 数
///   [22..24) idx — 当前开放的 journal buffer 索引
///   [24..34) buf0_count — buf[0] 保留计数
///   [34..44) buf1_count
///   [44..54) buf2_count
///   [54..64) buf3_count
///
/// 整个 fastpath 只需要一条 `atomic64_cmpxchg`。
pub struct JournalResState {
    bits: AtomicU64,
}

impl JournalResState {
    /// 初始化为 CLOSED_VAL（对应 bcachefs `union journal_res_state old = { .v = JOURNAL_ENTRY_CLOSED_VAL }`）。
    /// 这意味着初始状态下没有打开的 entry，`is_journal_entry_open()` 返回 false，
    /// `try_reserve()` 因 `cur_entry_offset` 为 sentinel 值而返回 None。
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(JOURNAL_ENTRY_CLOSED_VAL),
        }
    }

    /// 原子读取完整 state（对应 bcachefs `smp_load_acquire(&j->reservations.v)`）
    pub fn read(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }

    /// 提取 cur_entry_offset（单位 u64）
    /// 对应 bcachefs `union journal_res_state` 的 `cur_entry_offset` 位字段
    pub fn cur_entry_offset(v: u64) -> u32 {
        (v & CUR_ENTRY_OFFSET_MASK) as u32
    }

    /// 提取 idx（当前 Accepting buf 索引）
    /// 对应 bcachefs `union journal_res_state` 的 `idx` 位字段
    pub fn idx(v: u64) -> u32 {
        ((v >> IDX_SHIFT) & IDX_MASK) as u32
    }

    /// 获取指定 buf 的 refcount（对应 bcachefs `journal_state_count()` journal.h:243）
    pub fn buf_count(v: u64, idx: u32) -> u32 {
        let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
        ((v >> shift) & BUF_COUNT_MAX) as u32
    }

    /// Try to reserve `req_u64s` in current entry (CAS loop).
    ///
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:475-518) 的核心 CAS。
    ///
    /// 返回 `(old_state, new_state)` on success, `None` on failure (need slowpath).
    pub fn try_reserve(&self, req_u64s: u32) -> Option<(u64, u64)> {
        let mut old = self.bits.load(Ordering::Relaxed);
        loop {
            let cur_off = Self::cur_entry_offset(old);
            let idx = Self::idx(old);

            // 检查是否有足够空间（bcachefs journal.h:491）
            if (cur_off as u64).wrapping_add(req_u64s as u64) > BUF_SIZE_U64S as u64 {
                return None;
            }

            // 检查 refcount 溢出（bcachefs journal.h:505）
            let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
            let count = (old >> shift) & BUF_COUNT_MAX;
            if count == BUF_COUNT_MAX {
                return None;
            }

            let mut new = old;
            // 推进 cur_entry_offset（bcachefs journal.h:499）
            new = (new & !CUR_ENTRY_OFFSET_MASK)
                | ((cur_off as u64).wrapping_add(req_u64s as u64) & CUR_ENTRY_OFFSET_MASK);
            // 递增 buf refcount（bcachefs journal_state_inc）
            new = (new & !(BUF_COUNT_MAX << shift)) | ((count + 1) & BUF_COUNT_MAX) << shift;

            match self
                .bits
                .compare_exchange_weak(old, new, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Some((old, new)),
                Err(updated) => old = updated,
            }
        }
    }

    /// Release a reservation: decrement refcount for buf idx.
    ///
    /// 对应 bcachefs `bch2_journal_buf_put()` (journal.h:395-403) 的 atomic_sub。
    /// 返回 decrement 前的 state 值，调用者可检查 refcount 是否归零。
    pub fn release(&self, idx: u32) -> u64 {
        let shift = BUF0_COUNT_SHIFT + (idx as u64) * BUF_COUNT_BITS;
        self.bits.fetch_sub(1 << shift, Ordering::Release)
    }

    /// Close current entry: set cur_entry_offset to CLOSED_VAL.
    ///
    /// 对应 bcachefs `__journal_entry_close_one()` (journal.c:276) 的 CAS close。
    /// bcachefs 通过 `new.cur_entry_offset = closed_val` (journal.c:293) 设置，
    /// 其中 closed_val = JOURNAL_ENTRY_CLOSED_VAL = CUR_ENTRY_OFFSET_MASK - 1。
    ///
    /// 返回 CAS 成功前捕获的 `cur_entry_offset`（单位 u64），
    /// 调用方可用此值设置 `buf.data_end`。
    /// 见 J2 flush data race 修复：先 close_entry（原子捕获 offset + 阻止新 reservation），
    /// 再 drain refcount，最后设 data_end，防止截断并发写入数据。
    fn close_entry(&self) -> u32 {
        loop {
            let old = self.bits.load(Ordering::Relaxed);
            let captured_offset = Self::cur_entry_offset(old);
            // Clear cur_entry_offset field and set to JOURNAL_ENTRY_CLOSED_VAL
            let new = (old & !CUR_ENTRY_OFFSET_MASK) | JOURNAL_ENTRY_CLOSED_VAL;
            if self
                .bits
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return captured_offset;
            }
        }
    }

    /// Open new entry: set idx to `new_idx`, clear cur_entry_offset and buf_count.
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:549-564) 的 CAS open。
    fn open_entry(&self, new_idx: u32) {
        debug_assert!(new_idx < 4);
        loop {
            let old = self.bits.load(Ordering::Relaxed);
            let mut new = old;
            // Clear idx field
            new &= !(IDX_MASK << IDX_SHIFT);
            // Set new idx
            new |= (new_idx as u64) << IDX_SHIFT;
            // Clear cur_entry_offset
            new &= !CUR_ENTRY_OFFSET_MASK;
            // Clear buf count for new_idx
            let shift = BUF0_COUNT_SHIFT + (new_idx as u64) * BUF_COUNT_BITS;
            new &= !(BUF_COUNT_MAX << shift);
            if self
                .bits
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Align idx to match `(seq - 1) & BUF_MASK` (bcachefs invariant).
    ///
    /// 在 bcachefs 中，`reservations.idx ≡ (j->seq - 1) & BUF_MASK`（因为 Rust 使用
    /// `fetch_add` 而 bcachefs 使用 `inc_return`，seq 语义偏移了 1）。
    /// 需要在 `from_superblock` 恢复后、第一次 `journal_entry_open` 前调用。
    fn align_idx_to_seq(&self, seq: u64) {
        let desired = seq.wrapping_sub(1) & (JOURNAL_STATE_BUF_NR as u64 - 1);
        let old = self.bits.load(Ordering::Relaxed);
        let new = (old & !(IDX_MASK << IDX_SHIFT)) | (desired << IDX_SHIFT);
        self.bits.store(new, Ordering::Release);
    }

    /// Check if current entry is closed
    #[allow(dead_code)]
    fn is_closed(&self) -> bool {
        Self::cur_entry_offset(self.bits.load(Ordering::Relaxed)) as u64 >= JOURNAL_ENTRY_CLOSED_VAL
    }
}

/// Wrapper around UnsafeCell for the journal buf array.
///
/// # Safety
///
/// Sync is safe because:
/// - `commit()` writes to non-overlapping regions (each reservation has a unique offset
///   guaranteed by CAS on `JournalResState`)
/// - State transitions (Free → Accepting → Closing → ...) are single-threaded
///   (happen under the journal lock or slowpath serialization)
/// - After a buf is closed, no new reservations target it (refcount drain is the
///   last access path)
struct BufArray {
    bufs: UnsafeCell<[JournalBuf; JOURNAL_STATE_BUF_NR]>,
}

unsafe impl Sync for BufArray {}
unsafe impl Send for BufArray {}

impl BufArray {
    fn new() -> Self {
        Self {
            bufs: UnsafeCell::new(std::array::from_fn(|_| JournalBuf::free())),
        }
    }

    /// Get immutable reference to buf at index
    #[allow(dead_code)]
    fn get(&self, idx: usize) -> &JournalBuf {
        unsafe { &(*self.bufs.get())[idx] }
    }

    /// Get mutable reference to buf at index (caller guarantees no aliasing violations)
    #[allow(clippy::mut_from_ref)]
    fn get_mut(&self, idx: usize) -> &mut JournalBuf {
        unsafe { &mut (*self.bufs.get())[idx] }
    }

    /// Get mutable reference to all bufs (for bucket flush which accesses sequentially)
    #[allow(dead_code, clippy::mut_from_ref)]
    fn get_all_mut(&self) -> &mut [JournalBuf; JOURNAL_STATE_BUF_NR] {
        unsafe { &mut *self.bufs.get() }
    }
}

// ═══════════════════════════════════════════════════════════
// Part 5: Pin FIFO — per-seq btree reference tracking
// ═══════════════════════════════════════════════════════════

/// 最大 pin 条目数（固定预分配数组的大小）
///
/// 对应 bcachefs `JOURNAL_PIN_LIST_SIZE` 的概念。
/// 128 = 最多 128 个在途 journal entry，足以覆盖 4 buffer × 32 bucket 的并发。
pub use super::reclaim::PIN_FIFO_SIZE as MAX_PIN_ENTRIES;

// JournalEntryPin 和 PinFifo 定义已移至 reclaim.rs。
// 导入自: use super::reclaim::{JournalEntryPin, JournalEntryPinList, PinListFifo};

// ═══════════════════════════════════════════════════════════
// Part 5b: JournalSpace — slowpath space tracking
// ═══════════════════════════════════════════════════════════

/// Journal space category — 对应 bcachefs journal_space 数组中不同等级的可回收空间
///
/// 四个索引按"可回收程度"排序：
///   DISCARDED(0):  已 discard 的 bucket（最安全，完全自由）
///   CLEAN_ONDISK(1): 已落盘且 clean 的 bucket
///   CLEAN(2):      内存中 clean 的 bucket
///   TOTAL(3):      全部 journal bucket（包含当前正在写入的）
#[derive(Debug, Clone, Copy)]
pub struct JournalSpace {
    /// 该类别总字节数
    pub total: u64,
    /// 该类别可用字节数
    pub available: u64,
}

impl JournalSpace {
    pub const fn new() -> Self {
        Self {
            total: 0,
            available: 0,
        }
    }
}

impl Default for JournalSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// JournalSpace 数组索引常量
pub const JOURNAL_SPACE_DISCARDED: usize = 0;
pub const JOURNAL_SPACE_CLEAN_ONDISK: usize = 1;
pub const JOURNAL_SPACE_CLEAN: usize = 2;
pub const JOURNAL_SPACE_TOTAL: usize = 3;
pub const JOURNAL_SPACE_NR: usize = 4;

/// Journal slowpath 状态下所有 bucket 管理字段
///
/// 被 `Journal.slowpath: Mutex<JournalSlowpath>` 保护。
/// 通过 `slowpath_lock` 序列化所有慢路径操作。
#[derive(Debug)]
pub(crate) struct JournalSlowpath {
    /// journal bucket 列表
    pub buckets: Vec<JournalDevice>,
    /// 每个 bucket 的 max seq（同 bcachefs ja->bucket_seq[]）
    pub bucket_seq: Vec<u64>,
    /// 当前写入的 bucket 索引
    pub current_bucket: usize,
    /// 当前 bucket 内的偏移（字节）
    pub current_offset: u32,
    /// 当前 bucket 还剩多少可用字节
    pub remaining_bytes: u32,
    /// 下一个可丢弃的 bucket 索引 (模 nr)
    /// 四索引不变式: discard_idx ≤ dirty_idx_ondisk ≤ dirty_idx ≤ cur_idx
    pub discard_idx: usize,
    /// 内存中最旧的 dirty bucket
    pub dirty_idx: usize,
    /// 确认落盘的最旧 dirty bucket
    pub dirty_idx_ondisk: usize,
}

impl JournalSlowpath {
    pub fn new(bucket_addrs: Vec<u64>) -> Self {
        let nr = bucket_addrs.len();
        Self {
            bucket_seq: vec![0; nr],
            buckets: bucket_addrs
                .into_iter()
                .map(|addr| JournalDevice {
                    addr,
                    max_seq: 0,
                    dirty: false,
                })
                .collect(),
            current_bucket: 0,
            current_offset: 0,
            remaining_bytes: BUCKET_BLOCKS * JSET_BLOCK_SIZE,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
        }
    }

    pub fn from_superblock(state: &JournalSuperblockState) -> Self {
        let nr = state.bucket_addrs.len();
        let bucket_seq = if state.bucket_seq.len() == nr {
            state.bucket_seq.clone()
        } else {
            vec![0; nr]
        };
        let bucket_idx = (state.last_bucket as usize).min(nr.saturating_sub(1));
        Self {
            bucket_seq,
            buckets: state
                .bucket_addrs
                .iter()
                .map(|addr| JournalDevice {
                    addr: *addr,
                    max_seq: 0,
                    dirty: false,
                })
                .collect(),
            current_bucket: bucket_idx,
            current_offset: 0,
            remaining_bytes: BUCKET_BLOCKS * JSET_BLOCK_SIZE,
            discard_idx: state.discard_idx as usize,
            dirty_idx: state.dirty_idx as usize,
            dirty_idx_ondisk: state.dirty_idx_ondisk as usize,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Part 6: Journal — main struct
// ═══════════════════════════════════════════════════════════

/// Journal 实例结构
///
/// 管理 journal bucket 的写入状态和 seq 分配。
/// 不直接依赖 Volume 或 BtreeEngine。
///
/// # 并发模型
///
/// - **Fastpath** (`journal_res_get_fast`, `journal_res_put`, `commit`):
///   接受 `&self`，无锁原子操作
/// - **Slowpath** (`journal_cycle_locked`, `journal_res_get_slowpath`):
///   通过 `slowpath_lock` 序列化，使用 `slowpath: Mutex<JournalSlowpath>` 保护 bucket 状态
/// - **Full flush/reclaim** (`flush`, `reclaim`, `rotate_or_reclaim`):
///   接受 `&mut self`，管理 bucket 和 backend I/O
pub struct Journal {
    // ★ 原子保留 + 多 buffer（fastpath，无锁）
    /// 原子保留状态（无锁 fastpath，bcachefs `union journal_res_state`）
    reservations: JournalResState,
    /// 多 buffer 数组（UnsafeCell 包装，commit 时写非重叠区域）
    bufs: BufArray,
    /// 无锁 seq 分配（bcachefs `atomic64_t seq`）
    seq: AtomicU64,
    /// 在途 buf 索引队列
    in_flight: Mutex<VecDeque<u32>>,

    // ★ Bucket 管理状态（由 slowpath_lock 或 &mut self 保护）
    slowpath: Mutex<JournalSlowpath>,

    // ★ Per-seq pin FIFO（bcachefs reclaim 侧的对齐实现）
    /// 最老的未 flush seq（用于回收判定）。AtomicU64 — 在 pin_put(&self) 中更新。
    pub last_seq_ondisk: AtomicU64,
    /// Per-seq pin FIFO：追踪 btree node 对 journal reservation seq 的引用。
    /// 使用 PinListFifo（128-slot 预分配）替代旧 PinFifo。
    /// 对应 bcachefs `struct journal` 的 `pin_list[6]`（按 pin type 分离）。
    pub(crate) pin_fifo: UnsafeCell<PinListFifo>,
    /// flush 当前处理中的 pin（retry loop 保护）。
    /// 对应 bcachefs `journal->flush_in_progress`。
    pub(crate) flush_in_progress: AtomicU64,
    /// flush 期间 pin_drop 标记（防止 UAF）。
    /// 对应 bcachefs `journal->flush_in_progress_dropped`。
    pub(crate) flush_in_progress_dropped: AtomicBool,
    /// flush 等待条件变量（pin_flush 等待 flush_in_progress 变化）。
    /// 对应 bcachefs `journal->pin_flush_wait`。
    pub(crate) pin_flush_wait: Arc<Condvar>,
    /// pin_flush_wait 的互斥锁（Condvar::wait 需要 MutexGuard）。
    pub(crate) pin_flush_lock: Mutex<()>,
    /// 最大已落盘 seq（flush 完成后更新）。
    flushed_seq_marker: AtomicU64,

    // ★ Watermark 水位线系统
    /// 当前 journal 水位线（利用率越高值越大，阻止低优先级操作）
    current_watermark: AtomicU8,

    /// Journal 错误状态（0=无错误，非零=对应 JournalErrorCode 编码）。
    /// 一旦设置后不可清除，后续所有 `journal_res_get` 返回错误。
    /// 对应 bcachefs `journal->res->error`（atomic_t）。
    journal_error: AtomicU8,

    // ★ 新增：slowpath 状态机 + 空间追踪 + 自动 flush
    /// slowpath 序列化锁（`journal_res_get` 从 &self 进入 slowpath 时使用）
    slowpath_lock: Mutex<()>,
    /// reclaim 互斥锁 — 串行化整个回收流程，防止并发 flush/reclaim 竞争。
    /// 对应 bcachefs `journal->reclaim_lock` (reclaim.c:1073)。
    pub(crate) reclaim_lock: Mutex<()>,
    /// reclaim_kicked 标志 — 后台线程即时唤醒机制。
    /// 设置此标志后通知后台线程立即执行回收循环，无需等待间隔超时。
    /// 对应 bcachefs `journal->reclaim_kicked` (reclaim.h:14)。
    pub(crate) reclaim_kicked: AtomicBool,
    /// 前台 reclaim 总 flush 计数（direct pass 中 flush 的 pin 数）。
    /// 对应 bcachefs `journal->nr_direct_reclaim` (types.h:396)。
    pub(crate) nr_direct_reclaim: AtomicU64,
    /// 后台 reclaim 总 flush 计数（background pass 中 flush 的 pin 数）。
    /// 对应 bcachefs `journal->nr_background_reclaim` (types.h:397)。
    pub(crate) nr_background_reclaim: AtomicU64,
    /// 4 级空间追踪（discarded / clean_ondisk / clean / total）
    space: [JournalSpace; JOURNAL_SPACE_NR],
    /// 自动 flush 间隔（None = 禁用）
    auto_flush_ms: Option<u64>,
    /// 后台回收间隔（毫秒，0=禁用）。由 spawn_background_reclaim_task 设置。
    reclaim_interval_ms: AtomicU64,

    // ★ P2-7: flush write flag + jiffies 追踪
    /// Journal needs flush write flag — 是否有数据需要写入（对应 bcachefs `JOURNAL_NEEDS_FLUSH_WRITE`）
    needs_flush_write: AtomicBool,
    /// 上次 flush 时的 jiffies 时间戳（用于 flush 频率控制）
    last_flush_jiffies: AtomicU64,
}

impl std::fmt::Debug for JournalEntryPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalEntryPin")
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("pin_type", &self.pin_type)
            .field("flush", &self.flush.lock().is_some())
            .finish()
    }
}

// Journal is Sync: all fields are Sync-safe
// - BufArray: has manual Sync impl (see safety comment above)
// - JournalResState: contains AtomicU64
// - AtomicU64: Sync
// - Mutex<VecDeque<u32>>: Sync
// - PinListFifo: contains [Option<JournalEntryPinList>; 128] + usize; all Sync
// - flushed_seq_marker: AtomicU64 → Sync
// - Mutex<JournalSlowpath>: Sync (Mutex is Sync, JournalSlowpath fields are Send+Sync)
// - Mutex<()>: Sync
// - [JournalSpace; 4]: Sync (all u64 fields)
// - Option<u64>: Sync
// - AtomicU8: Sync
// - Arc<Condvar>: Sync
// - Mutex<()> (reclaim_lock): Sync
// - AtomicBool (reclaim_kicked): Sync
// Journal remains Sync
unsafe impl Sync for Journal {}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let last_seq_ondisk = self.last_seq_ondisk.load(Ordering::Acquire);
        let flushed_seq_marker = self.flushed_seq_marker.load(Ordering::Acquire);
        let pin_fifo_len = unsafe { (*self.pin_fifo.get()).len() };
        let sp = self.slowpath.lock().unwrap();
        f.debug_struct("Journal")
            .field("bucket_count", &sp.buckets.len())
            .field("current_bucket", &sp.current_bucket)
            .field("current_offset", &sp.current_offset)
            .field("remaining_bytes", &sp.remaining_bytes)
            .field("cur_seq", &self.journal_cur_seq())
            .field("last_seq_ondisk", &last_seq_ondisk)
            .field("flushed_seq_marker", &flushed_seq_marker)
            .field("pin_fifo_len", &pin_fifo_len)
            .field("discard_idx", &sp.discard_idx)
            .field("dirty_idx", &sp.dirty_idx)
            .field("dirty_idx_ondisk", &sp.dirty_idx_ondisk)
            .finish()
    }
}

impl Journal {
    // ─── 构造函数 ────────────────────────────────────────────

    /// 创建新 Journal（预分配 bucket 地址，主要用于测试）
    /// 对应 bcachefs `bch2_fs_journal_alloc()` (init.c:305)
    pub fn new(bucket_addrs: Vec<u64>) -> Self {
        let journal = Self {
            reservations: JournalResState::new(),
            bufs: BufArray::new(),
            seq: AtomicU64::new(1),
            in_flight: Mutex::new(VecDeque::new()),
            slowpath: Mutex::new(JournalSlowpath::new(bucket_addrs)),
            last_seq_ondisk: AtomicU64::new(0),
            pin_fifo: UnsafeCell::new(PinListFifo::new()),
            flush_in_progress: AtomicU64::new(0),
            flush_in_progress_dropped: AtomicBool::new(false),
            pin_flush_wait: Arc::new(Condvar::new()),
            pin_flush_lock: Mutex::new(()),
            flushed_seq_marker: AtomicU64::new(0),
            current_watermark: AtomicU8::new(0),
            journal_error: AtomicU8::new(JE_NONE),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            reclaim_kicked: AtomicBool::new(false),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            auto_flush_ms: None,
            reclaim_interval_ms: AtomicU64::new(0),
            needs_flush_write: AtomicBool::new(false),
            last_flush_jiffies: AtomicU64::new(0),
        };
        // Open the first journal entry so buf[0] is immediately accepting
        journal.journal_entry_open();
        journal
    }

    /// 从 BchAllocator 动态分配 N 个 bucket（生产构造函数）
    /// 对应 bcachefs `bch2_fs_journal_alloc()` + `bch2_dev_journal_alloc()` (init.c:305/263)
    pub fn create(
        allocator: &BchAllocator,
        engine: &mut BtreeEngine,
        bucket_count: u32,
    ) -> Result<Self, JournalError> {
        let addrs = allocator
            .bch2_alloc_buckets(
                bucket_count,
                engine,
                &AllocRequest::new(Watermark::Normal, BchDataType::Reserved),
                Some(WritePointSpecifier::Direct(DedicatedWp::Journal)),
            )
            .map_err(JournalError::Io)?;
        Ok(Self::new(addrs))
    }

    /// 从 Superblock 状态恢复 Journal
    /// 对应 bcachefs `bch2_fs_journal_init()` (init.c:802) + `bch2_fs_journal_init_rw()` (init.c:758)
    pub fn from_superblock(state: &JournalSuperblockState) -> Self {
        let journal = Self {
            reservations: JournalResState::new(),
            bufs: BufArray::new(),
            seq: AtomicU64::new(state.last_seq),
            in_flight: Mutex::new(VecDeque::new()),
            slowpath: Mutex::new(JournalSlowpath::from_superblock(state)),
            last_seq_ondisk: AtomicU64::new(state.last_seq_ondisk),
            pin_fifo: UnsafeCell::new(PinListFifo::new()),
            flush_in_progress: AtomicU64::new(0),
            flush_in_progress_dropped: AtomicBool::new(false),
            pin_flush_wait: Arc::new(Condvar::new()),
            pin_flush_lock: Mutex::new(()),
            flushed_seq_marker: AtomicU64::new(0),
            current_watermark: AtomicU8::new(0),
            journal_error: AtomicU8::new(JE_NONE),
            slowpath_lock: Mutex::new(()),
            reclaim_lock: Mutex::new(()),
            reclaim_kicked: AtomicBool::new(false),
            nr_direct_reclaim: AtomicU64::new(0),
            nr_background_reclaim: AtomicU64::new(0),
            space: [JournalSpace::new(); JOURNAL_SPACE_NR],
            auto_flush_ms: None,
            reclaim_interval_ms: AtomicU64::new(0),
            needs_flush_write: AtomicBool::new(false),
            last_flush_jiffies: AtomicU64::new(0),
        };
        // Align idx to seq 满足 bcachefs 不变量 (idx ≡ (seq-1) & BUF_MASK)
        journal.reservations.align_idx_to_seq(state.last_seq);
        // Open first journal entry
        journal.journal_entry_open();
        journal
    }

    /// 导出 Journal 状态快照（用于 close 时持久化到 Superblock）
    /// 对应 bcachefs `bch2_journal_buckets_to_sb()` (sb.c:176)
    pub fn to_superblock_state(&self) -> JournalSuperblockState {
        let sp = self.slowpath.lock().unwrap();
        JournalSuperblockState {
            bucket_addrs: sp.buckets.iter().map(|bs| bs.addr).collect(),
            last_seq: self.journal_cur_seq(),
            last_seq_ondisk: self.last_seq_ondisk.load(Ordering::Acquire),
            last_bucket: sp.current_bucket as u32,
            discard_idx: sp.discard_idx as u32,
            dirty_idx: sp.dirty_idx as u32,
            dirty_idx_ondisk: sp.dirty_idx_ondisk as u32,
            bucket_seq: sp.bucket_seq.clone(),
            replayed_seqs: Vec::new(),
        }
    }

    // ─── 错误处理 ──────────────────────────────────────────

    /// 设置 journal 错误状态（一旦设置不可清除）。
    ///
    /// 对应 bcachefs `bch2_journal_error_set()` (journal.c:672-693)。
    /// 设置后，后续所有 `journal_res_get_fast` 和 `journal_res_get` 返回错误。
    /// 使用原子存储确保并发安全。
    pub fn journal_error_set(&self, err: &JournalError) {
        let code = match err {
            JournalError::Overflow(_) => JE_OVERFLOW,
            JournalError::ChecksumMismatch => JE_CHECKSUM,
            JournalError::Io(_) => JE_IO,
            JournalError::Stuck(_) => JE_STUCK,
            JournalError::Full(_) => JE_FULL,
            JournalError::PinFull(_) => JE_PIN_FULL,
            JournalError::Blocked(_) => JE_BLOCKED,
        };
        // 只存储第一个错误（一旦设置不可覆盖）
        let _ = self.journal_error.compare_exchange(
            JE_NONE,
            code,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    /// 检查 journal 是否处于错误状态。
    ///
    /// 对应 bcachefs `journal_error_check()`。
    /// 返回 `Some(JournalError)` 如果错误已设置。
    pub fn journal_error_check(&self) -> Option<JournalError> {
        let code = self.journal_error.load(Ordering::Acquire);
        match code {
            JE_NONE => None,
            JE_OVERFLOW => Some(JournalError::Overflow("journal error set".into())),
            JE_CHECKSUM => Some(JournalError::ChecksumMismatch),
            JE_IO => Some(JournalError::Io(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "journal io error",
            )))),
            JE_STUCK => Some(JournalError::Stuck("journal error set".into())),
            JE_FULL => Some(JournalError::Full("journal error set".into())),
            JE_PIN_FULL => Some(JournalError::PinFull("journal error set".into())),
            JE_BLOCKED => Some(JournalError::Blocked("journal error set".into())),
            _ => Some(JournalError::Overflow("unknown journal error".into())),
        }
    }

    // ─── New Fastpath API (accept &self, lock-free) ────────

    /// Fastpath reservation — atomic CAS, no mutex.
    ///
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:475-518)。
    ///
    /// 在当前 buf 中原子保留 `req_u64s` 个 u64。成功返回 `JournalRes`，
    /// 失败（空间不足 / refcount 溢出）返回 `JournalError::Overflow`。
    /// 若请求水位线低于当前 journal 水位线，返回 `StorageError::WatermarkTooLow`。
    ///
    /// # Fastpath 特性
    ///
    /// - 仅操作 `AtomicU64`，无锁定
    /// - 共享 `&self` 引用（多线程可同时调用）
    /// - 成功后调用者必须 `commit()` 写入并 `journal_res_put()` 释放
    ///
    /// # Seq 来源
    ///
    /// seq 是 entry 级别而非 reservation 级别：同 entry 内所有 reservation 共享 buf.seq。
    /// seq 在 `journal_entry_open()` 中递增分配，此处直接从 buf 读取。
    /// 对应 bcachefs `journal_res_get_fast()` (journal.h:515) 的 `res->seq = journal_cur_seq(j)`
    /// 减去状态掩码调整。
    ///
    /// # 水位线检查
    ///
    /// 对应 bcachefs `journal.h:502-504`：
    /// ```c
    /// if ((flags & BCH_WATERMARK_MASK) < j->watermark)
    ///     return 0;
    /// ```
    /// 即 request < current → 拒绝。高水位线只允许最紧急操作通过。
    pub fn journal_res_get_fast(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        // 错误状态检查（journal 进入错误状态后所有 reservation 拒绝）
        if let Some(err) = self.journal_error_check() {
            return Err(err);
        }

        // 水位线准入检查（bcachefs journal.h:502-504）
        let current_wm = Watermark::from_bits(self.current_watermark.load(Ordering::Acquire));
        if !current_wm.allows(watermark) {
            return Err(JournalError::Overflow(format!(
                "watermark blocked: request={:?} < current={:?}",
                watermark, current_wm,
            )));
        }

        let (old, _new) = self
            .reservations
            .try_reserve(req_u64s)
            .ok_or_else(|| JournalError::Overflow("slowpath needed".into()))?;

        let idx = JournalResState::idx(old);
        let offset_bytes = JournalResState::cur_entry_offset(old) * 8; // u64 → byte
                                                                       // 读取 buf 的 entry 级别 seq（seq 在 journal_entry_open 中分配）
                                                                       // B1: seq 按 entry 分配，同 entry 内所有 reservation 共享 entry_seq
        let entry_seq = self.bufs.get_mut(idx as usize).seq;

        Ok(JournalRes {
            seq: entry_seq,
            entry_seq,
            offset: offset_bytes,
            u64s: req_u64s,
            buf_idx: idx,
            must_flush: false,
        })
    }

    /// 更新当前 journal 水位线（对应 bcachefs `bch2_journal_set_watermark`）
    ///
    /// 基于利用率自动调整：利用率越高，水位线越高（准入越严格）。
    /// 在 flush() 结束时调用。
    pub fn bch2_journal_set_watermark(&self) {
        let util = self.utilization();
        let wm = Watermark::from_journal_utilization(util);
        let old =
            Watermark::from_bits(self.current_watermark.swap(wm.to_bits(), Ordering::Release));
        // C 语义：新水位线（数值更小=空间更充裕）优于旧水位线时唤醒等待者
        // 对应 bcachefs `swap(watermark, j->watermark); if (watermark > j->watermark) journal_wake(j);`
        // Watermark 枚举定义为 Stripe=0 ... InteriorUpdate=6，数值越小优先级越高
        if wm.to_bits() < old.to_bits() {
            self.bch2_journal_wake_up();
        }
    }

    /// 获取当前 journal 水位线
    pub fn watermark(&self) -> Watermark {
        Watermark::from_bits(self.current_watermark.load(Ordering::Acquire))
    }

    // ─── B4: 错误处理 ─────────────────────────────────────

    /// 设置 journal 错误状态 — 阻止后续所有分配（对应 bcachefs `bch2_journal_set_error`）。
    ///
    /// 一旦设置后不可清除。后续所有 `journal_res_get` 返回此错误。
    /// 使用 `AtomicU8` 而非枚举以支持无锁写入。
    pub fn bch2_journal_error_set(&self, err: JournalError) {
        let code = match &err {
            JournalError::Overflow(_) => 1,
            JournalError::ChecksumMismatch => 2,
            JournalError::Io(_) => 3,
            JournalError::Stuck(_) => 4,
            JournalError::Full(_) => 5,
            JournalError::PinFull(_) => 6,
            JournalError::Blocked(_) => 7,
        };
        // 只在未设置错误时设置（首次写入）
        self.journal_error
            .compare_exchange(JE_NONE, code, Ordering::Release, Ordering::Relaxed)
            .ok();
        self.bch2_journal_wake_up();
    }

    /// 检查 journal 是否处于错误状态（对应 bcachefs `bch2_journal_error`）。
    ///
    /// 返回 `None` 表示无错误，`Some(JournalError)` 表示已设置的具体错误。
    pub fn bch2_journal_error_check(&self) -> Option<JournalError> {
        let code = self.journal_error.load(Ordering::Acquire);
        match code {
            JE_NONE => None,
            1 => Some(JournalError::Overflow("journal error set".into())),
            2 => Some(JournalError::ChecksumMismatch),
            3 => Some(JournalError::Io(StorageError::JournalError(
                "journal error set".into(),
            ))),
            4 => Some(JournalError::Stuck("journal error set".into())),
            5 => Some(JournalError::Full("journal error set".into())),
            6 => Some(JournalError::PinFull("journal error set".into())),
            7 => Some(JournalError::Blocked("journal error set".into())),
            _ => None,
        }
    }

    /// 获取当前 seq（原子，无锁）
    ///
    /// 对应 bcachefs `journal_cur_seq()` (journal.h:137-140)
    pub fn journal_cur_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// 将数据写入 buf 中已保留的位置（无竞争写 —— 每个 reservation offset 唯一）
    ///
    /// 对应 bcachefs `bch2_journal_add_entry()` 写入 buf data 的阶段。
    /// 数据写入 `buf[res.buf_idx].data[res.offset..]`。
    /// 由于每个 reservation 的 offset 由 CAS 保证全局唯一，写入无冲突。
    pub fn add_entry(&self, res: &JournalRes, data: &[u8]) {
        let buf = self.bufs.get_mut(res.buf_idx as usize);
        let offset = res.offset as usize;
        let end = offset + data.len();
        buf.data[offset..end].copy_from_slice(data);
        if res.must_flush {
            buf.has_must_flush = true;
        }
    }

    /// 释放 reservation —— 递减 buf refcount，归零时自动触发写入。
    ///
    /// 对应 bcachefs `bch2_journal_buf_put()` (journal.h:395-403) +
    /// `__bch2_journal_buf_put_final()` (journal.c:240-256)。
    ///
    /// 当 refcount 归零且 buf 处于 Closing 状态时，自动推进到 WriteSubmitted：
    /// - Closing → WriteSubmitted：标记 buf 为待写入，通知等待者
    /// - 实际 I/O 由后续的 `flush()` 统一完成（收集所有 WriteSubmitted buf）
    ///
    /// Accepting 状态的 buf 即使 refcount 归零也不会触发写入，
    /// 因为 flush() 中会统一关闭 entry 并推进写入。
    pub fn journal_res_put(&self, res: &JournalRes) {
        let idx = res.buf_idx;
        // fetch_sub 返回 decrement 前的值
        let old = self.reservations.release(idx);
        let count_before = JournalResState::buf_count(old, idx);

        // refcount 归零 (1→0) 且 buf 已关闭 → 自动推进到 WriteSubmitted
        if count_before == 1 {
            let buf = self.bufs.get_mut(idx as usize);
            if buf.state == BufState::Closing {
                buf.state = BufState::WriteSubmitted;
                buf.notify.notify_waiters();
            }
        }
    }

    // ─── P2-6: 提交回调注册 + wake_up ─────────────────────

    /// 在指定 buf_idx 上注册一个提交完成回调（buf → WriteDone 时调用）。
    pub fn bch2_journal_set_commit_callback(
        &self,
        buf_idx: u32,
        callback: Box<dyn FnOnce() + Send>,
    ) {
        let buf = self.bufs.get_mut(buf_idx as usize);
        buf.write_done_callbacks.push(Some(callback));
    }

    /// 唤醒所有在 journal buf 上等待的线程（对应 bcachefs `journal_wake` = `closure_wake_up(&j->async_wait)`）。
    ///
    /// bcachefs 的 `closure_wake_up` 唤醒所有在 async_wait 上挂起的闭包，
    /// 由释放 journal 空间的操作调用（flush 完成、reclaim 回收、cycle 轮换）。
    ///
    /// 差异：bcachefs 使用单一 closure_waitlist；volmount 使用每个 buf 各有一个 Notify，
    /// 唤醒所有 buf 上的等待者等价于 C 中唤醒所有等待 journal 空间的闭包。
    ///
    /// 注意：`journal_res_put()` 已处理 Closing→WriteSubmitted 状态转换（refcount 归零时自动触发），
    /// 此函数不做状态推进。
    pub fn bch2_journal_wake_up(&self) {
        for idx in 0..JOURNAL_STATE_BUF_NR {
            let buf = self.bufs.get_mut(idx);
            buf.notify.notify_waiters();
        }
    }

    /// 设置 needs_flush_write 标志（journal 有数据需要写入后端）
    pub fn journal_set_needs_flush_write(&self) {
        self.needs_flush_write.store(true, Ordering::Release);
    }

    /// 清除 needs_flush_write 标志（写入完成）
    pub fn journal_clear_needs_flush_write(&self) {
        self.needs_flush_write.store(false, Ordering::Release);
    }

    /// 检查是否有数据需要写入后端
    pub fn journal_needs_flush_write(&self) -> bool {
        self.needs_flush_write.load(Ordering::Acquire)
    }

    /// 标记 journal 恢复完成（恢复→正常运行模式过渡）
    ///
    /// 对应 bcachefs `bch2_journal_set_replay_done()` (init.c:619-631)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 在此函数中：
    /// 1. `bch2_journal_space_available(j)` — 重新计算 space budget
    /// 2. `set_bit(JOURNAL_need_flush_write)` — 首次写入必须 flush
    /// 3. `set_bit(JOURNAL_running)` — 允许 background reclaim
    /// 4. `set_bit(JOURNAL_replay_done)` — 允许 journal seq 推进超过 replay 范围
    ///
    /// # volmount 映射
    ///
    /// volmount 没有 JOURNAL_running/JOURNAL_replay_done 标志位（journal 始终处于运行状态，
    /// Rust Drop 替代了退出逻辑），且 space 计算尚未完全对齐。此函数的 volmount 版本：
    /// 1. 设置 `needs_flush_write = true` — 恢复后首次写入保证是 flush write
    /// 2. 确保 bucket 索引状态与恢复结果一致
    ///
    /// 调用时机：`bch2_fs_recovery()` 所有 pass 完成后、持久化 superblock 之前。
    pub fn bch2_journal_set_replay_done(&self) {
        // 恢复后首次写入必须是 flush write（对应 bcachefs `set_bit(JOURNAL_need_flush_write)`）
        self.journal_set_needs_flush_write();
    }

    /// 关闭 journal：flush 所有 pending entries + 写入空 entry 推进 clock hands
    ///
    /// 对应 bcachefs `bch2_fs_journal_stop()` (init.c:438-485)。
    ///
    /// # bcachefs 语义
    ///
    /// bcachefs 在此函数中：
    /// 1. `bch2_journal_reclaim_stop(j)` — 停止 background reclaim
    /// 2. `bch2_journal_flush_all_pins(j)` — flush 所有 pin
    /// 3. `__bch2_journal_meta(j)` — 写入空 entry 推进 clock hands
    /// 4. `bch2_journal_shutdown_quiesce(j)` — 阻止新 reservation
    /// 5. `clear_bit(JOURNAL_running)` — 标记 journal 不再运行
    ///
    /// # volmount 映射
    ///
    /// volmount 没有 background reclaim（无 background work）、没有 pin flush 分离（flush
    /// 就是 flush current entry）、没有 JOURNAL_running 标志位。此函数的 volmount 版本：
    /// 1. 关闭当前 entry + flush 到后端（对应 `bch2_journal_flush_all_pins`）
    /// 2. 打开新 entry + flush（对应 `__bch2_journal_meta` — 空 entry 推进 clock hands）
    ///
    /// 调用方负责关闭后的封存（持久化 superblock 等）。
    pub async fn bch2_fs_journal_stop(
        &self,
        backend: &dyn BlockDevice,
    ) -> Result<(), JournalError> {
        // 1. flush 所有 pending（关闭当前 entry + 写出到后端）
        //    对应 bcachefs `bch2_journal_flush_all_pins(j)` (reclaim.c:1231)
        self.bch2_journal_flush(backend).await?;

        // 2. 写入空 journal entry 推进 clock hands（对应 bcachefs `__bch2_journal_meta(j)`）
        //    bcachefs 的 journal_meta 写入一个空 entry（只有 header + csum）以确保
        //    last_seq_ondisk 和 seq 之间的所有 seq 都被覆盖。volmount 中 flush 已经
        //    close+write 了当前 entry，这里再 open+flush 一次以达到同样效果。
        self.journal_entry_open();
        self.bch2_journal_flush(backend).await
    }

    /// 更新 last_flush_jiffies 为当前时间戳（自启动以来的毫秒数）
    pub fn journal_update_flush_jiffies(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_flush_jiffies.store(now, Ordering::Release);
    }

    /// 获取上次 flush 的 jiffies 时间戳
    pub fn journal_last_flush_jiffies(&self) -> u64 {
        self.last_flush_jiffies.load(Ordering::Acquire)
    }

    /// 更新 last_seq_ondisk — 由 `bch2_journal_maybe_update_last_seq` 实现。
    /// 对应 bcachefs `bch2_journal_update_last_seq()` (reclaim.c:1088-1116)。
    fn bch2_journal_update_last_seq(&self) {
        self.bch2_journal_maybe_update_last_seq();
    }

    /// Open a new journal entry: find free buf, switch reservations.idx.
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:391-569)。
    ///
    /// seq 在此处递增分配（per-entry），而非在 `journal_res_get_fast` 中（去掉了 per-reservation 分配）。
    /// 对应 bcachefs `__journal_entry_open_one` line 476：
    /// `u64 seq = atomic64_inc_return(&j->seq);`
    ///
    /// 执行：
    /// 1. 分配 entry 级别 seq（fetch_add 1）
    /// 2. 找到可用的 free buf，初始化
    /// 3. 通过 CAS 切换 `reservations.idx` 到新 buf
    /// 4. 注册到 `in_flight` 队列
    /// 5. 推入 pin_fifo 自钉（count=1）
    fn journal_entry_open(&self) {
        let mut in_flight = self.in_flight.lock().unwrap();

        // 1. 分配 entry 级别 seq（对应 bcachefs `__journal_entry_open_one` line 476:
        //    `u64 seq = atomic64_inc_return(&j->seq);`）
        //    每个 journal entry 获得唯一 seq，同 entry 内所有 reservation 共享此 seq。
        let new_seq = self.seq.fetch_add(1, Ordering::AcqRel);

        // 2. 找到 free buf（bcachefs idx++ 循环模式 + seq 断言）
        let idx = self.find_free_buf(new_seq);
        let buf = self.bufs.get_mut(idx as usize);
        buf.reset_for_accepting(new_seq);

        // 3. 通过 CAS 切换 reservations.idx
        self.reservations.open_entry(idx);

        // 4. 注册到 in_flight
        in_flight.push_back(idx);

        // 5. 推入 pin_fifo 自钉（count=1），标记此 entry 在内存中
        //    push_back 只在 journal_entry_open 中发生（被 journal 生命周期序列化）。
        //    UnsafeCell 用于从 &self 获取 &mut 权限。
        //    bcachefs 中对应 journal_entry_open 的 self-pin（JOURNAL_PIN_JOURNAL 类型）。
        unsafe {
            let success = (*self.pin_fifo.get()).push_back(JournalEntryPinList::new(1));
            assert!(
                success.is_ok(),
                "pin_fifo full: journal entries cycled too fast"
            );
        }
    }

    /// Close current entry: stop accepting new reservations.
    ///
    /// 对应 bcachefs `__journal_entry_close_one()` (journal.c:276-384)。
    /// 设置 cur_entry_offset = CLOSED_VAL。
    /// 返回 CAS 关闭前捕获的 cur_entry_offset（单位 u64），
    /// 用于 J2 flush data race 修复中安全设置 buf.data_end。
    fn journal_entry_close(&self) -> u32 {
        self.reservations.close_entry()
    }

    /// 等待指定 buf_idx 的所有 in-flight reservation 完成（refcount 归零）。
    ///
    /// 在 `close_entry()` 后调用：此时 CLOSED_VAL 阻止所有新 reservation，
    /// 但已有 reservation 持有的 refcount 尚未释放。
    /// 自旋等待直到 `buf_count(state, buf_idx) == 0`。
    ///
    /// 这是 J2 flush data race 修复的一部分：
    /// 先 close_entry（原子捕获 offset + 阻止新 reservation），
    /// 再 drain（等待已有 reservation 完成），
    /// 最后设 data_end（此时安全——不再有 thread 写入此 buf 的 data_end 之外）。
    fn wait_for_pending_drain(&self, buf_idx: usize) {
        loop {
            let state = self.reservations.read();
            let count = JournalResState::buf_count(state, buf_idx as u32);
            if count == 0 {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// 找到可用的 free buf。
    ///
    /// 对应 bcachefs `__journal_entry_open_one()` (journal.c:549-564) 的 idx++ 模式：
    /// ```c
    /// new.idx++;
    /// BUG_ON(journal_state_count(new, new.idx));
    /// BUG_ON(new.idx != (seq & JOURNAL_STATE_BUF_MASK));
    /// ```
    /// idx 循环递增（不是线性扫描），且必须等于 `new_seq & JOURNAL_STATE_BUF_MASK`。
    fn find_free_buf(&self, new_seq: u64) -> u32 {
        let old = self.reservations.read();
        let old_idx = JournalResState::idx(old);
        // bcachefs: idx = (idx + 1) & JOURNAL_STATE_BUF_MASK (cyclic increment)
        let idx = ((old_idx + 1) & (JOURNAL_STATE_BUF_NR as u32 - 1)) as usize;
        // bcachefs: BUG_ON(new.idx != (seq & JOURNAL_STATE_BUF_MASK))
        debug_assert_eq!(
            idx as u64,
            new_seq & (JOURNAL_STATE_BUF_NR as u64 - 1),
            "journal idx must equal seq & BUF_MASK"
        );
        let buf = self.bufs.get_mut(idx);
        // 对应 bcachefs: BUG_ON(journal_state_count(new, new.idx)) — buf 必须可用
        assert!(
            buf.state == BufState::Free || buf.state == BufState::WriteDone,
            "journal buf {} not free (state={:?}, seq={}): \
             bcachefs would fail with journal_max_open",
            idx,
            buf.state,
            new_seq,
        );
        buf.state = BufState::Free;
        idx as u32
    }

    // ─── Convenience: old append API (now uses new fastpath) ──

    /// 追加 btree update（insert/delete）
    ///
    /// 使用新 fastpath API（接受 `&self`，无锁）:
    /// 1. `journal_res_get_fast()` — 在 buf 中原子保留空间
    /// 2. `commit()` — 将序列化的 Jset 写入 buf
    /// 3. `journal_res_put()` — 释放 refcount
    ///
    /// # 参数
    /// - `must_flush`: 若为 true，append 完成后立即调用 backend.flush() 保证持久化
    /// - `backend`: 后端块设备（仅当 must_flush=true 时需要）
    ///
    /// 返回分配的 seq。
    pub async fn append(
        &self,
        btree_type: BtreeId,
        entries: &[BtreeEntry],
        must_flush: bool,
        backend: &dyn BlockDevice,
    ) -> Result<u64, JournalError> {
        // 构建 JsetEntry
        let entries_bytes = bincode::serialize(&entries.to_vec())
            .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
        let entry = RawJsetEntry::new(
            btree_type as u8,
            JsetEntryType::BtreeKeys as u8,
            entries_bytes,
        )
        .map_err(JournalError::Io)?;
        let jset_base = Jset {
            header: JsetHeader {
                magic: super::jset::JOURNAL_MAGIC,
                seq: 0,
                last_seq: self.last_seq_ondisk.load(Ordering::Acquire),
                crc32: 0,
                entry_count: 1,
                version: super::jset::JSET_VERSION as u32,
                csum_type: super::jset::CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: vec![entry],
        };
        // seq=0 不影响布局大小；直接计算 padding 后长度，避免为了 size 预先分配完整 buffer。
        let size0 = jset_base.serialized_padded_len();
        let req_u64s = size0.div_ceil(8) as u32;
        let res = self.journal_res_get_fast(Watermark::Btree, req_u64s)?;
        let jset = Jset {
            header: JsetHeader {
                seq: res.seq,
                ..jset_base.header
            },
            entries: jset_base.entries,
        };
        let serialized = jset.serialize_padded().map_err(JournalError::Io)?;

        // 设置 must_flush 标志，add_entry 会将其传播到 buf.has_must_flush
        let mut res = res;
        res.must_flush = must_flush;
        self.add_entry(&res, &serialized);

        self.journal_res_put(&res);

        if must_flush {
            self.bch2_journal_flush(backend).await?;
        }

        Ok(res.seq)
    }

    /// 追加 btree_root entry（记录 root 指针变化）
    pub async fn append_btree_root(
        &self,
        btree_type: BtreeId,
        root_addr: u64,
        must_flush: bool,
        backend: &dyn BlockDevice,
    ) -> Result<u64, JournalError> {
        let root_entry = BtreeEntry::new(
            crate::btree::key::Bpos::new(0, root_addr, 0),
            crate::btree::key::KeyType::Normal,
            crate::btree::key::KeyValue::Raw(vec![]),
        );
        let entries_bytes = bincode::serialize(&vec![root_entry])
            .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
        let entry = RawJsetEntry::new(
            btree_type as u8,
            JsetEntryType::BtreeRoot as u8,
            entries_bytes,
        )
        .map_err(JournalError::Io)?;
        let jset_template = Jset {
            header: JsetHeader {
                magic: super::jset::JOURNAL_MAGIC,
                seq: 0,
                last_seq: self.last_seq_ondisk.load(Ordering::Acquire),
                crc32: 0,
                entry_count: 1,
                version: super::jset::JSET_VERSION as u32,
                csum_type: super::jset::CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: vec![entry],
        };
        let res = self.journal_res_get_fast(
            Watermark::InteriorUpdate,
            jset_template.serialized_padded_len().div_ceil(8) as u32,
        )?;
        let jset = Jset {
            header: JsetHeader {
                seq: res.seq,
                ..jset_template.header
            },
            entries: jset_template.entries,
        };
        let serialized = jset.serialize_padded().map_err(JournalError::Io)?;
        let mut res = res;
        res.must_flush = must_flush;
        self.add_entry(&res, &serialized);
        self.journal_res_put(&res);

        if must_flush {
            self.bch2_journal_flush(backend).await?;
        }

        Ok(res.seq)
    }

    // ─── Bucket write: bch2_journal_write ─────────────────

    /// 将一个 journal buf 数据写入设备（对应 bcachefs `bch2_journal_write`, write.c:819-946）。
    ///
    /// # 写入管道（bcachefs 对齐）
    ///
    /// 1. **Prep**: 捕获 buf 数据，触发写入完成回调，设置 BufState::WriteDone
    /// 2. **Alloc**: 分配设备空间（当前为检查 bucket 旋转）
    /// 3. **Checksum**: 计算校验和（当前为预留，csum_type=NONE 时跳过）
    /// 4. **Submit**: 按 JSET_BLOCK_SIZE 分块写入设备（对应 bvec 迭代）
    ///
    /// # 参数
    /// - `buf_data`: 待写入的 buf 数据
    /// - `buf_must_flush`: 是否需要后端 flush
    /// - `buf_no_flush`: noflush 优化标志（跳过 flush 节省一次 fsync）
    /// - `backend`: 块设备后端
    async fn bch2_journal_write(
        &self,
        buf_data: &[u8],
        buf_must_flush: bool,
        buf_no_flush: bool,
        backend: &dyn BlockDevice,
    ) -> Result<(), JournalError> {
        // === Phase 2: Alloc — 检查/旋转 bucket（对应 bcachefs journal_advance_devs_to_next_bucket, write.c:29-57） ===
        // bcachefs journal_write_alloc（write.c:112-159）是多设备 extent 分配器，为 journal
        // 数据分配带 device ptr 的 extent bkey。volmount 是单设备架构，省去 replicas 管理，
        // 只需在 current bucket 空间不足时旋转到下一个 bucket。
        let needs_rotate = {
            let sp = self.slowpath.lock().unwrap();
            sp.remaining_bytes < JSET_BLOCK_SIZE
        };
        if needs_rotate {
            self.bch2_journal_rotate_or_reclaim(backend).await?;
        }

        // === Phase 4: Submit — 每次写一块（对应 bcachefs journal_write_submit, write.c:513-583） ===
        // bcachefs 通过 extent_for_each_ptr 遍历设备 bkey 中的每个 ptr，为每个设备提交一个 bio。
        // volmount 当前为单设备后端，使用 write_block 提交，按 JSET_BLOCK_SIZE 分块写入。
        let mut write_offset = 0;
        while write_offset < buf_data.len() {
            let needs_rotate;
            {
                let sp = self.slowpath.lock().unwrap();
                needs_rotate = sp.remaining_bytes < JSET_BLOCK_SIZE;
            }
            if needs_rotate {
                self.bch2_journal_rotate_or_reclaim(backend).await?;
            }
            let chunk_size = JSET_BLOCK_SIZE as usize;
            let end = (write_offset + chunk_size).min(buf_data.len());
            let chunk = &buf_data[write_offset..end];

            let block_addr = {
                let sp = self.slowpath.lock().unwrap();
                let bucket_start = sp.buckets[sp.current_bucket].addr;
                let block_idx = sp.current_offset / JSET_BLOCK_SIZE;
                BlockAddr::new(bucket_start + block_idx as u64)
            };

            let mut block_data = vec![0u8; JSET_BLOCK_SIZE as usize];
            block_data[..chunk.len()].copy_from_slice(chunk);
            backend.write_block(block_addr, &block_data).await?;

            {
                let mut sp = self.slowpath.lock().unwrap();
                sp.current_offset += JSET_BLOCK_SIZE;
                sp.remaining_bytes = sp.remaining_bytes.saturating_sub(JSET_BLOCK_SIZE);
            }
            write_offset += chunk_size;
        }

        // 落盘策略 — 对应 bcachefs journal_write_submit 中的 opf | REQ_FUA / REQ_PREFLUSH
        // bcachefs 通过 FUA 或分离式 PREFLUSH 保证持久化。
        // volmount 通过 backend.flush() 实现。
        // 若 buf_no_flush 为 true，跳过此 flush（等待下次 flush 周期自然落盘）。
        if buf_must_flush && !buf_no_flush {
            backend.flush().await?;
        }
        Ok(())
    }

    /// 将所有 WriteSubmitted buf 写入 bucket（调用 `bch2_journal_write` 逐个处理）。
    ///
    /// 对应 bcachefs `bch2_journal_flush()` 中的写入循环部分。
    /// bcachefs 通过 `closure_call(&w->io, bch2_journal_write, ...)` 异步提交每个 buf。
    async fn write_bufs_to_bucket(&self, backend: &dyn BlockDevice) -> Result<(), JournalError> {
        for idx in 0..JOURNAL_STATE_BUF_NR {
            let state = self.bufs.get_mut(idx).state;
            if state != BufState::WriteSubmitted {
                continue;
            }

            // === Phase 1: Prep — 捕获 buf 数据 + 触发写入完成回调（对应 bcachefs bch2_journal_write_prep, write.c:621-733） ===
            // bcachefs prep 在写时执行：压缩空 entry、传播 btree_root、刷新 write_buffer_keys 到
            // write buffer、添加 datetime 和 super entries。volmount 的数据在 append/commit 时已通过
            // bincode 序列化完成（jset 不变），无需写时 prep 压缩。此处仅拷贝 data_end 截断的数据、
            // 触发写入完成回调、并转换 buf 状态。
            let (buf_data, buf_must_flush, buf_no_flush) = {
                let buf = self.bufs.get_mut(idx);
                let end = buf.data_end.min(buf.data.len());
                let data = buf.data[..end].to_vec();
                let must_flush = buf.has_must_flush;
                let no_flush = buf.no_flush;
                // P2-6: 触发所有写入完成回调（在状态变更为 WriteDone 前消费）
                for cb_opt in buf.write_done_callbacks.drain(..) {
                    if let Some(cb) = cb_opt {
                        cb();
                    }
                }
                buf.state = BufState::WriteDone;
                (data, must_flush, no_flush)
            };

            self.bch2_journal_write(&buf_data, buf_must_flush, buf_no_flush, backend)
                .await?;
        }
        Ok(())
    }

    /// flush pending buf data to backend（对应 bcachefs `bch2_journal_flush`）
    ///
    /// # 顺序（J2 flush data race 修复后）
    ///
    /// 修复了 bcachefs 风格的 data race：旧顺序（read offset → set data_end → close_entry）
    /// 在 data_end 与 close_entry 之间有一个窗口，此时新 reservation 写入的数据超过 data_end。
    ///
    /// 新顺序（close_entry → drain → set data_end）：
    /// 1. 捕获当前 accepting buf 索引
    /// 2. 关闭当前 entry（原子捕获 final offset + CLOSED_VAL 阻止新 reservation）
    /// 3. 等待旧 buf 的 refcount 归零（已有 reservation 完成写入）
    /// 4. 设置 buf.data_end（安全：不再有 reservation 写入此 buf）
    /// 5. 将所有 Accepting buf → Closing
    /// 6. 将所有 Closing buf → WriteSubmitted（通知等待者）
    /// 7. 将所有 WriteSubmitted buf 数据写入 bucket（按 data_end 截断）
    /// 8. backend.flush()
    /// 9. 打开新 entry（后续 append 用）
    ///
    /// # 设计说明
    ///
    /// `journal_res_put()` 仅递减 refcount，不触发写入。
    /// `bch2_journal_flush()` 统一管理 buf 状态转换和 bucket 写入，
    /// 确保一次 flush 将所有累积的 buf 数据落盘。
    /// 只写 `data_end` 字节而非 BUF_SIZE，避免零 padding 浪费。
    ///
    /// # 并发
    ///
    /// 接受 `&self`，通过内部 Mutex 序列化 bucket 状态修改。
    pub async fn bch2_journal_flush(&self, backend: &dyn BlockDevice) -> Result<(), JournalError> {
        // P2-7: 标记有数据需要写入
        self.journal_set_needs_flush_write();

        // 1. 捕获当前 accepting buf 索引
        let old_idx = JournalResState::idx(self.reservations.read()) as usize;

        // 2. 关闭当前 entry — 原子捕获 final offset + 设置 CLOSED_VAL 阻止新 reservation
        //    J2 fix: 先 close_entry，再设 data_end（防止并发 reservation 数据被截断）
        let final_off = self.journal_entry_close();

        // 3. 等待旧 buf 上所有 in-flight reservation 完成（refcount 归零）
        //    close_entry 后 CLOSED_VAL 阻止新 reservation，
        //    但已有 reservation 仍持有 refcount。
        //    等待 refcount→0 确保所有写入已完成，data_end 设置不会截断数据。
        if old_idx < JOURNAL_STATE_BUF_NR {
            self.wait_for_pending_drain(old_idx);
        }

        // 4. 设置 buf.data_end（安全：不再有 reservation 写入此 buf）
        if old_idx < JOURNAL_STATE_BUF_NR {
            let used_bytes = (final_off as usize) * 8; // u64 → byte
            let buf = self.bufs.get_mut(old_idx);
            buf.data_end = used_bytes.min(BUF_SIZE);
        }

        // 3. Accepting → Closing（标记为待写入）
        for idx in 0..JOURNAL_STATE_BUF_NR {
            let buf = self.bufs.get_mut(idx);
            if buf.state == BufState::Accepting {
                buf.state = BufState::Closing;
            }
        }

        // 4. Closing → WriteSubmitted（通知写入线程）
        for idx in 0..JOURNAL_STATE_BUF_NR {
            let buf = self.bufs.get_mut(idx);
            if buf.state == BufState::Closing {
                buf.state = BufState::WriteSubmitted;
                buf.notify.notify_waiters();
            }
        }

        // 5. 将所有 WriteSubmitted buf 写入 bucket（按 data_end 截断）
        self.write_bufs_to_bucket(backend).await?;

        // 6. backend flush（确认落盘持久化）
        backend.flush().await?;

        // P2-7: 清除 needs_flush_write 标志 + 更新 jiffies
        self.journal_clear_needs_flush_write();
        self.journal_update_flush_jiffies();

        // === post-write cleanup（对应 bcachefs journal_write_done, write.c:234-466） ===
        //
        // bcachefs journal_write_done 是 closure 回调链的终点，在 bio 完成（_endio）后
        // 被调用。它处理：
        //   1. replicas 引用更新（put 旧 devs，get 新 replicas entry）
        //   2. in_flight FIFO front 推进（释放 buf 供重用）
        //   3. seq_ondisk / flushed_seq_ondisk / last_seq_ondisk 更新
        //   4. flushed_seq 的 waiters 唤醒
        //   5. last_seq_ondisk 更新后的 discards 触发 + reclaim kick
        //
        // volmount 的 cleanup 顺序在 write_bufs_to_bucket 同步完成后执行，等价于
        // bcachefs 中所有 bio 完成后的收尾工作：
        //   7. 更新 bucket_seq + 释放自钉
        let mut flushed_seqs: Vec<u64> = Vec::new();
        for idx in 0..JOURNAL_STATE_BUF_NR {
            let buf = self.bufs.get_mut(idx);
            if buf.state == BufState::WriteDone && buf.seq > 0 {
                flushed_seqs.push(buf.seq);
            }
        }
        for seq in &flushed_seqs {
            self.update_bucket_seq(*seq);
            self.__bch2_journal_pin_put(*seq);
        }

        // 8. flush 所有 ≤ cur_seq 的 pin callback
        //    在 pin_put 后执行（自钉已释放），可触发 btree/key-cache 的写回调。
        //    当前仅当有外部 pin 注册了 flush_fn 时才实际工作。
        let cur_seq_for_flush = self.seq.load(Ordering::Acquire);
        // flush_pins callback 错误在此路径中忽略，不中断 flush 流程
        // （bcachefs 在 reclaim 路径中处理 callback 错误，write path 继续。
        //  callback 错误通过 reclaim 路径的 ? 传播，参见 __bch2_journal_reclaim。）
        let _ = self.journal_flush_pins(cur_seq_for_flush);

        // 9. 更新 flushed_seq_marker 并尝试推进 last_seq_ondisk
        let cur_seq = self.seq.load(Ordering::Acquire);
        self.flushed_seq_marker.store(cur_seq, Ordering::Release);
        self.bch2_journal_update_last_seq();

        // 9. 打开新 entry（会添加新自钉）
        self.journal_entry_open();

        // 10. 更新水位线（flush 后利用率可能变化）
        self.bch2_journal_set_watermark();

        Ok(())
    }

    /// 等待所有 in-flight buf 写入完成
    ///
    /// 对应 design.md §4 中的 `wait_all_done()` 接口。
    /// 委托给 `bch2_journal_flush`。
    pub async fn bch2_journal_flush_all(
        &self,
        backend: &dyn BlockDevice,
    ) -> Result<(), JournalError> {
        self.bch2_journal_flush(backend).await
    }

    // ─── Utilization (unchanged) ──────────────────────────

    /// 返回当前写入率（0.0~1.0），1.0 = 满
    pub fn utilization(&self) -> f64 {
        let sp = self.slowpath.lock().unwrap();
        let total_bucket_bytes =
            (sp.buckets.len() as u64) * (BUCKET_BLOCKS as u64) * (JSET_BLOCK_SIZE as u64);
        let used: u64 = if sp.current_bucket > 0 {
            (sp.current_bucket as u64) * (BUCKET_BLOCKS as u64) * (JSET_BLOCK_SIZE as u64)
                + (sp.current_offset as u64)
        } else {
            sp.current_offset as u64
        };
        if total_bucket_bytes == 0 {
            return 0.0;
        }
        (used as f64) / (total_bucket_bytes as f64)
    }

    // ─── Read (unchanged) ─────────────────────────────────

    /// 读取一个 journal bucket 的全部 Jset entries（用于 replay，对应 bcachefs `bch2_journal_read`）
    pub async fn bch2_journal_read(
        &self,
        backend: &dyn BlockDevice,
        bucket_idx: u32,
    ) -> Result<Vec<Jset>, JournalError> {
        let bucket_start = {
            let sp = self.slowpath.lock().unwrap();
            let idx = bucket_idx as usize;
            if idx >= sp.buckets.len() {
                return Ok(Vec::new());
            }
            sp.buckets[idx].addr
        };
        let mut jsets = Vec::new();

        for block_off in 0..BUCKET_BLOCKS {
            let block_addr = BlockAddr::new(bucket_start + block_off as u64);
            let mut buf = vec![0u8; JSET_BLOCK_SIZE as usize];

            match backend.read_block(block_addr, &mut buf).await {
                Ok(()) => match Jset::deserialize(&buf) {
                    Ok(Some(jset)) => {
                        // bcachefs 顺序：bch2_jset_validate_early (magic+CRC) → bch2_jset_validate (entry)
                        if !jset.verify() || !super::validate::jset_validate(&jset) {
                            continue;
                        }
                        jsets.push(jset);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                },
                Err(StorageError::BlockNotFound(_)) => break,
                Err(e) => return Err(JournalError::Io(e)),
            }
        }

        Ok(jsets)
    }

    /// 反向读取一个 journal bucket 的 Jset entries（R6: journal rewind 支持）
    ///
    /// 从 bucket 末尾开始反向扫描，找到有效的 Jset 后继续向后（向前回溯）扫描。
    /// 返回的 jsets 按 seq 升序排列（最早的在前），与 bch2_journal_read 返回值一致。
    /// 在 journal head 指针丢失时可用于查找最近的 journal entries。
    pub async fn bch2_journal_read_reverse(
        &self,
        backend: &dyn BlockDevice,
        bucket_idx: u32,
    ) -> Result<Vec<Jset>, JournalError> {
        let bucket_start = {
            let sp = self.slowpath.lock().unwrap();
            let idx = bucket_idx as usize;
            if idx >= sp.buckets.len() {
                return Ok(Vec::new());
            }
            sp.buckets[idx].addr
        };
        let mut jsets_rev = Vec::new();

        for block_off in (0..BUCKET_BLOCKS).rev() {
            let block_addr = BlockAddr::new(bucket_start + block_off as u64);
            let mut buf = vec![0u8; JSET_BLOCK_SIZE as usize];

            match backend.read_block(block_addr, &mut buf).await {
                Ok(()) => match Jset::deserialize(&buf) {
                    Ok(Some(jset)) => {
                        if !jset.verify() || !super::validate::jset_validate(&jset) {
                            continue;
                        }
                        jsets_rev.push(jset);
                    }
                    Ok(None) => break,
                    Err(_) => break,
                },
                Err(StorageError::BlockNotFound(_)) => break,
                Err(e) => return Err(JournalError::Io(e)),
            }
        }

        // 反向扫描得到的是从高 block 到低 block 的顺序
        // jsets_rev 中最早 seq 的在末尾，按 seq 升序排列需要反转
        jsets_rev.reverse();
        Ok(jsets_rev)
    }

    /// 遍历所有 journal bucket 中的所有 Jset entries（用于 replay，对应 bcachefs `bch2_journal_entries_read`）
    pub async fn bch2_journal_entries_read(
        &self,
        backend: &dyn BlockDevice,
    ) -> Result<Vec<(u32, Jset)>, JournalError> {
        let bucket_count = {
            let sp = self.slowpath.lock().unwrap();
            sp.buckets.len()
        };
        let mut all = Vec::new();
        for bucket_idx in 0..bucket_count as u32 {
            let jsets = self.bch2_journal_read(backend, bucket_idx).await?;
            for jset in jsets {
                all.push((bucket_idx, jset));
            }
        }
        Ok(all)
    }

    // ─── Bucket management (unchanged) ────────────────────

    /// 更新 bucket_seq[当前 bucket] 为 max(当前值, jset_seq)
    fn update_bucket_seq(&self, jset_seq: u64) {
        let mut sp = self.slowpath.lock().unwrap();
        let idx = sp.current_bucket;
        if idx < sp.bucket_seq.len() {
            sp.bucket_seq[idx] = sp.bucket_seq[idx].max(jset_seq);
        }
    }

    /// 推进 dirty_idx（使用已完成回收/flush 的 last_seq_ondisk 作为边界）
    fn advance_dirty_idx(&self) {
        let nr;
        let cur_idx;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
            cur_idx = sp.current_bucket;
        }
        if nr == 0 {
            return;
        }
        let last_seq = self.last_seq_ondisk.load(Ordering::Acquire);
        loop {
            let mut sp = self.slowpath.lock().unwrap();
            if sp.dirty_idx == cur_idx {
                break;
            }
            if sp.bucket_seq.get(sp.dirty_idx).copied().unwrap_or(0) < last_seq {
                sp.dirty_idx = (sp.dirty_idx + 1) % nr;
            } else {
                break;
            }
        }
    }

    /// 推进 dirty_idx_ondisk：确认落盘后推进
    fn advance_dirty_idx_ondisk(&self) {
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return;
        }
        let last_seq = self.last_seq_ondisk.load(Ordering::Acquire);
        loop {
            let mut sp = self.slowpath.lock().unwrap();
            if sp.dirty_idx_ondisk == sp.dirty_idx {
                break;
            }
            if sp.bucket_seq.get(sp.dirty_idx_ondisk).copied().unwrap_or(0) < last_seq {
                sp.dirty_idx_ondisk = (sp.dirty_idx_ondisk + 1) % nr;
            } else {
                break;
            }
        }
    }

    /// 计算需要 flush 的最老 journal seq。
    ///
    /// 对应 bcachefs `journal_seq_to_flush()` (reclaim.c:861-888)：
    /// 1. 对所有 RW member 设备，计算让 journal 保持最多半满时
    ///    需要 flush 的 bucket 对应的 seq，取最大值。
    /// 2. 取 pin FIFO 半满目标：`cur_seq - pin.size / 2`.
    ///
    /// volmount 当前为单设备实现，无 per-device bucket_seq 追踪，
    /// 因此只使用 pin FIFO 半满规则。
    pub fn journal_seq_to_flush(&self) -> u64 {
        // pin FIFO 半满规则 — 尝试让 pin FIFO 保持最多半满。
        // 对应 bcachefs reclaim.c:885-887:
        //   max_t(s64, seq_to_flush, (s64) journal_cur_seq(j) - (j->pin.size >> 1));
        let cur_seq = self.journal_cur_seq();
        cur_seq.saturating_sub((PIN_FIFO_SIZE / 2) as u64)
    }

    /// 检查是否需要执行 journal reclaim。
    ///
    /// 对应 bcachefs `__bch2_journal_reclaim()` 中的触发条件检查(reclaim.c:1111-1134)：
    /// 1. 时间触发：距离上次 flush 超过 reclaim_delay_ms
    /// 2. 空间触发：journal 可用空间低于中间水位线
    ///
    /// 后续可扩展：btree cache 脏比例、key cache 积压（需要外部参数传入）。
    pub fn journal_reclaim_needed(&self, reclaim_delay_ms: u64) -> bool {
        // 条件 1：时间触发 — 对应 bcachefs reclaim.c:1111-1113
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let elapsed = now.saturating_sub(self.journal_last_flush_jiffies());
        if reclaim_delay_ms > 0 && elapsed >= reclaim_delay_ms {
            return true;
        }

        // 条件 2：空间触发 — journal 可用空间低于 Watermark::Reclaim 阈值
        // 对应 bcachefs reclaim.c:1118 `journal_med_on_space(j)`
        let (total, clean, _) = self.compute_journal_space();
        // 如果 clean（=可循环写入的空间）不足总空间的 25%，触发回收
        if total > 0 && clean < total / 4 {
            return true;
        }

        false
    }

    /// 回收可重用的 journal bucket（对应 bcachefs `__bch2_journal_reclaim`）
    ///
    /// 与 bcachefs 对齐 (reclaim.c:1047-1182)：
    /// 1. 持有 `reclaim_lock` 串行化回收 (reclaim.c:1073)
    /// 2. 检查 journal 错误状态 (reclaim.c:1090-1092)
    /// 3. do-while 循环 flush pins：计算 seq_to_flush → 检查触发条件 → flush_pins → 继续
    /// 4. 更新 last_seq / dirty idx
    /// 5. TRIM 可回收 bucket
    ///
    /// - `direct=true`: 前台模式，单次 pass，等同 bcachefs `bch2_journal_reclaim(j)`
    /// - `direct=false`: 后台模式，在触发条件满足时循环，等同 bcachefs `__bch2_journal_reclaim(j, false, kicked)`
    pub async fn __bch2_journal_reclaim(
        &self,
        backend: &dyn BlockDevice,
        direct: bool,
    ) -> Result<(), JournalError> {
        // === Phase 1: Flush + advance（同步，持有 reclaim_lock）===
        // 对应 bcachefs reclaim.c:1073-1179 的 scoped_guard(mutex, &j->reclaim_lock)
        //
        // reclaim_lock 保护 flush 和 idx 更新，防止并发 reclaim 竞争。
        // 注意：lock scope 必须在 .await 之前结束，因为 std::sync::MutexGuard 不是 Send。
        let reclaim_delay_ms = self.reclaim_interval_ms.load(Ordering::Acquire);
        {
            let _lock = self.reclaim_lock.lock().unwrap();

            // 检查 journal 错误状态 — 对应 bcachefs reclaim.c:1090-1092
            if let Some(err) = self.journal_error_check() {
                return Err(err);
            }

            // do-while 主 flush 循环 — 对应 bcachefs reclaim.c:1083 do-while
            //
            // bcachefs 循环条件：(min_nr || min_key_cache) && nr_flushed && !direct
            // - 前台 direct: 单次 pass（min_nr=0 时仍可能因 nr_flushed 退出）
            // - 后台 !direct: 满足触发条件 && flushed > 0 时继续循环
            loop {
                // 触发条件检查 — 对应 bcachefs reclaim.c:1108-1134
                if !direct && !self.journal_reclaim_needed(reclaim_delay_ms) {
                    break;
                }

                // 计算需要 flush 的最老 seq — 对应 bcachefs reclaim.c:1100
                let seq_to_flush = self.journal_seq_to_flush();
                // 执行 pin flush — 对应 bcachefs reclaim.c:1153 journal_flush_pins
                // callback 错误通过 ? 传播，reclaim 调用者处理错误
                let nr_flushed = self.journal_flush_pins(seq_to_flush)?;
                if nr_flushed == 0 {
                    break;
                }
                // 统计计数 — 对应 bcachefs reclaim.c:1160-1162
                if direct {
                    self.nr_direct_reclaim
                        .fetch_add(nr_flushed as u64, Ordering::Relaxed);
                } else {
                    self.nr_background_reclaim
                        .fetch_add(nr_flushed as u64, Ordering::Relaxed);
                }
                // 前台 direct 模式：单次 pass — 对应 bcachefs reclaim.c:1179 && !direct
                if direct {
                    break;
                }
                // 后台模式：继续循环直到触发条件不满足或无工作可做
            }

            self.bch2_journal_update_last_seq();
            self.advance_dirty_idx();
            self.advance_dirty_idx_ondisk();
        } // reclaim_lock 在此释放，之后进入 async TRIM 阶段

        // === Phase 2: TRIM 可回收 bucket（异步，无需 reclaim_lock）===
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return Ok(());
        }

        loop {
            let bucket_addr;
            {
                let sp = self.slowpath.lock().unwrap();
                if sp.discard_idx == sp.dirty_idx_ondisk {
                    break;
                }
                bucket_addr = sp.buckets[sp.discard_idx].addr;
            }
            for bi in 0..BUCKET_BLOCKS {
                backend
                    .trim_block(BlockAddr::new(bucket_addr + bi as u64))
                    .await
                    .ok();
            }
            let mut sp = self.slowpath.lock().unwrap();
            sp.discard_idx = (sp.discard_idx + 1) % nr;
        }
        Ok(())
    }

    /// 前台 reclaim 入口 — 单次 pass，等同 bcachefs `bch2_journal_reclaim(j)`。
    pub async fn bch2_journal_reclaim(
        &self,
        backend: &dyn BlockDevice,
    ) -> Result<(), JournalError> {
        self.__bch2_journal_reclaim(backend, true).await
    }

    /// 阻塞等待所有 ≤ seq_to_flush 的 pin 完成 flush。
    ///
    /// 内部调用 `journal_flush_pins`（单次 pass），若 flush 无工作则返回；
    /// 否则重试。对应 bcachefs `bch2_journal_flush_pins()` (reclaim.c:1399-1411)。
    ///
    /// 返回 `Ok(true)` 表示至少执行了一次 flush callback。
    /// 若 callback 返回错误，传播 `Err(StorageError)`。
    ///
    /// # bcachefs 差异
    ///
    /// bcachefs 使用 closure_wait_event + journal_flush_done 在持有 reclaim_lock
    /// 的情况下循环等待。volmount 当前为同步/半同步引擎，使用简单的重试循环。
    pub fn bch2_journal_flush_pins(&self, seq_to_flush: u64) -> Result<bool, StorageError> {
        let mut did_work = false;
        loop {
            let flushed = self.journal_flush_pins(seq_to_flush)?;
            if flushed == 0 {
                return Ok(did_work);
            }
            did_work = true;
            std::thread::yield_now();
        }
    }

    /// 轮换到下一个 bucket（对应 bcachefs `bch2_journal_rotate_or_reclaim`）
    pub async fn bch2_journal_rotate_or_reclaim(
        &self,
        backend: &dyn BlockDevice,
    ) -> Result<(), JournalError> {
        let nr;
        {
            let sp = self.slowpath.lock().unwrap();
            nr = sp.buckets.len();
        }
        if nr == 0 {
            return Err(JournalError::Overflow("no journal buckets".into()));
        }

        {
            let mut sp = self.slowpath.lock().unwrap();
            let next = (sp.current_bucket + 1) % nr;
            if next != sp.dirty_idx {
                sp.current_bucket = next;
                sp.current_offset = 0;
                sp.remaining_bytes = BUCKET_BLOCKS * JSET_BLOCK_SIZE;
                return Ok(());
            }
        }

        self.bch2_journal_reclaim(backend).await?;

        {
            let mut sp = self.slowpath.lock().unwrap();
            let next2 = (sp.current_bucket + 1) % nr;
            if next2 == sp.dirty_idx {
                return Err(JournalError::Overflow(String::from(
                    "all journal buckets exhausted after reclaim",
                )));
            }
            sp.current_bucket = next2;
            sp.current_offset = 0;
            sp.remaining_bytes = BUCKET_BLOCKS * JSET_BLOCK_SIZE;
        }
        Ok(())
    }

    // ─── Blacklist ─────────────────────────────────────────

    /// 写 blacklist entries 到 journal（对应 bcachefs `bch2_journal_seq_blacklist_add`）
    pub async fn bch2_journal_seq_blacklist_add(
        &self,
        start_seq: u64,
        end_seq: u64,
        backend: &dyn BlockDevice,
    ) -> Result<u64, JournalError> {
        let payload = bincode::serialize(&vec![BlacklistEntry { start_seq, end_seq }])
            .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
        let entry = RawJsetEntry::new(0, JsetEntryType::Blacklist as u8, payload)
            .map_err(JournalError::Io)?;
        let jset = Jset {
            header: JsetHeader {
                magic: super::jset::JOURNAL_MAGIC,
                seq: 0,
                last_seq: self.last_seq_ondisk.load(Ordering::Acquire),
                crc32: 0,
                entry_count: 1,
                version: super::jset::JSET_VERSION as u32,
                csum_type: super::jset::CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: vec![entry],
        };
        let serialized = jset.serialize_padded().map_err(JournalError::Io)?;
        let req_u64s = serialized.len().div_ceil(8) as u32;

        let res = self.journal_res_get_fast(Watermark::InteriorUpdate, req_u64s)?;
        let buf = self.bufs.get_mut(res.buf_idx as usize);
        let offset = res.offset as usize;
        buf.data[offset..offset + serialized.len()].copy_from_slice(&serialized);
        self.journal_res_put(&res);

        // 立即 flush（blacklist 必须持久化后才能写 superblock）
        self.bch2_journal_flush(backend).await?;

        Ok(res.seq)
    }

    // ═══════════════════════════════════════════════════════════
    // Part 6a: New Slowpath Methods
    // ═══════════════════════════════════════════════════════════

    /// 获取指定水位线可用的 journal 空间字节数（对应 bcachefs `bch2_journal_space_available`）
    ///
    /// 基于 current_watermark 和空间分类决定。
    /// - `Stripe` / `Normal` → 只算 DISCARDED（最安全的空间）
    /// - `CopyGC` / `Btree` → 算 CLEAN_ONDISK
    /// - `BtreeCopyGC` / `Reclaim` → 算 CLEAN
    /// - `InteriorUpdate` → 算 TOTAL（全部空间）
    /// 从 slowpath 索引实时计算 4 个空间槽值（无缓存）
    ///
    /// 返回 `(total, clean, clean_ondisk)` 字节数。
    /// - total = 全部 journal bucket 容量
    /// - clean = current_bucket 到 dirty_idx 之间（可循环写入的 bucket 容量）
    /// - clean_ondisk = current_bucket 到 dirty_idx_ondisk 之间（已落盘的 bucket 容量）
    ///
    /// discarded ≈ clean（volmount 暂不追踪 per-bucket discard 状态）。
    fn compute_journal_space(&self) -> (u64, u64, u64) {
        let sp = self.slowpath.lock().unwrap();
        let nr = sp.buckets.len();
        if nr == 0 {
            return (0, 0, 0);
        }
        let cur = sp.current_bucket;
        let total_buckets = nr;
        let clean_buckets = (cur + nr - sp.dirty_idx) % nr;
        let clean_ondisk_buckets = (cur + nr - sp.dirty_idx_ondisk) % nr;

        let bucket_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) as u64;
        (
            total_buckets as u64 * bucket_bytes,
            clean_buckets as u64 * bucket_bytes,
            clean_ondisk_buckets as u64 * bucket_bytes,
        )
    }

    /// 返回指定水位线可用的 journal 空间字节数（Compute-and-set）
    ///
    /// 对应 bcachefs `bch2_journal_space_available()` (reclaim.c:262-358)。
    /// volmount 简化版从 slowpath 实时计算，不做 dirty_idx 推进（已在 reclaim 中处理）。
    pub fn bch2_journal_space_available(&self, watermark: Watermark) -> u64 {
        let (total, clean, clean_ondisk) = self.compute_journal_space();
        match watermark {
            Watermark::Stripe | Watermark::Normal => clean,
            Watermark::CopyGC | Watermark::Btree => clean_ondisk,
            Watermark::BtreeCopyGC | Watermark::Reclaim => clean,
            Watermark::InteriorUpdate => total,
        }
    }

    /// 关闭当前 entry 并尝试轮换到下一个 bucket（slowpath 核心同步操作）
    ///
    /// 对应 bcachefs `bch2_journal_cycle_locked()` (journal.c:636)
    /// bcachefs 的 cycle_locked 通过 flags 控制是否关闭 + 是否打开，volmount 简化版始终关闭再打开。
    ///
    /// 1. 关闭当前 entry
    /// 2. 尝试轮换 bucket（不涉及 I/O，纯索引操作）
    /// 3. 如果轮换成功，打开新 entry，返回 true
    /// 4. 如果无空闲 bucket，返回 false（调用方应重试或触发回收）
    pub fn journal_cycle_locked(&self) -> Result<bool, JournalError> {
        // 1. 关闭当前 entry
        self.journal_entry_close();

        // 2. 尝试轮换
        let mut sp = self.slowpath.lock().unwrap();
        let nr = sp.buckets.len();
        if nr == 0 {
            return Err(JournalError::Overflow("no journal buckets".into()));
        }

        let next = (sp.current_bucket + 1) % nr;
        if next != sp.dirty_idx {
            sp.current_bucket = next;
            sp.current_offset = 0;
            sp.remaining_bytes = BUCKET_BLOCKS * JSET_BLOCK_SIZE;
            drop(sp);
            // 3. 打开新 entry
            self.journal_entry_open();
            self.bch2_journal_wake_up();
            return Ok(true);
        }

        // 4. 无空闲 bucket
        self.bch2_journal_wake_up();
        Ok(false)
    }

    /// slowpath 预留 — 当 fastpath CAS 失败时调用。
    ///
    /// 三级 fallback（对齐 bcachefs `bch2_journal_res_get_slowpath`）：
    /// 1. cycle: `journal_cycle_locked()` — 关闭旧 entry，打开新 bucket
    /// 2. wait: 等待 in_flight buf 写入完成
    /// 3. reclaim: `bch2_journal_flush_pins()` + reclaim — 释放已 pin 空间
    ///
    /// 三级都失败后，返回 `JournalError::Overflow`（不可恢复状态，调用方应 panic）。
    pub fn journal_res_get_slowpath(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        // Phase 1: cycle
        if self.journal_cycle_locked()? {
            if let Ok(res) = self.journal_res_get_fast(watermark, req_u64s) {
                return Ok(res);
            }
        }

        // Phase 2: wait — 自旋等待 inflight 队列清空
        const SPIN_COUNT: u32 = 1024;
        for _ in 0..SPIN_COUNT {
            if self.in_flight.lock().unwrap().is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        if let Ok(res) = self.journal_res_get_fast(watermark, req_u64s) {
            return Ok(res);
        }

        // Phase 3: reclaim — flush pins + advance indices
        // 对应 bcachefs `bch2_journal_reclaim(j, BCH_RECLAIM_DIRECT)` (journal.c:850-880)。
        // 使用 `journal_seq_to_flush()` 而非 `journal_cur_seq()` 精确计算需 flush 的 seq，
        // 避免过度 flush（pin FIFO 半满水位控制）。
        let seq_to_flush = self.journal_seq_to_flush();
        let nr_flushed = self.journal_flush_pins(seq_to_flush)?;
        if nr_flushed > 0 {
            self.nr_direct_reclaim
                .fetch_add(nr_flushed as u64, Ordering::Relaxed);
        }
        self.bch2_journal_update_last_seq();
        self.advance_dirty_idx();
        self.advance_dirty_idx_ondisk();
        if self.journal_cycle_locked()? {
            if let Ok(res) = self.journal_res_get_fast(watermark, req_u64s) {
                return Ok(res);
            }
        }

        Err(JournalError::Overflow(format!(
            "slowpath: no journal space after cycle+wait+reclaim (watermark={:?}, req_u64s={})",
            watermark, req_u64s,
        )))
    }

    /// 公开的 journal reservation 入口 — 尝试 fastpath，失败后自动进入 slowpath
    ///
    /// 对应 bcachefs `bch2_journal_res_get()` (journal.h:521)
    /// bcachefs 在 `__journal_res_get` (journal.c:820) 中处理更多 flags（如 JOURNAL_RES_GET_CHECK），
    /// volmount 的简化版始终进行 fast→slow 两级 fallback。
    ///
    /// 这是推荐的 reservation API：
    /// 1. 先尝试无锁 fastpath（CAS on `JournalResState`）
    /// 2. 如果 fastpath 因空间不足失败，获取序列化锁并通过 slowpath 重试
    ///
    /// # 并发安全性
    ///
    /// - Fastpath 路径：完全无锁，CAS 保护
    /// - Slowpath 路径：通过 `slowpath_lock` 互斥，确保同一时间只有一个线程修改 bucket 状态
    pub fn journal_res_get(
        &self,
        watermark: Watermark,
        req_u64s: u32,
    ) -> Result<JournalRes, JournalError> {
        // 1. 尝试 fastpath
        if let Ok(res) = self.journal_res_get_fast(watermark, req_u64s) {
            return Ok(res);
        }

        // 2. Fastpath 失败 → 获取 slowpath 锁后进入 slowpath
        let _guard = self.slowpath_lock.lock().unwrap();
        self.journal_res_get_slowpath(watermark, req_u64s)
    }

    /// 设置自动 flush 间隔（毫秒）
    ///
    /// 当 interval 为 0 时禁用自动 flush。
    pub fn set_auto_flush_interval(&mut self, ms: u64) {
        self.auto_flush_ms = if ms > 0 { Some(ms) } else { None };
    }

    /// 获取自动 flush 间隔
    pub fn auto_flush_interval(&self) -> Option<u64> {
        self.auto_flush_ms
    }

    /// 启动自动 flush 后台任务
    ///
    /// 当 `auto_flush_ms` 有值时，启动一个 tokio::spawn 循环：
    /// - 每个间隔周期检查 buf 利用率
    /// - 如果 buf 利用率 > 75% 或定时器超时，触发 flush
    ///
    /// # 调用要求
    ///
    /// 调用方需要持有 `Arc<Journal>` 和 `Arc<dyn BlockDevice>`。
    /// 此方法在所有创建 Journal 的场景（daemon、测试）中可选调用。
    pub fn spawn_auto_flush_task(
        self: &Arc<Self>,
        backend: Arc<dyn BlockDevice>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let interval_ms = self.auto_flush_ms?;
        if interval_ms == 0 {
            return None;
        }

        let journal = self.clone();
        let handle = tokio::spawn(async move {
            let mut last_flush_seq: u64 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;

                // 检查是否需要 flush
                let cur_seq = journal.journal_cur_seq();
                if cur_seq == last_flush_seq {
                    continue; // 没有新写入
                }

                // 检查 buf 利用率
                let buf_util = {
                    let res_state = journal.reservations.read();
                    let cur_off = JournalResState::cur_entry_offset(res_state);
                    (cur_off as f64) / (BUF_SIZE_U64S as f64)
                };

                if buf_util > 0.75 || cur_seq > last_flush_seq + 1 {
                    if let Err(e) = journal.bch2_journal_flush(&*backend).await {
                        eprintln!("auto-flush failed: {}", e);
                    }
                    last_flush_seq = journal.journal_cur_seq();
                }
            }
        });
        Some(handle)
    }

    /// 启动后台回收任务
    ///
    /// 定时调 `bch2_journal_reclaim()` 回收可重用的 journal bucket。
    /// 当 `interval_ms` 为 0 时返回 `None`（不启动）。
    ///
    /// # 调用要求
    ///
    /// 调用方需要持有 `Arc<Journal>` 和 `Arc<dyn BlockDevice>`。
    /// 此方法在所有创建 Journal 的场景（daemon、测试）中可选调用。
    pub fn spawn_background_reclaim_task(
        self: &Arc<Self>,
        backend: Arc<dyn BlockDevice>,
        interval_ms: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if interval_ms == 0 {
            return None;
        }
        self.reclaim_interval_ms
            .store(interval_ms, Ordering::Release);

        let journal = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                // bcachefs reclaim_kicked 语义(reclaim.c:1232-1234):
                // 检查 kicked 标志并清除 — 如果被踢醒则跳过本次睡眠
                let kicked = journal.reclaim_kicked.swap(false, Ordering::AcqRel);
                if !kicked {
                    // 正常间隔睡眠
                    tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
                }

                // 执行 reclaim（后台模式，带触发条件循环）
                // 对应 bcachefs reclaim.c:1236-1237:
                //   scoped_guard(mutex, &j->reclaim_lock)
                //       ret = __bch2_journal_reclaim(j, false, kicked);
                //
                // direct=false 允许 __bch2_journal_reclaim 在触发条件满足时
                // 多次循环 flush pins（时间触发 + 空间触发），
                // 直到触发条件不满足或无待 flush 的 pin。
                if let Err(e) = journal.__bch2_journal_reclaim(&*backend, false).await {
                    eprintln!("background reclaim failed: {}", e);
                }
            }
        });
        Some(handle)
    }
}

// ═══════════════════════════════════════════════════════════
// Part 7: Blacklist helpers (unchanged)
// ═══════════════════════════════════════════════════════════

/// 从 Jset 列表中提取所有 blacklist entries
pub fn extract_blacklist_entries(jsets: &[Jset]) -> Vec<BlacklistEntry> {
    let mut entries = Vec::new();
    for jset in jsets {
        for entry in &jset.entries {
            if entry.hdr.entry_type == JsetEntryType::Blacklist as u8 {
                if let Ok(blacklist) = bincode::deserialize::<Vec<BlacklistEntry>>(&entry.payload) {
                    entries.extend(blacklist);
                }
            }
        }
    }
    entries
}

// ═══════════════════════════════════════════════════════════
// Part 7: Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
    use crate::types::BlockAddr;
    use std::sync::atomic::Ordering;

    /// 测试辅助：获取 slowpath 中 bucket 字段的引用
    fn sp_buckets_len(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().buckets.len()
    }

    fn sp_current_bucket(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().current_bucket
    }

    fn sp_current_offset(j: &Journal) -> u32 {
        j.slowpath.lock().unwrap().current_offset
    }

    fn sp_remaining_bytes(j: &Journal) -> u32 {
        j.slowpath.lock().unwrap().remaining_bytes
    }

    fn sp_bucket_seq(j: &Journal) -> Vec<u64> {
        j.slowpath.lock().unwrap().bucket_seq.clone()
    }

    fn sp_discard_idx(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().discard_idx
    }

    fn sp_dirty_idx(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().dirty_idx
    }

    fn sp_dirty_idx_ondisk(j: &Journal) -> usize {
        j.slowpath.lock().unwrap().dirty_idx_ondisk
    }

    fn make_test_entry() -> BtreeEntry {
        BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1),
        )
    }

    // ── JournalResState unit tests ──

    #[test]
    fn test_res_state_initial() {
        let rs = JournalResState::new();
        // bcachefs: 初始状态 cur_entry_offset = JOURNAL_ENTRY_CLOSED_VAL，idx = 0
        assert_eq!(rs.read(), JOURNAL_ENTRY_CLOSED_VAL);
        assert!(rs.is_closed());
        assert_eq!(
            JournalResState::cur_entry_offset(JOURNAL_ENTRY_CLOSED_VAL),
            JOURNAL_ENTRY_CLOSED_VAL as u32
        );
        assert_eq!(JournalResState::idx(JOURNAL_ENTRY_CLOSED_VAL), 0);
        assert_eq!(JournalResState::buf_count(JOURNAL_ENTRY_CLOSED_VAL, 0), 0);
    }

    #[test]
    fn test_res_state_try_reserve_basic() {
        let rs = JournalResState::new();
        // bcachefs: 必须先 open entry 才能做 reservation
        rs.open_entry(0);
        // Reserve 10 u64s
        let (old, new) = rs.try_reserve(10).unwrap();
        assert_eq!(JournalResState::cur_entry_offset(old), 0);
        assert_eq!(JournalResState::cur_entry_offset(new), 10);
        assert_eq!(JournalResState::buf_count(new, 0), 1); // buf0_count incremented
    }

    #[test]
    fn test_res_state_try_reserve_multiple() {
        let rs = JournalResState::new();
        rs.open_entry(0);
        rs.try_reserve(10).unwrap();
        rs.try_reserve(20).unwrap();
        let v = rs.read();
        assert_eq!(JournalResState::cur_entry_offset(v), 30);
        assert_eq!(JournalResState::buf_count(v, 0), 2);
    }

    #[test]
    fn test_res_state_release() {
        let rs = JournalResState::new();
        rs.open_entry(0);
        rs.try_reserve(10).unwrap();
        let v = rs.read();
        assert_eq!(JournalResState::buf_count(v, 0), 1);

        let old_v = rs.release(0);
        let count_before = (old_v >> BUF0_COUNT_SHIFT) & BUF_COUNT_MAX;
        assert_eq!(count_before, 1); // was 1 before decrement
    }

    #[test]
    fn test_res_state_close_open() {
        let rs = JournalResState::new();
        // bcachefs: 初始状态即为 closed
        assert!(rs.is_closed());

        // open → close → open cycle
        rs.open_entry(1);
        assert!(!rs.is_closed());
        let v = rs.read();
        assert_eq!(JournalResState::idx(v), 1);
        assert_eq!(JournalResState::cur_entry_offset(v), 0);

        rs.close_entry();
        assert!(rs.is_closed());
    }

    #[test]
    fn test_res_state_open_entry_clears_count() {
        let rs = JournalResState::new();
        rs.open_entry(0); // 先打开 entry 0
        rs.try_reserve(5).unwrap(); // buf0_count=1, idx=0
        rs.close_entry();
        rs.open_entry(1); // buf1 opens

        let v = rs.read();
        assert_eq!(JournalResState::idx(v), 1);
        assert_eq!(JournalResState::buf_count(v, 0), 1); // buf0 count preserved
        assert_eq!(JournalResState::buf_count(v, 1), 0); // buf1 count cleared
    }

    // ── Journal constructor tests (updated) ──

    #[test]
    fn test_journal_new() {
        let addrs = vec![100, 200, 300];
        let journal = Journal::new(addrs.clone());
        let addrs_out: Vec<u64> = {
            let sp = journal.slowpath.lock().unwrap();
            sp.buckets.iter().map(|bs| bs.addr).collect()
        };
        assert_eq!(addrs_out, addrs);
        assert_eq!(journal.journal_cur_seq(), 2); // new() opens first entry → fetch_add 1→2
        assert_eq!(sp_current_bucket(&journal), 0);
        assert_eq!(
            sp_remaining_bytes(&journal),
            BUCKET_BLOCKS * JSET_BLOCK_SIZE
        );
        assert_eq!(sp_bucket_seq(&journal).len(), 3);
        assert_eq!(sp_bucket_seq(&journal), vec![0, 0, 0]);
    }

    #[test]
    fn test_journal_from_superblock() {
        let state = JournalSuperblockState {
            bucket_addrs: vec![100, 200, 300],
            last_seq: 42,
            last_seq_ondisk: 40,
            last_bucket: 1,
            discard_idx: 0,
            dirty_idx: 0,
            dirty_idx_ondisk: 0,
            bucket_seq: vec![10, 20, 30],
            replayed_seqs: vec![],
        };
        let journal = Journal::from_superblock(&state);
        let addrs_out: Vec<u64> = {
            let sp = journal.slowpath.lock().unwrap();
            sp.buckets.iter().map(|bs| bs.addr).collect()
        };
        assert_eq!(addrs_out, vec![100, 200, 300]);
        assert_eq!(journal.journal_cur_seq(), 43); // from_superblock opens first entry → fetch_add 42→43
        assert_eq!(journal.last_seq_ondisk.load(Ordering::Acquire), 40);
        assert_eq!(sp_current_bucket(&journal), 1);
        assert_eq!(sp_bucket_seq(&journal), vec![10, 20, 30]);
    }

    #[test]
    fn test_journal_seq_increment() {
        let journal = Journal::new(vec![100]);
        // new() calls journal_entry_open → seq fetch_add 1 from 1 to 2, returns 1
        assert_eq!(journal.journal_cur_seq(), 2);
        journal.seq.fetch_add(1, Ordering::Relaxed);
        assert_eq!(journal.journal_cur_seq(), 3);
        journal.seq.fetch_add(5, Ordering::Relaxed);
        assert_eq!(journal.journal_cur_seq(), 8);
    }

    #[tokio::test]
    async fn test_journal_append_seq_increment() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        let entry = make_test_entry();

        let seq1 = journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&entry),
                false,
                &backend,
            )
            .await
            .unwrap();
        // seq=1 from first entry opened in new(), first append uses same buf
        assert_eq!(seq1, 1);

        let seq2 = journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&entry),
                false,
                &backend,
            )
            .await
            .unwrap();
        // Same entry, same seq (per-entry, not per-reservation)
        assert_eq!(seq2, 1);

        // Flush to cycle entry, then new append gets new seq
        // (flush creates a new entry, so seq advances)
    }

    #[tokio::test]
    async fn test_journal_append_btree_root() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100]);
        let seq = journal
            .append_btree_root(BtreeId::Extents, 0xABCD, false, &backend)
            .await
            .unwrap();
        assert_eq!(seq, 1);
    }

    #[tokio::test]
    async fn test_journal_flush_readback() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        let entry = make_test_entry();

        // Wait, journal.new() already opens the first entry.
        // append → reserve + commit + put on buf[0]
        journal
            .append(BtreeId::Extents, &[entry], false, &backend)
            .await
            .unwrap();

        // flush → close entry, write bufs to bucket, open new entry
        journal.bch2_journal_flush(&backend).await.unwrap();
        // new entry opened → seq advances (fetch_add 2→3)
        assert_eq!(journal.journal_cur_seq(), 3);

        // Read back the block from bucket
        let block_addr = BlockAddr::new(100);
        let mut buf = vec![0u8; JSET_BLOCK_SIZE as usize];
        backend.read_block(block_addr, &mut buf).await.unwrap();

        let restored = Jset::deserialize(&buf).unwrap().unwrap();
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].hdr.btree_type, 0); // Extents
    }

    #[tokio::test]
    async fn test_journal_read() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100]);
        let entry = make_test_entry();

        journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&entry),
                false,
                &backend,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush(&backend).await.unwrap();

        // Read back
        let jsets = journal.bch2_journal_read(&backend, 0).await.unwrap();
        // After flush, the Jset data is in the bucket
        // Each append creates one Jset; flush writes all buf data to bucket
        // The bucket may contain one or more Jset blocks depending on buf data size
        assert!(!jsets.is_empty(), "should have at least one Jset");
    }

    #[tokio::test]
    async fn test_journal_entries_read() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 500]);
        let entry = make_test_entry();

        // bucket 0: write 1 entry
        journal
            .append(
                BtreeId::Extents,
                std::slice::from_ref(&entry),
                false,
                &backend,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush(&backend).await.unwrap();

        // rotate to bucket 1
        journal
            .bch2_journal_rotate_or_reclaim(&backend)
            .await
            .unwrap();

        // bucket 1: write 1 entry
        journal
            .append(BtreeId::Alloc, &[entry], false, &backend)
            .await
            .unwrap();
        journal.bch2_journal_flush(&backend).await.unwrap();

        let all = journal.bch2_journal_entries_read(&backend).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_journal_utilization() {
        let mut journal = Journal::new(vec![100, 200]);
        assert_eq!(journal.utilization(), 0.0);

        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) / 2;
            sp.remaining_bytes = (BUCKET_BLOCKS * JSET_BLOCK_SIZE) / 2;
        }
        let u = journal.utilization();
        assert!(u > 0.24 && u < 0.26);
    }

    #[tokio::test]
    async fn test_journal_rotate_or_reclaim() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        assert_eq!(sp_current_bucket(&journal), 0);

        // Fill bucket 0
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
        }
        journal.seq.store(100, Ordering::Relaxed);

        journal
            .bch2_journal_rotate_or_reclaim(&backend)
            .await
            .unwrap();
        assert_eq!(sp_current_bucket(&journal), 1);
        assert_eq!(sp_current_offset(&journal), 0);
    }

    #[tokio::test]
    async fn test_journal_ring_full_overflow() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);
        let _entry = make_test_entry();

        // Fill bucket 0 → rotate to bucket 1
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
            sp.bucket_seq[0] = 10;
        }
        journal.seq.store(10, Ordering::Relaxed);
        journal
            .bch2_journal_rotate_or_reclaim(&backend)
            .await
            .unwrap();
        assert_eq!(sp_current_bucket(&journal), 1);

        // Fill bucket 1 → can't rotate back (dirty_idx=0 not advanced) → Overflow
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.current_offset = BUCKET_BLOCKS * JSET_BLOCK_SIZE - OVERFLOW_MARGIN;
            sp.remaining_bytes = OVERFLOW_MARGIN - 1;
        }
        let result = journal.bch2_journal_rotate_or_reclaim(&backend).await;
        assert!(result.is_err());
        match result {
            Err(JournalError::Overflow(msg)) => assert!(msg.contains("exhausted")),
            _ => panic!("expected Overflow"),
        }
    }

    #[test]
    fn test_journal_bucket_seq_initialization() {
        let journal = Journal::new(vec![100, 200, 300]);
        assert_eq!(sp_bucket_seq(&journal), vec![0, 0, 0]);
        assert_eq!(sp_discard_idx(&journal), 0);
        assert_eq!(sp_dirty_idx(&journal), 0);
        assert_eq!(sp_dirty_idx_ondisk(&journal), 0);
        // new() 调用 journal_entry_open 推入 1 个自钉
        assert_eq!(unsafe { (*journal.pin_fifo.get()).len() }, 1);
        assert_eq!(journal.flushed_seq_marker.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_journal_to_superblock_state() {
        let journal = Journal::new(vec![100, 200]);
        let state = journal.to_superblock_state();
        assert_eq!(state.bucket_addrs, vec![100, 200]);
        // journal_cur_seq() reads AtomicU64 which is 2 after new() opens entry
        assert_eq!(state.last_seq, 2);
        assert_eq!(state.last_seq_ondisk, 0);
    }

    #[test]
    fn test_journal_advance_dirty_idx() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10, 0];
            sp.current_bucket = 2;
        }
        journal.last_seq_ondisk.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 1);
    }

    #[test]
    fn test_journal_advance_dirty_idx_ignores_open_seq() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10, 0];
            sp.current_bucket = 2;
        }
        journal.seq.store(20, Ordering::Relaxed);
        journal.last_seq_ondisk.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 1);
    }

    #[test]
    fn test_journal_no_advance_when_dirty_idx_equals_boundary() {
        let mut journal = Journal::new(vec![100, 200]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10];
            sp.current_bucket = 0;
        }
        journal.last_seq_ondisk.store(4, Ordering::Relaxed);

        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 0);
    }

    #[test]
    fn test_journal_advance_dirty_idx_wraparound() {
        let mut journal = Journal::new(vec![100, 200, 300]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![20, 5, 15];
            sp.current_bucket = 0;
            sp.dirty_idx = 1;
        }
        journal.last_seq_ondisk.store(18, Ordering::Relaxed);

        journal.advance_dirty_idx();
        journal.advance_dirty_idx();
        assert_eq!(sp_dirty_idx(&journal), 0);
    }

    #[test]
    fn test_journal_advance_dirty_idx_ondisk() {
        let mut journal = Journal::new(vec![100, 200]);
        {
            let sp = journal.slowpath.get_mut().unwrap();
            sp.bucket_seq = vec![5, 10];
            sp.dirty_idx = 2;
        }
        journal.seq.store(20, Ordering::Relaxed);
        journal.last_seq_ondisk.store(8, Ordering::Relaxed);

        journal.advance_dirty_idx_ondisk();
        assert_eq!(sp_dirty_idx_ondisk(&journal), 1);
    }

    // ── bch2_journal_seq_blacklist_add / extract_blacklist_entries tests ──

    #[tokio::test]
    async fn test_journal_seq_blacklist_add() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);

        let seq = journal
            .bch2_journal_seq_blacklist_add(1, 10, &backend)
            .await
            .unwrap();
        assert_eq!(seq, 1);
        // new() opens entry → buf.seq=1, cur_seq=2; flush() opens new entry → cur_seq=3
        assert_eq!(journal.journal_cur_seq(), 3);

        // Verify readback
        let jsets = journal.bch2_journal_read(&backend, 0).await.unwrap();
        assert!(!jsets.is_empty(), "should have blacklist jset");
    }

    #[tokio::test]
    async fn test_journal_seq_blacklist_add_range() {
        let backend = MockBlockDevice::new();
        let mut journal = Journal::new(vec![100, 200]);

        let seq = journal
            .bch2_journal_seq_blacklist_add(5, 42, &backend)
            .await
            .unwrap();
        assert_eq!(seq, 1);

        // Read back and extract blacklist
        let jsets = journal.bch2_journal_read(&backend, 0).await.unwrap();
        let bl = super::extract_blacklist_entries(&jsets);
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].start_seq, 5);
        assert_eq!(bl[0].end_seq, 42);
    }

    #[test]
    fn test_extract_blacklist_entries_empty() {
        let jsets: Vec<Jset> = vec![];
        let bl = super::extract_blacklist_entries(&jsets);
        assert!(bl.is_empty());
    }

    #[test]
    fn test_extract_blacklist_entries_skips_non_blacklist() {
        // Build Jset structs directly (not through Journal.append which uses buf)
        let entry1 = RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, vec![]).unwrap();
        let entry2 = RawJsetEntry::new(0, JsetEntryType::BtreeRoot as u8, vec![]).unwrap();
        let jsets = vec![
            Jset {
                header: JsetHeader {
                    magic: super::super::jset::JOURNAL_MAGIC,
                    seq: 1,
                    last_seq: 0,
                    crc32: 0,
                    entry_count: 1,
                    version: 0,
                    csum_type: 0,
                    pad: [0u8; 27],
                },
                entries: vec![entry1],
            },
            Jset {
                header: JsetHeader {
                    magic: super::super::jset::JOURNAL_MAGIC,
                    seq: 2,
                    last_seq: 0,
                    crc32: 0,
                    entry_count: 1,
                    version: 0,
                    csum_type: 0,
                    pad: [0u8; 27],
                },
                entries: vec![entry2],
            },
        ];
        let bl = super::extract_blacklist_entries(&jsets);
        assert!(bl.is_empty(), "non-blacklist entries should be ignored");
    }

    // ── Journal P2: must_flush + background reclaim tests ──

    #[tokio::test]
    async fn test_must_flush_flag() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        let entry = make_test_entry();

        // must_flush=true 时 append 应正常完成
        let result = journal
            .append(BtreeId::Extents, &[entry], true, &backend)
            .await;
        assert!(result.is_ok(), "append with must_flush=true should succeed");
        let seq = result.unwrap();
        assert!(seq > 0, "seq should be non-zero");
    }

    #[tokio::test]
    async fn test_must_flush_default_false() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        let entry = make_test_entry();

        // must_flush=false 时 append 也正常完成
        let result = journal
            .append(BtreeId::Extents, &[entry], false, &backend)
            .await;
        assert!(
            result.is_ok(),
            "append with must_flush=false should succeed"
        );
        let seq = result.unwrap();
        assert!(seq > 0, "seq should be non-zero");
    }

    #[tokio::test]
    async fn test_must_flush_propagation() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100, 200]));
        let entry = make_test_entry();

        // 使用 must_flush=true 调用 append
        journal
            .append(BtreeId::Extents, &[entry], true, &backend)
            .await
            .unwrap();

        // flush 后检查 buf 的 has_must_flush 标记在 write_bufs_to_bucket 中被正确处理
        // 只需验证 flush 不报错
        journal.bch2_journal_flush(&backend).await.unwrap();
    }

    #[tokio::test]
    async fn test_must_flush_btree_root() {
        let backend = MockBlockDevice::new();
        let journal = Arc::new(Journal::new(vec![100]));

        // append_btree_root 也支持 must_flush
        let seq = journal
            .append_btree_root(BtreeId::Extents, 0xABCD, true, &backend)
            .await
            .unwrap();
        assert!(seq > 0, "seq should be non-zero");

        // flush 以确认数据落盘
        journal.bch2_journal_flush(&backend).await.unwrap();
    }

    #[tokio::test]
    async fn test_background_reclaim_task() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Arc::new(Journal::new(vec![100, 200, 300]));

        // interval=0 时不应启动
        let handle = Journal::spawn_background_reclaim_task(&journal, backend.clone(), 0);
        assert!(handle.is_none(), "interval=0 should return None");

        // interval>0 时应启动
        let handle = Journal::spawn_background_reclaim_task(&journal, backend, 1000);
        assert!(handle.is_some(), "interval>0 should return Some(handle)");
        // 取消后台任务
        if let Some(h) = handle {
            h.abort();
        }
    }
}
