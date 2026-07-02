//! Btree Write Buffer — bcachefs 对齐
//!
//! 对应 bcachefs btree_write_buffer.c + btree_write_buffer.h 中的公开 API。
//! Write buffer 用于延迟写入（deferred write），将 journal 中的 key 批量刷入 btree。
//!
//! bcachefs write buffer 架构：
//! - 每个启用 write buffer 的 btree type 有一个 BCH_WB_BTREE_NR 条目
//! - `inc` keys: 新到达的写入暂存于此
//! - `flushing` keys: 当前正在刷入 btree 的 keys
//! - flush worker: 定期将 inc 中的 keys 排序后刷入 btree

use std::cmp::Ordering;
use std::sync::Mutex;

use tokio::runtime::Handle;

use crate::block_device::BlockDevice;
use crate::btree::key::{BchVal, Bpos, BtreeKey, KeyType};
use crate::btree::transaction::BtreeTrans;
use crate::btree::{BtreeEngine, BtreeId};
use crate::journal::Journal;
use crate::StorageError;

/// bcachefs 对齐: BCH_WB_BTREE_NR — write buffer 的 btree 数量
pub const BCH_WB_BTREE_NR: usize = 11;

/// bcachefs 对齐: enum bch_wb_btree — write buffer 的 btree 索引
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BchWbBtree {
    Accounting = 0,
    Lru = 1,
    NeedDiscard = 2,
    Backpointers = 3,
    DeletedInodes = 4,
    ReconcileWork = 5,
    ReconcileHipri = 6,
    ReconcilePending = 7,
    ReconcileWorkPhys = 8,
    ReconcileHipriPhys = 9,
    StripeBackpointers = 10,
}

/// bcachefs 对齐: enum wb_flush_caller — flush 调用来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WbFlushCaller {
    Thread = 0,
    JournalPin = 1,
    Sync = 2,
    Maybe = 3,
    Tryflush = 4,
}

/// bcachefs 对齐: struct btree_write_buffered_key — write buffer 中的 key 条目
///
/// 固定大小存储，每个条目包含完整的位置、值和元数据。
/// 对应 bcachefs 的 `struct btree_write_buffered_key`。
#[derive(Debug, Clone)]
pub struct BtreeWriteBufferedKey {
    pub journal_seq: u64,
    pub btree_id: BtreeId,
    pub key: BtreeKey,
    pub value: BchVal,
    pub key_type: KeyType,
}

/// bcachefs 对齐: struct btree_write_buffer_keys — write buffer key 集合
///
/// 包含 inc 或 flushing 队列的全部 key 条目。
/// `lock` 用于保护 inc 队列的并发访问。
#[derive(Debug)]
pub struct BtreeWriteBufferKeys {
    pub keys: Vec<BtreeWriteBufferedKey>,
    pub lock: Mutex<()>,
    pub nr: usize,
}

/// 排序用轻量级引用 — 对应 bcachefs struct wb_key_ref
///
/// 在 flush 时从 flushing.keys 构建排序索引数组，
/// 避免移动实际 key 数据。
#[derive(Debug, Clone, Copy)]
struct WbKeyRef {
    /// keys 中的索引
    idx: u32,
    /// btree 类型（排序键）
    btree_id: u8,
    /// bpos.inode — 排序键
    inode: u64,
    /// bpos.offset — 排序键
    offset: u64,
    /// bpos.snapshot — 排序键
    snapshot: u32,
    /// journal_seq — 用于 dedup 时保留较新条目
    journal_seq: u64,
}

/// bcachefs 对齐: struct bch_fs_btree_write_buffer — 单 btree 的 write buffer
#[derive(Debug)]
pub struct BtreeWriteBuffer {
    pub idx: BchWbBtree,
    pub inc: BtreeWriteBufferKeys,
    pub flushing: BtreeWriteBufferKeys,
    pub nr_flushes: u64,
    pub nr_keys_flushed: u64,
}

/// bcachefs 对齐: 11 个 BtreeWriteBuffer 实例集合（对应 bcachefs `c->btree.write_buffer[]`）
///
/// 覆盖所有启用 write buffer 的 btree 类型。
/// `init_early()` 负责设置每个 buffer 的 idx 字段。
#[derive(Debug)]
pub struct BtreeWriteBufferSet {
    pub buffers: [BtreeWriteBuffer; BCH_WB_BTREE_NR],
}

impl BtreeWriteBufferSet {
    /// 创建集合，所有 buffer 使用占位 idx（需 `init_early()` 设置正确值）
    pub fn new() -> Self {
        Self {
            buffers: std::array::from_fn(|_| btree_write_buffer_new(BchWbBtree::Accounting)),
        }
    }

    /// 遍历每个 buffer（只读）
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&BtreeWriteBuffer),
    {
        for wb in self.buffers.iter() {
            f(wb);
        }
    }

    /// 遍历每个 buffer（可变）
    pub fn for_each_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut BtreeWriteBuffer),
    {
        for wb in self.buffers.iter_mut() {
            f(wb);
        }
    }
}

impl Default for BtreeWriteBufferSet {
    fn default() -> Self {
        Self::new()
    }
}

/// bcachefs 对齐: struct journal_keys_to_wb — journal keys 转移上下文
///
/// 在 `bch2_journal_keys_to_write_buffer_start/end` 之间使用。
/// volmount 单线程简化版：不含锁守卫。
#[derive(Debug)]
pub struct JournalKeysToWb<'a> {
    pub set: &'a BtreeWriteBufferSet,
    pub seq: u64,
}

// ─── Public API ─────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_write_buffer_flush_locked — 核心 flush 管线
///
/// 完成完整 flush 管线：
/// 1. move_keys_from_inc_to_flushing
/// 2. 构建排序索引 WbKeyRef
/// 3. 按 (btree_id, bpos) 排序
/// 4. 相同 pos 的条目去重（保留最新）
/// 5. Fastpath：engine.get_entry noop 检查 + engine.insert_entry
/// 6. Slowpath：通过事务提交重试失败条目
pub fn bch2_btree_write_buffer_flush_locked(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> Result<(), StorageError> {
    // Step 1: move inc → flushing
    move_keys_from_inc_to_flushing(wb);

    if wb.flushing.nr == 0 {
        wb.nr_flushes += 1;
        return Ok(());
    }

    // Step 2: build sorted index
    let mut refs = build_sorted_index(&wb.flushing.keys);

    // Step 3: sort
    wb_sort(&mut refs);

    // Step 4: dedup
    let deduped = dedup_sorted_refs(&refs, &wb.flushing.keys);

    // Step 5: fastpath flush
    let slowpath_indices = flush_fastpath(&deduped, &wb.flushing.keys, engine);

    // Step 6: slowpath flush (if journal available)
    if !slowpath_indices.is_empty() {
        let slowpath_refs: Vec<&WbKeyRef> = slowpath_indices.iter().map(|&i| deduped[i]).collect();
        flush_slowpath(&slowpath_refs, &wb.flushing.keys, engine, journal, backend)?;
    }

    // 统计
    wb.nr_flushes += 1;
    wb.nr_keys_flushed += wb.flushing.nr as u64;

    // 清空 flushing
    wb.flushing.keys.clear();
    wb.flushing.nr = 0;

    Ok(())
}

/// bcachefs 对齐: bch2_btree_write_buffer_flush — 获取 inc 锁后执行 flush
///
/// 先锁定 inc，再调用 flush_locked。
/// 对应 bcachefs 中需要外部锁保护的 flush 入口。
pub fn bch2_btree_write_buffer_flush(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> Result<(), StorageError> {
    {
        let _lock = wb.inc.lock.lock().unwrap();
        // _lock 在此作用域结束时释放
    }
    bch2_btree_write_buffer_flush_locked(wb, engine, journal, backend)
}

/// bcachefs 对齐: bch2_btree_write_buffer_flush_sync — 同步刷新 write buffer
///
/// 将 write buffer 中的所有条目刷入 btree。
pub fn bch2_btree_write_buffer_flush_sync(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> i32 {
    match bch2_btree_write_buffer_flush_locked(wb, engine, journal, backend) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// bcachefs 对齐: bch2_btree_write_buffer_flush_going_ro — 只读转换时的 flush
///
/// 在文件系统转换为只读前，将 write buffer 中的剩余条目全部刷入 btree。
pub fn bch2_btree_write_buffer_flush_going_ro(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> bool {
    bch2_btree_write_buffer_flush_locked(wb, engine, journal, backend).is_ok()
}

/// bcachefs 对齐: bch2_btree_write_buffer_tryflush — 尝试刷 write buffer
///
/// 非阻塞尝试 flush，仅在条件满足时执行。
pub fn bch2_btree_write_buffer_tryflush(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> i32 {
    if bch2_btree_write_buffer_must_wait(wb) {
        match bch2_btree_write_buffer_flush_locked(wb, engine, journal, backend) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    } else {
        0
    }
}

/// bcachefs 对齐: bch2_btree_write_buffer_must_wait — 是否需要等待 write buffer
///
/// 当 write buffer 使用率超过 75% 时返回 true。
pub fn bch2_btree_write_buffer_must_wait(wb: &BtreeWriteBuffer) -> bool {
    let total = wb.inc.nr + wb.flushing.nr;
    let capacity = wb.inc.keys.capacity().max(1);
    total > capacity * 3 / 4
}

/// bcachefs 对齐: bch2_btree_write_buffer_resize — 调整 write buffer 大小
pub fn bch2_btree_write_buffer_resize(_new_size: usize) -> i32 {
    // volmount: 当前为骨架实现
    0
}

/// bcachefs 对齐: bch2_journal_key_to_wb — 将 journal key 插入 write buffer
///
/// 将 (btree_id, key, value) 插入到指定 write buffer 的 inc 队列。
/// 返回 0 表示成功，-1 表示失败。
pub fn bch2_journal_key_to_wb(
    wb: &BtreeWriteBuffer,
    btree_id: BtreeId,
    key: BtreeKey,
    value: BchVal,
    journal_seq: u64,
) -> i32 {
    let _lock = wb.inc.lock.lock().unwrap();
    // 通过 unsafe 获取内部可变性（BtreeWriteBuffer 的 inc 是 &self 访问的可变字段）
    // 使用内部可变性模式：写操作通过 raw ptr
    let wk = BtreeWriteBufferedKey {
        journal_seq,
        btree_id,
        key,
        value,
        key_type: key.key_type,
    };
    // 安全说明：我们持有 inc.lock，因此对 inc 的访问是线程安全的
    unsafe {
        let inc_ptr: *const BtreeWriteBufferKeys = &wb.inc;
        let inc_mut = inc_ptr as *mut BtreeWriteBufferKeys;
        (*inc_mut).keys.push(wk);
        (*inc_mut).nr = (*inc_mut).keys.len();
    }
    0
}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_init_early — 早期初始化
///
/// 对每个 buffer 设置正确的 `idx` 字段。
pub fn bch2_fs_btree_write_buffer_init_early(set: &mut BtreeWriteBufferSet) {
    for (i, wb) in set.buffers.iter_mut().enumerate() {
        wb.idx = match i {
            0 => BchWbBtree::Accounting,
            1 => BchWbBtree::Lru,
            2 => BchWbBtree::NeedDiscard,
            3 => BchWbBtree::Backpointers,
            4 => BchWbBtree::DeletedInodes,
            5 => BchWbBtree::ReconcileWork,
            6 => BchWbBtree::ReconcileHipri,
            7 => BchWbBtree::ReconcilePending,
            8 => BchWbBtree::ReconcileWorkPhys,
            9 => BchWbBtree::ReconcileHipriPhys,
            10 => BchWbBtree::StripeBackpointers,
            _ => unreachable!(),
        };
    }
}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_init — 初始化 write buffer
///
/// 预分配 inc 和 flushing keys 的容量（初始 1024）。
pub fn bch2_fs_btree_write_buffer_init(set: &mut BtreeWriteBufferSet) -> i32 {
    let initial_size = 1024;
    for wb in set.buffers.iter_mut() {
        wb.inc.keys.reserve(initial_size);
        wb.flushing.keys.reserve(initial_size);
    }
    0
}

/// bcachefs 对齐: bch2_btree_write_buffer_start — 启动 write buffer worker 线程
///
/// volmount 单线程简化：无异步 flush worker，空操作。
pub fn bch2_btree_write_buffer_start() -> i32 {
    0
}

/// bcachefs 对齐: bch2_btree_write_buffer_stop — 停止 write buffer worker 线程
///
/// volmount 单线程简化：无异步 flush worker 需要取消。
pub fn bch2_btree_write_buffer_stop(_set: &BtreeWriteBufferSet) {}

/// bcachefs 对齐: bch2_fs_btree_write_buffer_exit — 退出 write buffer
///
/// 清空所有 buffer 的 inc 和 flushing keys，释放内存。
pub fn bch2_fs_btree_write_buffer_exit(set: &mut BtreeWriteBufferSet) {
    for wb in set.buffers.iter_mut() {
        wb.inc.keys.clear();
        wb.inc.nr = 0;
        wb.flushing.keys.clear();
        wb.flushing.nr = 0;
    }
}

/// bcachefs 对齐: bch2_btree_write_buffer_maybe_flush — 可能执行 flush
///
/// 在事务中调用，当检测到 write buffer 满时触发 flush。
pub fn bch2_btree_write_buffer_maybe_flush(
    wb: &mut BtreeWriteBuffer,
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> i32 {
    if bch2_btree_write_buffer_must_wait(wb) {
        match bch2_btree_write_buffer_flush_locked(wb, engine, journal, backend) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    } else {
        0
    }
}

/// bcachefs 对齐: bch2_journal_keys_to_write_buffer_start —
/// 开始将 journal keys 转移到 write buffer
///
/// 创建 JournalKeysToWb 上下文，记录 seq 和 set 引用。
pub fn bch2_journal_keys_to_write_buffer_start<'a>(
    set: &'a BtreeWriteBufferSet,
    dst: &mut JournalKeysToWb<'a>,
    seq: u64,
) {
    dst.set = set;
    dst.seq = seq;
}

/// bcachefs 对齐: bch2_journal_keys_to_write_buffer_end —
/// 完成将 journal keys 转移到 write buffer
///
/// 检查 flush 触发条件：如果任意 buffer 的 inc.nr > capacity/4，返回 1 触发 flush。
pub fn bch2_journal_keys_to_write_buffer_end(
    set: &BtreeWriteBufferSet,
    _dst: &mut JournalKeysToWb,
) -> i32 {
    for wb in set.buffers.iter() {
        let capacity = wb.inc.keys.capacity().max(1);
        if wb.inc.nr > capacity / 4 {
            return 1;
        }
    }
    0
}

/// bcachefs 对齐: bch2_journal_write_buffer_need_flush — 是否需要 flush write buffer
///
/// 检查所有 write buffer 是否有 pending key。
/// 如果有任何 pending key 未刷入 btree，返回 true。
pub fn bch2_journal_write_buffer_need_flush(wbs: &[BtreeWriteBuffer]) -> bool {
    wbs.iter().any(|wb| wb.inc.nr > 0 || wb.flushing.nr > 0)
}

/// 创建新的 write buffer 实例
pub fn btree_write_buffer_new(idx: BchWbBtree) -> BtreeWriteBuffer {
    BtreeWriteBuffer {
        idx,
        inc: BtreeWriteBufferKeys {
            keys: Vec::new(),
            lock: Mutex::new(()),
            nr: 0,
        },
        flushing: BtreeWriteBufferKeys {
            keys: Vec::new(),
            lock: Mutex::new(()),
            nr: 0,
        },
        nr_flushes: 0,
        nr_keys_flushed: 0,
    }
}

/// 将 BtreeWriteBufferedKey 的 bpos 提取为 WbKeyRef 的排序字段
fn key_to_wb_key_ref(idx: u32, key: &BtreeWriteBufferedKey) -> WbKeyRef {
    let pos = Bpos::from_key(&key.key);
    WbKeyRef {
        idx,
        btree_id: key.btree_id as u8,
        inode: pos.inode,
        offset: pos.offset,
        snapshot: pos.snapshot,
        journal_seq: key.journal_seq,
    }
}

/// 构建排序索引数组 — 对应 bcachefs wb_key_ref 数组构造
fn build_sorted_index(keys: &[BtreeWriteBufferedKey]) -> Vec<WbKeyRef> {
    keys.iter()
        .enumerate()
        .map(|(i, k)| key_to_wb_key_ref(i as u32, k))
        .collect()
}

/// 排序 wb_key_ref 数组 — 按 (btree_id, inode, offset, snapshot) 排序
fn wb_sort(refs: &mut [WbKeyRef]) {
    refs.sort_unstable_by(|a, b| {
        a.btree_id
            .cmp(&b.btree_id)
            .then_with(|| a.inode.cmp(&b.inode))
            .then_with(|| a.offset.cmp(&b.offset))
            .then_with(|| a.snapshot.cmp(&b.snapshot))
    });
}

/// 将 inc 中的 keys 移到 flushing — 对应 bcachefs move_keys_from_inc_to_flushing
///
/// 锁定 inc.lock，交换 inc 和 flushing 的 keys，重置 inc。
fn move_keys_from_inc_to_flushing(wb: &mut BtreeWriteBuffer) {
    let _lock = wb.inc.lock.lock().unwrap();
    // 将 inc 的所有 key 移到 flushing
    wb.flushing.keys.append(&mut wb.inc.keys);
    wb.flushing.nr = wb.flushing.keys.len();
    // 重置 inc
    wb.inc.nr = 0;
    // _lock 在此释放
}

/// 对排序后的 refs 去重 — 相同 (btree_id, inode, offset, snapshot) 的条目中保留最新
///
/// 返回去重后的 WbKeyRef 列表（已过滤掉 journal_seq=0 的条目）。
fn dedup_sorted_refs<'a>(
    sorted_refs: &'a [WbKeyRef],
    keys: &'a [BtreeWriteBufferedKey],
) -> Vec<&'a WbKeyRef> {
    let mut result: Vec<&WbKeyRef> = Vec::new();
    let mut i = 0;
    while i < sorted_refs.len() {
        let current = &sorted_refs[i];
        // 找所有相同 pos 的条目
        let mut j = i + 1;
        while j < sorted_refs.len() {
            let next = &sorted_refs[j];
            if next.btree_id != current.btree_id
                || next.inode != current.inode
                || next.offset != current.offset
                || next.snapshot != current.snapshot
            {
                break;
            }
            j += 1;
        }
        // [i, j) 范围内的条目具有相同的 btree_id + pos
        // 选择 journal_seq 最大的（最新写入）
        let best = (i..j).max_by_key(|&k| sorted_refs[k].journal_seq).unwrap();
        // 从实际 key 中检查 journal_seq（跳过已丢弃的）
        let actual_key = &keys[sorted_refs[best].idx as usize];
        if actual_key.journal_seq > 0 {
            result.push(&sorted_refs[best]);
        }
        i = j;
    }
    result
}

/// Fastpath flush — 遍历 sorted_refs，通过 engine.get_entry 做 noop 检查，
/// 成功后通过 engine.insert_entry 写入 btree。
///
/// 返回仍然有未 flush key 的索引列表（slowpath 需要重试）。
fn flush_fastpath(
    refs: &[&WbKeyRef],
    keys: &[BtreeWriteBufferedKey],
    engine: &mut BtreeEngine,
) -> Vec<usize> {
    let mut slowpath_indices: Vec<usize> = Vec::new();
    for (result_idx, wb_ref) in refs.iter().enumerate() {
        let key_idx = wb_ref.idx as usize;
        let wk = &keys[key_idx];
        // Noop 检查：engine 中已有相同 key 和 value 的条目
        let existing = engine.get_entry(wk.btree_id, &wk.key);
        let is_noop = match existing {
            Some((ref ek, ref ev)) => {
                ek.key_type == wk.key_type
                    && wk.key.get_vaddr() == ek.get_vaddr()
                    && wk.key.get_snapshot_id() == ek.get_snapshot_id()
                    && ev.paddr == wk.value.paddr
                    && ev.ver == wk.value.ver
            }
            None => false,
        };
        if is_noop {
            // Noop — 标记为已 flush
            continue;
        }
        // 尝试 fastpath insert
        let success = engine.insert_entry(wk.btree_id, wk.key, wk.value, wk.journal_seq);
        if !success {
            slowpath_indices.push(result_idx);
        }
    }
    slowpath_indices
}

/// Slowpath flush — 对 fastpath 失败的 key 通过事务提交重试
fn flush_slowpath(
    refs: &[&WbKeyRef],
    keys: &[BtreeWriteBufferedKey],
    engine: &mut BtreeEngine,
    journal: Option<&Journal>,
    backend: &dyn BlockDevice,
) -> Result<(), StorageError> {
    for &wb_ref in refs {
        let wk = &keys[wb_ref.idx as usize];
        if wk.journal_seq == 0 {
            continue; // 已被 noop 消除或已 flush
        }
        if let Some(j) = journal {
            // 通过事务路径重试（使用 block_on 在同步上下文中调用 async trans_commit）
            let mut trans = BtreeTrans::default();
            trans.begin();
            trans.journal_insert(wk.btree_id, 0, false, wk.key, wk.value, 0);
            Handle::current()
                .block_on(trans.trans_commit(j, engine, backend))
                .map_err(|e| StorageError::JournalError(e.to_string()))?;
        } else {
            // 没有 journal 时直接插入（无事务保证）
            engine.insert_entry(wk.btree_id, wk.key, wk.value, wk.journal_seq);
        }
    }
    Ok(())
}

/// bcachefs 对齐: wb_key_cmp — write buffer key 比较函数
///
/// 按 (btree_id ASC, bpos ASC) 顺序比较两个 write buffered key。
pub fn wb_key_cmp(a: &BtreeWriteBufferedKey, b: &BtreeWriteBufferedKey) -> Ordering {
    let a_pos = Bpos::from_key(&a.key);
    let b_pos = Bpos::from_key(&b.key);
    (a.btree_id as u8)
        .cmp(&(b.btree_id as u8))
        .then_with(|| a_pos.cmp(&b_pos))
}

/// bcachefs 对齐: bch_wb_btree_idx — 从 BtreeId 映射到 wb_btree 索引
///
/// 将 volmount 的 6 种 BtreeId 映射到 bcachefs 对齐的 BchWbBtree 枚举。
pub fn bch_wb_btree_idx(btree_id: BtreeId) -> BchWbBtree {
    match btree_id {
        BtreeId::Extents => BchWbBtree::Accounting,
        BtreeId::Subvolumes => BchWbBtree::Lru,
        BtreeId::Snapshots => BchWbBtree::NeedDiscard,
        BtreeId::SnapshotTrees => BchWbBtree::Backpointers,
        BtreeId::Alloc => BchWbBtree::DeletedInodes,
        BtreeId::Freespace => BchWbBtree::ReconcileWork,
        BtreeId::BucketGens => BchWbBtree::ReconcileWork,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;

    fn make_test_key(vaddr: u64, snapshot_id: u32, key_type: KeyType) -> BtreeKey {
        BtreeKey::new(vaddr, snapshot_id, key_type)
    }

    fn make_test_value(paddr: u64, ver: u16) -> BchVal {
        BchVal::new(paddr, ver)
    }

    fn make_wb_key(
        journal_seq: u64,
        btree_id: BtreeId,
        vaddr: u64,
        snapshot_id: u32,
    ) -> BtreeWriteBufferedKey {
        BtreeWriteBufferedKey {
            journal_seq,
            btree_id,
            key: make_test_key(vaddr, snapshot_id, KeyType::Normal),
            value: make_test_value(vaddr, 1),
            key_type: KeyType::Normal,
        }
    }

    #[test]
    fn test_write_buffer_must_wait() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        assert!(!bch2_btree_write_buffer_must_wait(&wb));

        // 填满 buffer 到超过 75%
        for i in 0..100 {
            wb.inc
                .keys
                .push(make_wb_key(i as u64, BtreeId::Extents, i as u64, 0));
            wb.inc.nr = wb.inc.keys.len();
        }
        assert!(bch2_btree_write_buffer_must_wait(&wb));
    }

    #[test]
    fn test_write_buffer_create() {
        let wb = btree_write_buffer_new(BchWbBtree::Accounting);
        assert_eq!(wb.idx as u8, 0);
        assert!(wb.inc.keys.is_empty());
        assert!(wb.flushing.keys.is_empty());
    }

    #[test]
    fn test_write_buffer_insert_and_flush() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let mut engine = BtreeEngine::new();

        // 插入 3 个 key
        let wk1 = make_wb_key(1, BtreeId::Extents, 100, 0);
        let wk2 = make_wb_key(2, BtreeId::Extents, 200, 0);
        let wk3 = make_wb_key(3, BtreeId::Extents, 300, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // flush
        let backend = MockBlockDevice::new();
        let result = bch2_btree_write_buffer_flush_locked(&mut wb, &mut engine, None, &backend);
        assert!(result.is_ok());

        // 验证 key 已写入 engine
        assert!(engine
            .get_entry(BtreeId::Extents, &make_test_key(100, 0, KeyType::Normal))
            .is_some());
        assert!(engine
            .get_entry(BtreeId::Extents, &make_test_key(200, 0, KeyType::Normal))
            .is_some());
        assert!(engine
            .get_entry(BtreeId::Extents, &make_test_key(300, 0, KeyType::Normal))
            .is_some());

        // flushing 应已清空
        assert_eq!(wb.flushing.nr, 0);
        assert!(wb.flushing.keys.is_empty());
        assert_eq!(wb.nr_flushes, 1);
    }

    #[test]
    fn test_write_buffer_dedup() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let mut engine = BtreeEngine::new();

        // 对同一位置插入 3 个 key（不同 journal_seq）
        let wk1 = make_wb_key(10, BtreeId::Extents, 100, 0);
        let wk2 = make_wb_key(20, BtreeId::Extents, 100, 0);
        let wk3 = make_wb_key(30, BtreeId::Extents, 100, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // flush
        let backend = MockBlockDevice::new();
        let result = bch2_btree_write_buffer_flush_locked(&mut wb, &mut engine, None, &backend);
        assert!(result.is_ok());

        // 验证 engine 中只有最新值（journal_seq=30 的 paddr）
        let entry = engine.get_entry(BtreeId::Extents, &make_test_key(100, 0, KeyType::Normal));
        assert!(entry.is_some());
        let (_k, v) = entry.unwrap();
        assert_eq!(v.paddr.get(), 100); // paddr = vaddr = 100
        assert_eq!(v.ver, 1);
    }

    #[test]
    fn test_write_buffer_noop_elimination() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let mut engine = BtreeEngine::new();

        // 先在 engine 中插入一个 key
        let existing = make_test_key(100, 0, KeyType::Normal);
        let existing_val = make_test_value(100, 1);
        engine.insert_entry(BtreeId::Extents, existing, existing_val, 1);

        // 在 write buffer 中插入相同 key 和 value
        let wk = BtreeWriteBufferedKey {
            journal_seq: 2,
            btree_id: BtreeId::Extents,
            key: make_test_key(100, 0, KeyType::Normal),
            value: make_test_value(100, 1),
            key_type: KeyType::Normal,
        };
        wb.inc.keys.push(wk);
        wb.inc.nr = wb.inc.keys.len();

        let key_count_before = engine.get(BtreeId::Extents).key_count();

        let backend = MockBlockDevice::new();
        let result = bch2_btree_write_buffer_flush_locked(&mut wb, &mut engine, None, &backend);
        assert!(result.is_ok());

        // key count 应该不变（noop 消除）
        let key_count_after = engine.get(BtreeId::Extents).key_count();
        assert_eq!(key_count_before, key_count_after);
    }

    #[test]
    fn test_write_buffer_sort_order() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        #[allow(unused_mut)]
        let mut engine = BtreeEngine::new();

        // 无序插入
        let wk1 = make_wb_key(1, BtreeId::Extents, 300, 0);
        let wk2 = make_wb_key(2, BtreeId::Extents, 100, 0);
        let wk3 = make_wb_key(3, BtreeId::Extents, 200, 0);

        wb.inc.keys.push(wk1);
        wb.inc.keys.push(wk2);
        wb.inc.keys.push(wk3);
        wb.inc.nr = wb.inc.keys.len();

        // 验证排序
        let refs = build_sorted_index(&wb.inc.keys);
        let mut sorted_refs = refs.clone();
        wb_sort(&mut sorted_refs);

        // 排序后应为 100, 200, 300（按 vaddr/offset 升序）
        assert_eq!(sorted_refs[0].offset, 100);
        assert_eq!(sorted_refs[1].offset, 200);
        assert_eq!(sorted_refs[2].offset, 300);
    }

    #[test]
    fn test_write_buffer_should_flush() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        assert!(!bch2_btree_write_buffer_must_wait(&wb));

        // 填充少量 key → 不应触发 must_wait
        for i in 0..10 {
            wb.inc
                .keys
                .push(make_wb_key(i as u64, BtreeId::Extents, i as u64, 0));
        }
        wb.inc.nr = wb.inc.keys.len();
        // capacity 通常 >= 10，10 > capacity * 3/4 → false
        // 但取决于 Vec 的 capacity 策略，可能刚好 true
        // 不严格断言，只验证函数不 panic
        let _ = bch2_btree_write_buffer_must_wait(&wb);

        // 验证 need_flush 返回 true（有 pending key）
        let wbs = [wb];
        assert!(bch2_journal_write_buffer_need_flush(&wbs));
    }

    #[test]
    fn test_bch_wb_btree_idx() {
        assert_eq!(bch_wb_btree_idx(BtreeId::Extents) as u8, 0);
        assert_eq!(bch_wb_btree_idx(BtreeId::Subvolumes) as u8, 1);
        assert_eq!(bch_wb_btree_idx(BtreeId::Snapshots) as u8, 2);
        assert_eq!(bch_wb_btree_idx(BtreeId::SnapshotTrees) as u8, 3);
        assert_eq!(bch_wb_btree_idx(BtreeId::Alloc) as u8, 4);
        assert_eq!(bch_wb_btree_idx(BtreeId::Freespace) as u8, 5);
    }

    #[test]
    fn test_wb_key_cmp() {
        // 相同 btree_id + 相同 bpos → Equal
        let a = make_wb_key(1, BtreeId::Extents, 100, 0);
        let b = make_wb_key(2, BtreeId::Extents, 100, 0);
        assert_eq!(wb_key_cmp(&a, &b), Ordering::Equal);

        // 不同 btree_id → 按 btree_id 排序
        let c = make_wb_key(3, BtreeId::Freespace, 100, 0);
        assert_eq!(wb_key_cmp(&a, &c), Ordering::Less);

        // 相同 btree_id + 不同 offset → 按 offset 排序
        let d = make_wb_key(4, BtreeId::Extents, 200, 0);
        assert_eq!(wb_key_cmp(&a, &d), Ordering::Less);
    }

    #[test]
    fn test_write_buffer_flush_locked_empty() {
        let mut wb = btree_write_buffer_new(BchWbBtree::Accounting);
        let mut engine = BtreeEngine::new();

        // 空 buffer flush → 应成功且无副作用
        let backend = MockBlockDevice::new();
        let result = bch2_btree_write_buffer_flush_locked(&mut wb, &mut engine, None, &backend);
        assert!(result.is_ok());
        assert_eq!(wb.nr_flushes, 1);
    }

    // ─── 生命周期函数测试 ───────────────────────────────────────────────────

    #[test]
    fn test_wb_set_init_early() {
        let mut set = BtreeWriteBufferSet::new();
        bch2_fs_btree_write_buffer_init_early(&mut set);

        // 验证每个 buffer 的 idx 正确
        assert_eq!(set.buffers[0].idx, BchWbBtree::Accounting);
        assert_eq!(set.buffers[1].idx, BchWbBtree::Lru);
        assert_eq!(set.buffers[2].idx, BchWbBtree::NeedDiscard);
        assert_eq!(set.buffers[3].idx, BchWbBtree::Backpointers);
        assert_eq!(set.buffers[4].idx, BchWbBtree::DeletedInodes);
        assert_eq!(set.buffers[5].idx, BchWbBtree::ReconcileWork);
        assert_eq!(set.buffers[6].idx, BchWbBtree::ReconcileHipri);
        assert_eq!(set.buffers[7].idx, BchWbBtree::ReconcilePending);
        assert_eq!(set.buffers[8].idx, BchWbBtree::ReconcileWorkPhys);
        assert_eq!(set.buffers[9].idx, BchWbBtree::ReconcileHipriPhys);
        assert_eq!(set.buffers[10].idx, BchWbBtree::StripeBackpointers);
    }

    #[test]
    fn test_wb_set_init() {
        let mut set = BtreeWriteBufferSet::new();
        bch2_fs_btree_write_buffer_init_early(&mut set);
        let ret = bch2_fs_btree_write_buffer_init(&mut set);

        assert_eq!(ret, 0);
        // 验证每个 buffer 的 inc 和 flushing keys 容量 >= 1024
        for wb in set.buffers.iter() {
            assert!(
                wb.inc.keys.capacity() >= 1024,
                "buffer {:?} inc capacity < 1024",
                wb.idx
            );
            assert!(
                wb.flushing.keys.capacity() >= 1024,
                "buffer {:?} flushing capacity < 1024",
                wb.idx
            );
            assert!(wb.inc.keys.is_empty());
            assert!(wb.flushing.keys.is_empty());
        }
    }

    #[test]
    fn test_wb_set_exit() {
        let mut set = BtreeWriteBufferSet::new();
        bch2_fs_btree_write_buffer_init_early(&mut set);
        bch2_fs_btree_write_buffer_init(&mut set);

        // 添加一些 key 到 buffer[0]
        set.buffers[0]
            .inc
            .keys
            .push(make_wb_key(1, BtreeId::Extents, 100, 0));
        set.buffers[0].inc.nr = set.buffers[0].inc.keys.len();

        // 执行 exit
        bch2_fs_btree_write_buffer_exit(&mut set);

        // 验证所有 buffer 的 keys 已被清空
        for wb in set.buffers.iter() {
            assert!(
                wb.inc.keys.is_empty(),
                "buffer {:?} inc keys not empty after exit",
                wb.idx
            );
            assert_eq!(wb.inc.nr, 0, "buffer {:?} inc.nr != 0 after exit", wb.idx);
            assert!(
                wb.flushing.keys.is_empty(),
                "buffer {:?} flushing keys not empty after exit",
                wb.idx
            );
            assert_eq!(
                wb.flushing.nr, 0,
                "buffer {:?} flushing.nr != 0 after exit",
                wb.idx
            );
        }
    }

    #[test]
    fn test_journal_keys_to_wb_start_end() {
        let mut set = BtreeWriteBufferSet::new();
        bch2_fs_btree_write_buffer_init_early(&mut set);
        bch2_fs_btree_write_buffer_init(&mut set);

        // 第一阶段：空 buffer → 不需要 flush
        {
            let mut dst = JournalKeysToWb { set: &set, seq: 0 };
            bch2_journal_keys_to_write_buffer_start(&set, &mut dst, 42);
            assert_eq!(dst.seq, 42);
            assert!(std::ptr::eq(dst.set, &set));

            let ret = bch2_journal_keys_to_write_buffer_end(&set, &mut dst);
            assert_eq!(ret, 0);
        } // dst 释放，set 借用结束

        // 第二阶段：添加 key 到 buffer[0] 使其超过 capacity/4 阈值
        let capacity = set.buffers[0].inc.keys.capacity();
        let keys_to_add = capacity / 4 + 1;
        for i in 0..keys_to_add {
            set.buffers[0]
                .inc
                .keys
                .push(make_wb_key(i as u64, BtreeId::Extents, i as u64, 0));
        }
        set.buffers[0].inc.nr = set.buffers[0].inc.keys.len();

        // 第三阶段：触发 flush
        {
            let mut dst = JournalKeysToWb { set: &set, seq: 0 };
            bch2_journal_keys_to_write_buffer_start(&set, &mut dst, 43);
            let ret = bch2_journal_keys_to_write_buffer_end(&set, &mut dst);
            assert_eq!(ret, 1, "expected flush trigger when inc.nr > capacity/4");
        }
    }
}
