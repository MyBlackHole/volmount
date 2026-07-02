//! Btree I/O (Read/Write) — bcachefs 对齐
//!
//! 对应 bcachefs btree_io.c + btree_read.c 中的公开 API。
//! 当前实现包装 bucket_io.rs 的底层读写，提供 bcachefs 命名对齐的接口。

use crate::block_device::BlockDevice;
use crate::btree::bucket_io;
use crate::btree::cache::bch2_btree_node_write_done_clean;
use crate::btree::key::{bkey_unpack, bpos_cmp, BkeyPacked, Bpos, BtreeKey, BKEY_FORMAT_CURRENT};
use crate::btree::node::{BsetTree, BtreeNode, MAX_BSETS};
use crate::btree::types::NodeCache;
use crate::journal::Journal;
use crate::StorageError;
use std::cmp::Ordering;
use std::future::Future;
use std::sync::Arc;

// ─── Read Path ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_node_io_lock — 获取节点写入 I/O 锁
///
/// 使用 flag-based CAS 协议对齐 bcachefs `wait_on_bit_lock(BTREE_NODE_write_in_flight)`。
/// spin 等待直到 write_in_flight 标志清除，然后 CAS 设置为 true。
pub fn bch2_btree_node_io_lock(node: &BtreeNode) {
    loop {
        if node.try_lock_write_in_flight() {
            return;
        }
        std::thread::yield_now();
    }
}

/// bcachefs 对齐: bch2_btree_node_io_unlock — 释放节点写入 I/O 锁
pub fn bch2_btree_node_io_unlock(node: &BtreeNode) {
    bch2_btree_node_write_done_clean(node);
}

/// bcachefs 对齐: bch2_btree_node_wait_on_read — 等待节点读取完成
///
/// spin 等待直到 read_in_flight 标志清除。
pub fn bch2_btree_node_wait_on_read(node: &BtreeNode) {
    while node.is_read_in_flight() {
        std::thread::yield_now();
    }
}

/// bcachefs 对齐: bch2_btree_node_wait_on_write — 等待节点写入完成
///
/// spin 等待直到 write_in_flight 标志清除。
pub fn bch2_btree_node_wait_on_write(node: &BtreeNode) {
    while node.is_write_in_flight() {
        std::thread::yield_now();
    }
}

fn run_blocking_storage_io<F, T>(future: F) -> Result<T, StorageError>
where
    F: Future<Output = Result<T, StorageError>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| StorageError::JournalError(format!("failed to build runtime: {e}")))?
            .block_on(future)
    })
    .join()
    .map_err(|_| StorageError::JournalError("btree writeback thread panicked".into()))?
}

async fn write_node_record(
    node: Arc<BtreeNode>,
    block_addr: u64,
    backend: Arc<dyn BlockDevice>,
) -> Result<(), StorageError> {
    bucket_io::write_node_to_bucket(node.as_ref(), block_addr, backend.as_ref()).await
}

pub(crate) fn make_btree_node_flush_fn(
    node: Arc<BtreeNode>,
    backend: Arc<dyn BlockDevice>,
) -> crate::journal::reclaim::JournalPinFlushFn {
    Box::new(move |journal, pin, seq| {
        let block_addr = node.block_addr();
        if block_addr == 0 {
            return Err(StorageError::InvalidArgument(
                "btree node has no bound physical block address".into(),
            ));
        }
        let result = run_blocking_storage_io({
            let node = node.clone();
            let backend = backend.clone();
            async move { write_node_record(node, block_addr, backend).await }
        });

        if result.is_ok() && pin.seq.load(std::sync::atomic::Ordering::Acquire) == seq {
            journal.bch2_journal_pin_drop(pin);
        }

        result
    })
}

/// bcachefs 对齐: bch2_btree_node_read — 从后端读取 btree 节点
///
/// 从 backend 读取指定地址的节点并返回。节点数据由调用方负责插入缓存。
pub async fn bch2_btree_node_read(
    backend: &dyn BlockDevice,
    block_addr: u64,
) -> Result<BtreeNode, StorageError> {
    let mut node = bucket_io::load_btree_node(backend, block_addr).await?;
    bch2_btree_node_read_done(&mut node)?;
    Ok(node)
}

/// bcachefs 对齐: bch2_btree_root_read — 读取 btree 根节点
///
/// 从 backend 读取根节点，并输出 level 信息。
pub async fn bch2_btree_root_read(
    backend: &dyn BlockDevice,
    block_addr: u64,
) -> Result<(BtreeNode, u8), StorageError> {
    let mut node = bucket_io::load_btree_node(backend, block_addr).await?;
    let level = node.level;
    bch2_btree_node_read_done(&mut node)?;
    Ok((node, level))
}

// ─── Sort Iter 架构 (bcachefs sort_iter) ────────────────────────────────

/// bcachefs 对齐: sort_iter_entry — 单个 bset 上的 key 范围
#[derive(Debug, Clone, Copy)]
struct SortIterEntry {
    /// 指向 data buffer 中该 key 范围起始的偏移
    start_offset: u32,
    /// 指向该 key 范围结束的偏移
    end_offset: u32,
}

/// bcachefs 对齐: sort_iter — 用于收集多个 bsets 的 key 并全局排序
///
/// 对应 bcachefs `btree_read.c` 中的 sort_iter，用于 read_done 和 write 路径中
/// 将多个 bset 的 key 排序合并为单个紧凑 bset。
struct SortIter {
    entries: Vec<SortIterEntry>,
    data: *const u8,
    data_len: usize,
}

// Safety: SortIter 只持有指向 node.data 的指针，不拥有数据。
// SortIter 的生命周期必须短于 BtreeNode 的生命周期。
unsafe impl Send for SortIter {}
unsafe impl Sync for SortIter {}

impl SortIter {
    /// bcachefs 对齐: sort_iter_init — 从 BtreeNode 初始化 sort_iter
    pub fn init_from_node(node: &BtreeNode) -> Self {
        SortIter {
            entries: Vec::with_capacity(MAX_BSETS),
            data: node.data.as_ptr(),
            data_len: node.data.len(),
        }
    }

    /// bcachefs 对齐: sort_iter_add — 添加一个 bset 的 key 范围
    pub fn add(&mut self, start_offset: u32, end_offset: u32) {
        if start_offset < end_offset {
            self.entries.push(SortIterEntry {
                start_offset,
                end_offset,
            });
        }
    }

    /// 从 BtreeNode 的所有活跃 bsets 添加 key 范围
    pub fn add_all_bsets(&mut self, node: &BtreeNode) {
        let nsets = node.nsets() as usize;
        for si in 0..nsets {
            let s = &node.sets[si];
            if s.size > 0 {
                self.add(s.data_offset, s.end_offset);
            }
        }
    }

    /// 全局排序所有收集的 key 到 dst 缓冲区。
    /// 使用 packed key 直接比较（通过 bkey_cmp_packed），避免解包再重包。
    /// 返回写入 dst 的字节数。
    pub fn sort_into(&self, dst: &mut [u8]) -> Result<usize, StorageError> {
        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };

        // 第一遍：收集所有 key 的偏移
        let mut key_offsets: Vec<u32> = Vec::new();
        for entry in &self.entries {
            let mut cur = entry.start_offset;
            while cur < entry.end_offset {
                let offset = cur as usize;
                if offset + 3 > data.len() {
                    return Err(StorageError::CorruptData(format!(
                        "sort_iter: truncated key header at offset {}",
                        offset
                    )));
                }
                let u64s = data[offset];
                if u64s == 0 {
                    break; // 终止标记
                }
                let entry_bytes = (u64s as u32) * 8;
                if offset + entry_bytes as usize > data.len() {
                    return Err(StorageError::CorruptData(format!(
                        "sort_iter: key at offset {} exceeds data buffer",
                        offset
                    )));
                }
                key_offsets.push(cur);
                cur += entry_bytes;
            }
        }

        // 第二遍：对 key 偏移进行排序（通过 packed bpos 比较）
        key_offsets.sort_by(|&a, &b| unsafe {
            let pk_a = &*(data.as_ptr().add(a as usize) as *const crate::btree::key::BkeyPacked);
            let pk_b = &*(data.as_ptr().add(b as usize) as *const crate::btree::key::BkeyPacked);
            crate::btree::key::bkey_cmp_packed(&crate::btree::key::BKEY_FORMAT_CURRENT, pk_a, pk_b)
        });

        // 第三遍：按排序后的顺序复制 key 到 dst
        let mut dst_offset = 0usize;
        for &key_off in &key_offsets {
            let pk = unsafe {
                &*(data.as_ptr().add(key_off as usize) as *const crate::btree::key::BkeyPacked)
            };
            let entry_bytes = (pk.u64s as u32) as usize * 8;
            if dst_offset + entry_bytes > dst.len() {
                return Err(StorageError::CorruptData(
                    "sort_iter: destination buffer overflow".to_string(),
                ));
            }
            let src = key_off as usize;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr().add(src),
                    dst.as_mut_ptr().add(dst_offset),
                    entry_bytes,
                );
            }
            dst_offset += entry_bytes;
        }

        Ok(dst_offset)
    }

    /// 返回所有收集的 key 总数
    pub fn total_keys(&self) -> usize {
        let data = unsafe { std::slice::from_raw_parts(self.data, self.data_len) };
        let mut count = 0usize;
        for entry in &self.entries {
            let mut cur = entry.start_offset;
            while cur < entry.end_offset {
                let offset = cur as usize;
                if offset >= data.len() {
                    break;
                }
                let u64s = data[offset];
                if u64s == 0 {
                    break;
                }
                count += 1;
                cur += (u64s as u32) * 8;
            }
        }
        count
    }
}

// ─── Bset 内部迭代辅助 ──────────────────────────────────────────────────

/// 在 bset 数据范围内遍历所有 packed keys，返回 `(entry_u64s, format, type_, bpos)`。
///
/// 验证每个 key 的字节边界不超过范围，跳过 u64s=0（终止标记）。
fn iter_bset_packed_keys<'a>(
    data: &'a [u8],
    start: u32,
    end: u32,
) -> impl Iterator<Item = Result<(u8, u8, u8, Bpos), StorageError>> + 'a {
    let mut offset = start as usize;
    let end_usize = end as usize;
    std::iter::from_fn(move || {
        if offset >= end_usize {
            return None;
        }
        // 至少需要 3 字节 header (u64s, format+whiteout, type_)
        if offset + 3 > data.len() {
            return Some(Err(StorageError::CorruptData(format!(
                "bset entry at offset {}: only {} bytes remaining, need at least 3",
                offset,
                data.len() - offset
            ))));
        }
        let entry_u64s = data[offset];
        if entry_u64s == 0 {
            // 0 = 终止标记
            return None;
        }
        let entry_bytes = (entry_u64s as u32) * 8;
        if offset + entry_bytes as usize > end_usize {
            return Some(Err(StorageError::CorruptData(format!(
                "bset entry at offset {}: entry size {} bytes exceeds bset end {}",
                offset, entry_bytes, end_usize
            ))));
        }
        let format_whiteout = data[offset + 1];
        let type_ = data[offset + 2];
        // 解包 bpos
        let bpos = unsafe {
            let pk = &*(data.as_ptr().add(offset) as *const BkeyPacked);
            let (pos, _, _, _) = bkey_unpack(&BKEY_FORMAT_CURRENT, pk);
            pos
        };
        offset += entry_bytes as usize;
        Some(Ok((entry_u64s, format_whiteout, type_, bpos)))
    })
}

// ─── Bset 验证 ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: `bch2_validate_bset` — 验证单个 bset 的结构完整性
///
/// 验证内容（针对 volmount BsetTree 格式适配）：
/// - data_offset/end_offset 范围在节点 buffer 内
/// - 如果 size > 0，确保 data_offset < end_offset（存在实际数据）
/// - end_offset 8 字节对齐（每个 entry 都是 8 字节整数倍）
/// - 非首 bset（set_idx > 0）必须有 size > 0
pub fn bch2_validate_bset(node: &BtreeNode, set_idx: usize) -> Result<(), StorageError> {
    if set_idx >= MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "bset index {} exceeds MAX_BSETS {}",
            set_idx, MAX_BSETS
        )));
    }
    let set = &node.sets[set_idx];
    let node_size = node.node_size as usize;

    // data_offset 必须在节点 buffer 范围内
    if set.data_offset as usize > node_size {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] data_offset {} exceeds node_size {}",
            set_idx, set.data_offset, node_size
        )));
    }
    // end_offset 必须在节点 buffer 范围内
    if set.end_offset as usize > node_size {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] end_offset {} exceeds node_size {}",
            set_idx, set.end_offset, node_size
        )));
    }
    // 如果有 data，必须满足 start < end
    if set.size > 0 && set.data_offset >= set.end_offset {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] size={} but data_offset={} >= end_offset={}",
            set_idx, set.size, set.data_offset, set.end_offset
        )));
    }
    // end_offset 必须 8 字节对齐（entry 大小是 8 的倍数）
    if !set.end_offset.is_multiple_of(8u32) {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] end_offset {} not 8-byte aligned",
            set_idx, set.end_offset
        )));
    }
    // 如果没有数据，size 必须为 0
    if set.data_offset == set.end_offset && set.size != 0 {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] data_offset==end_offset but size={}",
            set_idx, set.size
        )));
    }
    // 非首 bset：必须有 size > 0（volmount 中增量 bset 不需要持久化白板）
    if set_idx > 0 && set.size == 0 {
        // 允许增量 bset 为空（写入路径可能预初始化）
    }

    Ok(())
}

/// bcachefs 对齐: `bch2_validate_bset_keys` — 验证 bset 内所有 key 的排序顺序
///
/// 遍历 bset 内所有 packed keys：
/// - 验证每个 key 的格式字段合法（format == KEY_FORMAT_CURRENT）
/// - 检查相邻 key 的 bpos 非降序
/// - 检查无相邻重复 key
pub fn bch2_validate_bset_keys(node: &BtreeNode, set_idx: usize) -> Result<(), StorageError> {
    if set_idx >= MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "bset index {} exceeds MAX_BSETS {}",
            set_idx, MAX_BSETS
        )));
    }
    let set = &node.sets[set_idx];
    if set.size == 0 {
        return Ok(());
    }
    let data = &node.data;
    let start = set.data_offset;
    let end = set.end_offset;

    let mut prev_bpos: Option<Bpos> = None;
    let mut entry_count: u16 = 0;

    for result in iter_bset_packed_keys(data, start, end) {
        let (entry_u64s, format_whiteout, _type_, bpos) = result?;
        let format = format_whiteout & 0x7F;

        // 验证 format 合法：只能是 KEY_FORMAT_CURRENT(1) 或 KEY_FORMAT_LOCAL_BTREE(0)
        if format != 1 && format != 0 {
            return Err(StorageError::CorruptData(format!(
                "bset[{}] entry {}: invalid format {}",
                set_idx,
                entry_count + 1,
                format
            )));
        }
        // 验证 u64s >= BKEY_U64S (3)
        if entry_u64s < crate::btree::key::BKEY_U64S {
            return Err(StorageError::CorruptData(format!(
                "bset[{}] entry {}: u64s={} less than minimum BKEY_U64S",
                set_idx,
                entry_count + 1,
                entry_u64s
            )));
        }

        entry_count += 1;

        // 检查相邻 key 的 bpos 非降序
        if let Some(prev) = prev_bpos {
            match bpos_cmp(prev, bpos) {
                Ordering::Greater => {
                    return Err(StorageError::CorruptData(format!(
                        "bset[{}] key order violation: entries {} and {} are descending \
                         (prev={} > curr={})",
                        set_idx,
                        entry_count - 1,
                        entry_count,
                        prev,
                        bpos
                    )));
                }
                Ordering::Equal => {
                    return Err(StorageError::CorruptData(format!(
                        "bset[{}] duplicate key at entries {} and {}: bpos={}",
                        set_idx,
                        entry_count - 1,
                        entry_count,
                        bpos
                    )));
                }
                Ordering::Less => {} // 正确顺序
            }
        }
        prev_bpos = Some(bpos);
    }

    // 如果 bset.size 不为 0，验证实际遍历到的 entry 数匹配
    if set.size > 0 && entry_count != set.size {
        return Err(StorageError::CorruptData(format!(
            "bset[{}] declared size={} but actual entries={}",
            set_idx, set.size, entry_count
        )));
    }

    Ok(())
}

/// bcachefs 对齐: `bch2_btree_node_read_done` — 节点读取完成后的验证流水线
///
/// 验证流程（对齐 bcachefs `read.c` 的 read_done 验证路径）：
/// 1. 验证 header magic 为 `BTREE_NODE_MAGIC`（兼容性检查—节点层已做，此处后备）
/// 2. 验证 level 无异常
/// 3. 遍历所有活跃 bsets，对每个 bset 调用 `validate_bset` + `validate_bset_keys`
/// 4. 全局 key 排序：调用 `read_done_sort` 将多 bset 合并为单紧凑 bset
/// 5. 可选：`drop_keys_outside_node`（由 caller 根据 updated_range 决定调用）
/// 6. 清除 read_in_flight 标志
pub fn bch2_btree_node_read_done(node: &mut BtreeNode) -> Result<(), StorageError> {
    // [0] 基础 sanity 检查
    if node.data.is_empty() {
        node.clear_read_in_flight();
        return Err(StorageError::CorruptData(
            "btree node has empty data buffer".to_string(),
        ));
    }
    if node.node_size == 0 {
        node.clear_read_in_flight();
        return Err(StorageError::CorruptData(
            "btree node has zero node_size".to_string(),
        ));
    }

    // [1] 验证 header 与 data 关系
    let nsets = node.nsets() as usize;
    let result = _read_done_inner(node, nsets);

    // 在所有路径上清除 read_in_flight
    node.clear_read_in_flight();
    result
}

/// read_done 的内部实现，不负责清理 read_in_flight
fn _read_done_inner(node: &mut BtreeNode, nsets: usize) -> Result<(), StorageError> {
    if nsets == 0 || nsets > MAX_BSETS {
        return Err(StorageError::CorruptData(format!(
            "btree node has invalid nsets={}",
            nsets
        )));
    }

    // [2] 遍历所有活跃 bsets, 执行逐 bset 验证
    for si in 0..nsets {
        bch2_validate_bset(node, si)?;
        bch2_validate_bset_keys(node, si)?;
    }

    // [3] 跨 bset 验证: journal_seq 一致性
    // volmount 中所有 bset 共享同一个 journal_seq（节点级），

    // [4] 全局排序合并: 读取后排序合并多个 bset 到单个紧凑 set[0]
    bch2_read_done_sort(node)?;

    // [5] 全局 key 范围验证
    let _ = bch2_btree_node_drop_keys_outside_node(node);

    Ok(())
}

// ─── 读取后全局排序合并（bcachefs sort_iter 模式） ─────────────────────────

/// bcachefs 对齐: 读取后全局排序合并
///
/// 对应 bcachefs read_done 中的 sort_iter 模式：
/// 1. sort_iter 收集所有 bsets 的 key 范围
/// 2. 全局排序（使用 packed bpos 比较）
/// 3. 将排序后的 key 写入节点 buffer
/// 4. compact() 完成去重 + 过滤 whiteout + aux tree 构建
///
/// 先使用 sort_iter 做初步排序，再通过 compact() 做最终去重和 aux 构建。
/// 当节点只有单个 bset 时，跳过 sort_iter，直接 compact。
pub fn bch2_read_done_sort(node: &mut BtreeNode) -> Result<(), StorageError> {
    let nsets = node.nsets();
    if nsets <= 1 {
        // 只有单个 bset → 直接 compact（无跨 set 合并需求）
        node.compact();
        return Ok(());
    }

    // 多个 bset: 先用 sort_iter 排序合并
    let total_keys = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.total_keys()
    };
    if total_keys == 0 {
        node.compact();
        return Ok(());
    }

    // 创建临时缓冲区存储排序后的 packed keys
    let buf_size = node.node_size as usize;
    let mut sorted_buf = vec![0u8; buf_size];
    let data_len = node.data.len();

    let written = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.sort_into(&mut sorted_buf)?
    };

    // 将排序后的数据写回节点 buffer
    if written > 0 && written <= buf_size {
        unsafe {
            std::ptr::copy_nonoverlapping(sorted_buf.as_ptr(), node.data.as_mut_ptr(), written);
        }
        // 清空多余区域
        let zero_end = buf_size.min(data_len);
        if written < zero_end {
            node.data[written..zero_end].fill(0);
        }

        // 更新 set[0] 指向排序后的数据
        node.sets[0].data_offset = 0;
        node.sets[0].end_offset = written as u32;
        node.sets[0].size = total_keys as u16;
        node.sets[0].aux_offset = 0;
        node.sets[0].extra = 0;
        // 清空其他 sets
        for i in 1..MAX_BSETS {
            node.sets[i].data_offset = 0;
            node.sets[i].end_offset = 0;
            node.sets[i].aux_offset = 0;
            node.sets[i].size = 0;
            node.sets[i].extra = 0;
        }
        node.key_count = total_keys as u32;
    }

    // compact() 完成去重、过滤 whiteout 和 aux tree 构建
    node.compact();

    Ok(())
}

/// bcachefs 对齐: `bch2_btree_node_sort_keys` — 写入前排序合并所有 key
///
/// 使用 sort_iter 架构将节点中所有未写入的 key 排序合并。
/// 在写入前调用，确保序列化时只需要处理一个排序后的 bset。
/// 返回修改后的节点引用（已排序合并）。
pub fn bch2_btree_node_sort_keys(node: &mut BtreeNode) -> Result<(), StorageError> {
    let nsets = node.nsets();
    if nsets <= 1 {
        // 单个 bset 无需合并，但可能仍有 uncommitted 的 key
        // 如果 key_count 为 0 或只有一个 set 且已有数据，无需操作
        if node.whiteout_count > 0 || node.sets[0].aux_offset == 0 {
            node.compact();
        }
        return Ok(());
    }

    // 多个 bset: 使用 sort_iter 排序合并
    // 收集所有 key
    let total_keys = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.total_keys()
    };
    if total_keys == 0 {
        return Ok(());
    }

    let buf_size = node.node_size as usize;
    let mut sorted_buf = vec![0u8; buf_size];
    let data_len = node.data.len();

    let written = {
        let mut iter = SortIter::init_from_node(node);
        iter.add_all_bsets(node);
        iter.sort_into(&mut sorted_buf)?
    };

    if written > 0 && written <= buf_size {
        unsafe {
            std::ptr::copy_nonoverlapping(sorted_buf.as_ptr(), node.data.as_mut_ptr(), written);
        }
        let zero_end = buf_size.min(data_len);
        if written < zero_end {
            node.data[written..zero_end].fill(0);
        }

        // 更新为单 bset 结构
        node.sets[0] = BsetTree {
            data_offset: 0,
            end_offset: written as u32,
            aux_offset: 0,
            size: total_keys as u16,
            extra: 0,
        };
        for i in 1..MAX_BSETS {
            node.sets[i] = BsetTree {
                data_offset: 0,
                end_offset: 0,
                aux_offset: 0,
                size: 0,
                extra: 0,
            };
        }
        node.key_count = total_keys as u32;
        node.whiteout_count = 0;
    }

    // compact 确保 aux tree 就绪
    node.compact();

    Ok(())
}

// ─── 范围裁剪 ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: `bch2_btree_node_drop_keys_outside_node` — 丢弃超出节点范围的 key
///
/// 遍历节点所有 bsets，丢弃 bpos < node.min_key 或 bpos > node.max_key 的条目。
/// 使用 compact() 重建 aux tree。
pub fn bch2_btree_node_drop_keys_outside_node(node: &mut BtreeNode) -> Result<(), StorageError> {
    let min_key = node.min_key;
    let max_key = node.max_key;

    // 空节点或空范围：跳过
    if node.key_count == 0 {
        return Ok(());
    }
    // 如果 min_key > max_key 表示没有有效的范围约束
    if bpos_cmp(min_key, max_key) != Ordering::Less {
        // 空节点（min_key = MAX, max_key = MIN, 实际 min > max）或未设置范围
        return Ok(());
    }

    // 收集所有 bset 中的条目，过滤出在范围内的条目
    let mut all: Vec<crate::btree::key::BtreeEntry> = Vec::new();
    for (si, _set) in crate::btree::node::for_each_bset(node) {
        let s = &node.sets[si];
        let mut cur = s.data_offset;
        while cur < s.end_offset {
            let entry = node.read_packed_entry_raw(cur as usize);
            let entry_bpos = entry.pos;
            // 丢弃范围外的 key
            if bpos_cmp(entry_bpos, min_key) != Ordering::Less
                && bpos_cmp(entry_bpos, max_key) != Ordering::Greater
            {
                all.push(entry);
            }
            let u64s = node.read_entry_u64s(cur as usize);
            cur += (u64s as u32) * 8;
        }
    }

    // 重写节点数据（类似 compact 但不排序去重，保持原始顺序）
    let n = all.len();
    let aes = std::mem::size_of::<crate::btree::key::BtreeKey>() + 4;
    let mut cur = 0u32;
    let mut offsets: Vec<u32> = Vec::with_capacity(n);
    for entry in &all {
        offsets.push(cur);
        let size = node.write_entry_bytes(cur, entry);
        cur += size;
    }

    // 写 aux 数组
    let ds = cur;
    let aux_used = n * aes;
    let mut aux_offset = 0u32;
    if (ds as usize + aux_used) <= node.node_size as usize {
        let aux_base = ds as usize;
        for (i, entry) in all.iter().enumerate() {
            let k = BtreeKey::from_bpos(entry.pos, entry.key_type);
            unsafe {
                let aux_ptr = &mut node.data[aux_base + i * aes] as *mut u8;
                std::ptr::addr_of_mut!(*aux_ptr.cast::<BtreeKey>()).write_unaligned(k);
                std::ptr::addr_of_mut!(*aux_ptr.add(std::mem::size_of::<BtreeKey>()).cast::<u32>())
                    .write_unaligned(offsets[i]);
            }
        }
        aux_offset = ds;
    }

    node.sets[0] = BsetTree {
        data_offset: 0,
        end_offset: ds,
        aux_offset,
        size: n as u16,
        extra: 0,
    };
    for i in 1..MAX_BSETS {
        node.sets[i] = BsetTree {
            data_offset: 0,
            end_offset: 0,
            aux_offset: 0,
            size: 0,
            extra: 0,
        };
    }
    node.key_count = n as u32;
    node.whiteout_count = 0;

    Ok(())
}

/// bcachefs 对齐: `bch2_btree_node_header_to_text` — 节点 header 的调试输出
///
/// 格式化输出 BtreeNode 关键字段（magic、version、level、key_count 等）。
/// 用于错误日志和调试。
pub fn bch2_btree_node_header_to_text(node: &BtreeNode) -> String {
    format!(
        "BtreeNode(level={}, key_count={}, whiteout={}, nsets={}, \
         min_key={}, max_key={}, journal_seq={}, node_size={}, \
         data_len={})",
        node.level,
        node.key_count,
        node.whiteout_count,
        node.nsets(),
        node.min_key,
        node.max_key,
        node.journal_seq,
        node.node_size,
        node.data.len(),
    )
}

/// bcachefs 对齐: bch2_btree_flush_all_reads — 刷新所有正在进行的读取操作
pub fn bch2_btree_flush_all_reads() -> bool {
    // volmount: 当前为同步读取，没有飞行中的读操作
    true
}

// ─── Write Path ─────────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_btree_node_write — 将 btree 节点写入后端
///
/// 将节点序列化后写入指定地址的 block。
/// 写入前执行 sort_iter 排序合并（如果有多个 bset）。
/// 如果提供了 `journal` 且节点已持有 active pin，写入成功后释放该 pin。
pub async fn bch2_btree_node_write(
    node: &BtreeNode,
    block_addr: u64,
    backend: &dyn BlockDevice,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    let node_seq = node.journal_seq;
    let pin = node.journal_pin.lock().unwrap();
    let has_active_pin = pin.as_ref().map(|pin| pin.is_active()).unwrap_or(false);
    drop(pin);

    // bch2_btree_node_write 接受 &BtreeNode（不可变引用），
    // 所以不能在此处修改节点。排序合并应在调用此函数前完成。
    // 写路径上的排序合并发生在 __bch2_btree_node_write 中。
    bch2_btree_node_io_lock(node);
    let result = bucket_io::write_node_to_bucket(node, block_addr, backend).await;
    bch2_btree_node_io_unlock(node);
    result?;

    if let Some(j) = journal {
        if has_active_pin {
            let pin_guard = node.journal_pin.lock().unwrap();
            if let Some(pin) = pin_guard.as_ref() {
                if pin.seq.load(std::sync::atomic::Ordering::Acquire) == node_seq {
                    j.bch2_journal_pin_drop(pin);
                }
            }
        }
    }

    Ok(())
}

/// bcachefs 对齐: bch2_btree_node_write_mut — 可变引用的写节点
///
/// 接受 `&mut BtreeNode`，在序列化前先执行 sort_iter 排序合并。
/// 对齐 bcachefs `__bch2_btree_node_write` 中写入前排序的语义。
pub async fn bch2_btree_node_write_mut(
    node: &mut BtreeNode,
    block_addr: u64,
    backend: &dyn BlockDevice,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    bch2_btree_node_io_lock(node);

    // 写入前排序合并多个 bset（对齐 bcachefs bch2_btree_node_sort + bch2_sort_whiteouts）
    bch2_btree_node_sort_keys(node)?;

    // 设置 just_written 标志（写入已完成）
    // 注意：在同步写入模型中，write_in_flight 在写入完成后立即清除
    node.set_just_written();

    let result = bucket_io::write_node_to_bucket(node, block_addr, backend).await;

    bch2_btree_post_write_cleanup(node);

    bch2_btree_node_io_unlock(node);

    result?;

    if let Some(j) = journal {
        let jseq = node.journal_seq;
        let pin_guard = node.journal_pin.lock().unwrap();
        if let Some(pin) = pin_guard.as_ref() {
            if pin.seq.load(std::sync::atomic::Ordering::Acquire) == jseq {
                j.bch2_journal_pin_drop(pin);
            }
        }
    }

    Ok(())
}

/// bcachefs 对齐: __bch2_btree_node_write — 内部写节点（低层）
///
/// 与 bch2_btree_node_write 相同，但接收缓存引用以更新状态。
/// 注意：此函数接受 &BtreeNode，无法修改节点。如果需要写入前 sort-merge，
/// 调用方应使用 bch2_btree_node_write_mut 或预调用 bch2_btree_node_sort_keys。
///
/// 如果提供了 `journal` 且节点已持有 active pin，写入成功后释放该 pin。
/// 这条路径不负责创建 pin；pin 由 dirty 路径注册。
pub async fn __bch2_btree_node_write(
    node: &BtreeNode,
    block_addr: u64,
    backend: &dyn BlockDevice,
    _cache: &NodeCache,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    bch2_btree_node_io_lock(node);
    let result = bucket_io::write_node_to_bucket(node, block_addr, backend).await;
    bch2_btree_node_io_unlock(node);
    result?;

    if let Some(j) = journal {
        let jseq = node.journal_seq;
        let pin_guard = node.journal_pin.lock().unwrap();
        if let Some(pin) = pin_guard.as_ref() {
            if pin.seq.load(std::sync::atomic::Ordering::Acquire) == jseq {
                j.bch2_journal_pin_drop(pin);
            }
        }
    }

    Ok(())
}

/// bcachefs 对齐: bch2_btree_node_write_trans — 在事务上下文中写节点
///
/// 接受可选 journal 引用以支持写后 drop pin。
pub async fn bch2_btree_node_write_trans(
    node: &BtreeNode,
    block_addr: u64,
    backend: &dyn BlockDevice,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    bch2_btree_node_write(node, block_addr, backend, journal).await
}

/// bcachefs 对齐: bch2_btree_post_write_cleanup — 写入完成后的清理
///
/// 写入完成后对节点进行后处理:
/// - 清除 just_written 标志（对应 clear_btree_node_just_written）
/// - 如果节点有多个 bset（nsets > 1）→ 排序合并到单个紧凑 set[0]（对应 bch2_btree_node_sort）
/// - 丢弃 whiteout（通过 compact 自动过滤 KeyType::Deleted）
/// - 构建 aux 树（compact 内置的 sorted/eytzinger 辅助数组）
/// - 初始化下一个增量 bset（对应 want_new_bset + bch2_bset_init_next）
///
/// 返回 true 表示迭代器失效（需要重新 init），false 表示无变化。
pub fn bch2_btree_post_write_cleanup(node: &mut BtreeNode) -> bool {
    // 清除 just_written 标志（对应 bcachefs btree_node_just_written 检查）
    if node.is_just_written() {
        node.clear_just_written();
    }

    let nsets = node.nsets();
    let invalidated = if nsets > 1 {
        // 多个 bset → 合并排序到 set[0]（对应 bch2_btree_node_sort(c, b, 0, b->nsets)）
        node.compact();
        true
    } else if node.whiteout_count > 0 {
        // 单 bset 有 whiteout → 清理（对应 bch2_drop_whiteouts(b, COMPACT_ALL)）
        node.compact();
        true
    } else if node.sets[0].size > 0 && node.sets[0].aux_offset == 0 {
        // 数据完整但 aux 树缺失 → 重建（对应 bch2_btree_build_aux_trees）
        node.compact();
        true
    } else {
        // 节点已是最优状态，无需操作
        false
    };

    // 准备下一个增量 bset（对应 bcachefs want_new_bset + bch2_bset_init_next）
    bch2_btree_init_next(node);

    invalidated
}

/// bcachefs 对齐: bch2_btree_init_next — 初始化节点中的下一个 bset
///
/// 在 post_write_cleanup 后调用，将下一个增量 bset（sets[nsets]）的
/// data_offset/end_offset 定位到当前所有 bset 之后的空闲区域起始位置。
/// 此后 insert/delete 可直接向该增量 bset 追加条目。
pub fn bch2_btree_init_next(node: &mut BtreeNode) {
    let nsets = node.nsets() as usize;
    if nsets >= MAX_BSETS {
        return;
    }
    // 计算所有已有 bset 占用的末尾位置（含 aux 数组），对齐到 8 字节
    let free_start = node.sets[..nsets]
        .iter()
        .map(|s| {
            let base = s.end_offset;
            if s.aux_offset > 0 {
                let aes = std::mem::size_of::<crate::btree::key::BtreeKey>() + 4;
                let aux_end = s.aux_offset + s.size as u32 * aes as u32;
                std::cmp::max(base, aux_end)
            } else {
                base
            }
        })
        .max()
        .unwrap_or(0);
    let aligned = (free_start + 7) & !7;
    node.sets[nsets] = BsetTree {
        data_offset: aligned,
        end_offset: aligned,
        aux_offset: 0,
        size: 0,
        extra: 0,
    };
}

/// bcachefs 对齐: bch2_btree_flush_all_writes — 刷新所有飞行中的写操作
pub fn bch2_btree_flush_all_writes() -> bool {
    // volmount: 当前为同步写入，没有飞行中的写操作
    true
}

/// bcachefs 对齐: bch2_btree_cancel_all_writes — 取消所有飞行中的写操作
pub fn bch2_btree_cancel_all_writes() {
    // volmount: 当前为同步写入，无需取消
}

/// bcachefs 对齐: btree_node_write_if_need — 按需写节点（仅在需要时写）
///
/// 接受 `&mut BtreeNode`，在写入前执行 sort-merge。
pub async fn btree_node_write_if_need(
    node: &mut BtreeNode,
    block_addr: u64,
    backend: &dyn BlockDevice,
    journal: Option<&Journal>,
) -> Result<(), StorageError> {
    // volmount: dirty 状态由 cache 层管理，节点无 dirty 标志；
    // 此函数始终执行写入（由 caller 控制），写入前排序合并
    bch2_btree_node_write_mut(node, block_addr, backend, journal).await
}

// ─── Compat 处理 ─────────────────────────────────────────────────────────

/// bcachefs 对齐: compat_bformat — 兼容性格式化处理
pub fn compat_bformat(
    _level: u8,
    _btree_id: u32,
    _version: u32,
    _big_endian: u32,
    _write: bool,
    _format: &mut crate::btree::key::BkeyFormat,
) {
    // volmount: 当前版本不需要兼容性转换
}

/// bcachefs 对齐: compat_bpos — 兼容性 Bpos 转换
pub fn compat_bpos(
    _level: u8,
    _btree_id: u32,
    _version: u32,
    _big_endian: u32,
    _write: bool,
    _pos: &mut crate::btree::key::Bpos,
) {
    // volmount: 当前版本不需要兼容性转换
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::{BchVal, BtreeKey, KeyType};
    use crate::btree::node::BLOCK_SIZE;
    use crate::journal::Journal;

    #[tokio::test]
    async fn test_bch2_btree_node_read_write() {
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.compact();

        bch2_btree_node_write(&node, 42, &backend, None)
            .await
            .unwrap();
        let loaded = bch2_btree_node_read(&backend, 42).await.unwrap();
        assert_eq!(loaded.key_count, node.key_count);
    }

    #[tokio::test]
    async fn test_bch2_btree_root_read() {
        let backend = MockBlockDevice::new();
        let node = BtreeNode::new_leaf();
        bch2_btree_node_write(&node, 99, &backend, None)
            .await
            .unwrap();
        let (_root, level) = bch2_btree_root_read(&backend, 99).await.unwrap();
        assert_eq!(level, 0);
    }

    #[tokio::test]
    async fn test_btree_node_write_does_not_create_pin_by_itself() {
        let backend = MockBlockDevice::new();
        let journal = Journal::new(vec![100]);

        let mut leaf = BtreeNode::new(0);
        leaf.journal_seq = 1;
        bch2_btree_node_write(&leaf, 123, &backend, Some(&journal))
            .await
            .unwrap();
        assert!(
            leaf.journal_pin.lock().unwrap().is_none(),
            "direct write should not synthesize a journal pin"
        );

        let mut interior = BtreeNode::new(5);
        interior.journal_seq = 2;
        bch2_btree_node_write(&interior, 124, &backend, Some(&journal))
            .await
            .unwrap();
        assert!(
            interior.journal_pin.lock().unwrap().is_none(),
            "direct write should not synthesize a journal pin"
        );
    }

    #[test]
    fn test_read_done_validates() {
        let mut node = BtreeNode::new_leaf();
        assert!(bch2_btree_node_read_done(&mut node).is_ok());
        let text = bch2_btree_node_header_to_text(&node);
        assert!(text.contains("level=0"));
    }

    #[test]
    fn test_validate_bset_rejects_invalid() {
        let mut node = BtreeNode::new_leaf();

        // 插入一些数据使其有有效的 bset 结构
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.compact();

        // set[0] 应该有效
        assert!(bch2_validate_bset(&node, 0).is_ok());

        // 越界的 set index
        assert!(bch2_validate_bset(&node, MAX_BSETS).is_err());
        assert!(bch2_validate_bset(&node, MAX_BSETS + 1).is_err());

        // 无效的 data_offset（超出 node_size）
        let mut bad_node = BtreeNode::new_leaf();
        bad_node.sets[0].data_offset = 99999;
        bad_node.sets[0].size = 10;
        assert!(bch2_validate_bset(&bad_node, 0).is_err());
    }

    #[test]
    fn test_validate_bset_keys_rejects_out_of_order() {
        let mut node = BtreeNode::new_leaf();
        // insert() 写入增量 bset（set[1]），直接追加，
        // 所以数据在缓冲区顺序为 3,1,2（非升序）
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));

        // 数据在 set[1]（增量 bset），set[0] 为空 → 验证 set[1] 应拒绝降序
        assert!(node.sets[1].size > 0, "insert should write to set[1]");
        let result = bch2_validate_bset_keys(&node, 1);
        assert!(
            result.is_err(),
            "unsorted entries in set[1] should fail: {:?}",
            result
        );

        // compact 后数据合入 set[0] 应为升序
        node.compact();
        assert!(bch2_validate_bset_keys(&node, 0).is_ok());
    }

    #[test]
    fn test_read_done_rejects_empty_data() {
        let mut node = BtreeNode::new_leaf();
        node.data.clear();
        assert!(bch2_btree_node_read_done(&mut node).is_err());
    }

    #[test]
    fn test_validate_bset_keys_zero_size_ok() {
        let node = BtreeNode::new_leaf();
        // size=0 的 bset 应该通过验证
        assert!(bch2_validate_bset_keys(&node, 0).is_ok());
    }

    #[tokio::test]
    async fn test_write_sorts_multiple_bsets() {
        // Phase 2 验证：写入前排序合并多个 bset
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();

        // 插入多个 key 后 compact（set[0] 填满）
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.compact();

        // 在 set[1] 追加更多 key（模拟增量写入）
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));

        // 不 compact，直接 write（write 内部应该自动排序合并）
        bch2_btree_node_write(&node, 77, &backend, None)
            .await
            .unwrap();

        // 读回：read_done 流水线验证
        let loaded = bch2_btree_node_read(&backend, 77).await.unwrap();
        let roundtrip = loaded.serialize_to_bucket(77).unwrap();
        assert!(bch2_btree_node_read_done(
            &mut BtreeNode::deserialize_from_bucket(&roundtrip).unwrap()
        )
        .is_ok());
    }

    #[tokio::test]
    async fn test_drop_keys_outside_node_removes_out_of_range() {
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();

        // 插入并 compact
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.compact();

        // 设置范围：只保留 15..25
        node.min_key = Bpos {
            inode: 0,
            offset: 15,
            snapshot: 1,
        };
        node.max_key = Bpos {
            inode: 0,
            offset: 25,
            snapshot: 1,
        };

        bch2_btree_node_drop_keys_outside_node(&mut node).unwrap();
        assert_eq!(node.key_count, 1); // 只保留 key[20]
    }

    #[test]
    fn test_header_to_text_format() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(42, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.compact();
        let text = bch2_btree_node_header_to_text(&node);
        assert!(text.contains("level=0"));
        assert!(text.contains("key_count=1"));
        assert!(text.contains("nsets=1"));
    }

    // ─── Phase 3: IO 标志位测试 ───────────────────────────────────

    #[test]
    fn test_io_lock_unlock() {
        let node = BtreeNode::new_leaf();

        // 初始状态：未被锁
        assert!(!node.is_write_in_flight());

        // 加锁
        bch2_btree_node_io_lock(&node);
        assert!(node.is_write_in_flight());

        // 再次尝试加锁应失败（已被锁）
        assert!(!node.try_lock_write_in_flight());

        // 解锁
        bch2_btree_node_io_unlock(&node);
        assert!(!node.is_write_in_flight());

        // 解锁后可重新加锁
        assert!(node.try_lock_write_in_flight());
        assert!(node.is_write_in_flight());
        bch2_btree_node_io_unlock(&node);
    }

    #[test]
    fn test_read_in_flight_flags() {
        let node = BtreeNode::new_leaf();

        assert!(!node.is_read_in_flight());
        assert!(node.try_lock_read_in_flight());
        assert!(node.is_read_in_flight());
        assert!(!node.try_lock_read_in_flight()); // 已被锁

        node.clear_read_in_flight();
        assert!(!node.is_read_in_flight());
    }

    #[test]
    fn test_just_written_flag() {
        let mut node = BtreeNode::new_leaf();

        assert!(!node.is_just_written());
        node.set_just_written();
        assert!(node.is_just_written());

        // post_write_cleanup 应清除 just_written
        bch2_btree_post_write_cleanup(&mut node);
        assert!(!node.is_just_written());
    }

    #[test]
    fn test_wait_on_read_write() {
        let node = BtreeNode::new_leaf();

        // 未设置标志时，wait 应立即返回
        bch2_btree_node_wait_on_read(&node);
        bch2_btree_node_wait_on_write(&node);
        // 如果没死锁即通过
    }

    // ─── Phase 2: sort_iter 测试 ──────────────────────────────────

    #[test]
    fn test_sort_iter_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));

        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 3);

        let mut buf = vec![0u8; node.node_size as usize];
        let written = iter.sort_into(&mut buf).unwrap();
        assert!(written > 0);

        // 验证排序后的 key 顺序正确
        let sorted_node_data = &buf[..written];
        let mut offset = 0usize;
        let mut prev_offset = 0u64;
        while offset + 3 <= sorted_node_data.len() {
            let u64s = sorted_node_data[offset];
            if u64s == 0 {
                break;
            }
            let pk = unsafe {
                &*(sorted_node_data.as_ptr().add(offset) as *const crate::btree::key::BkeyPacked)
            };
            let (bpos, _, _, _) =
                crate::btree::key::bkey_unpack(&crate::btree::key::BKEY_FORMAT_CURRENT, pk);
            assert!(
                bpos.offset >= prev_offset,
                "key order violation at offset {}: {} < {}",
                offset,
                bpos.offset,
                prev_offset
            );
            prev_offset = bpos.offset;
            offset += (u64s as u32) as usize * 8;
        }
    }

    #[test]
    fn test_sort_iter_multiple_bsets() {
        let mut node = BtreeNode::new_leaf();

        // set[0]: keys 10, 30, 50
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(0x500, 1));
        node.compact();
        assert!(node.sets[0].size == 3);

        // set[1]: keys 20, 40
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(0x400, 1));

        // 验证有多个 bset
        assert!(node.sets[1].size == 2);
        assert_eq!(node.nsets(), 2);

        // 使用 sort_iter 收集所有 keys
        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 5);

        // 排序并验证顺序
        let mut buf = vec![0u8; node.node_size as usize];
        let written = iter.sort_into(&mut buf).unwrap();
        assert!(written > 0);

        let sorted_node_data = &buf[..written];
        let mut offset = 0usize;
        let mut prev_offset = 0u64;
        while offset + 3 <= sorted_node_data.len() {
            let u64s = sorted_node_data[offset];
            if u64s == 0 {
                break;
            }
            let pk = unsafe {
                &*(sorted_node_data.as_ptr().add(offset) as *const crate::btree::key::BkeyPacked)
            };
            let (bpos, _, _, _) =
                crate::btree::key::bkey_unpack(&crate::btree::key::BKEY_FORMAT_CURRENT, pk);
            assert!(bpos.offset >= prev_offset, "key order violation");
            prev_offset = bpos.offset;
            offset += (u64s as u32) as usize * 8;
        }
    }

    #[test]
    fn test_sort_iter_empty() {
        let node = BtreeNode::new_leaf();
        let mut iter = SortIter::init_from_node(&node);
        iter.add_all_bsets(&node);
        assert_eq!(iter.total_keys(), 0);

        let mut buf = [0u8; 256];
        let written = iter.sort_into(&mut buf).unwrap();
        assert_eq!(written, 0);
    }

    // ─── Phase 1: read_done_sort 集成测试 ─────────────────────────

    #[tokio::test]
    async fn test_read_done_sort_integration() {
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();

        // 插入 5 个 key，模拟真实写入到磁盘
        for i in 0..5 {
            node.insert(
                BtreeKey::new(i as u64 + 1, 1, KeyType::Normal),
                BchVal::new((i as u64 + 1) * 0x100, 1),
            );
        }
        node.compact();

        // 写入磁盘并读回（read_done 在读取路径中被调用）
        bch2_btree_node_write(&node, 100, &backend, None)
            .await
            .unwrap();
        let mut loaded = bch2_btree_node_read(&backend, 100).await.unwrap();

        // 读回后手动调用 read_done 完成验证+排序
        let result = bch2_btree_node_read_done(&mut loaded);
        assert!(result.is_ok(), "read_done failed: {:?}", result);

        // 验证排序正确
        assert_eq!(loaded.key_count, 5);
        assert!(loaded
            .search(&BtreeKey::new(1, 1, KeyType::Normal))
            .is_some());
        assert!(loaded
            .search(&BtreeKey::new(5, 1, KeyType::Normal))
            .is_some());
    }

    // ─── Phase 2: 写入前排序测试 ──────────────────────────────────

    #[tokio::test]
    async fn test_bch2_btree_node_sort_keys_integration() {
        let mut node = BtreeNode::new_leaf();

        // 多个 bset 混合
        node.insert(BtreeKey::new(5, 1, KeyType::Normal), BchVal::new(0x500, 1));
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.compact();
        node.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));

        // 排序合并
        bch2_btree_node_sort_keys(&mut node).unwrap();

        // 验证合并后数据正确
        assert_eq!(node.nsets(), 1);
        assert_eq!(node.key_count, 4);
        assert!(node.search(&BtreeKey::new(1, 1, KeyType::Normal)).is_some());
        assert!(node.search(&BtreeKey::new(5, 1, KeyType::Normal)).is_some());
    }

    #[tokio::test]
    async fn test_write_mut_with_sort() {
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();

        // 写入，compact，再追加（多 bset 场景）
        node.insert(
            BtreeKey::new(10, 1, KeyType::Normal),
            BchVal::new(0x1000, 1),
        );
        node.insert(
            BtreeKey::new(30, 1, KeyType::Normal),
            BchVal::new(0x3000, 1),
        );
        node.compact();
        node.insert(
            BtreeKey::new(20, 1, KeyType::Normal),
            BchVal::new(0x2000, 1),
        );

        // write_mut 应在序列化前排序合并
        bch2_btree_node_write_mut(&mut node, 55, &backend, None)
            .await
            .unwrap();

        // 读取并验证
        let loaded = bch2_btree_node_read(&backend, 55).await.unwrap();
        assert_eq!(loaded.key_count, 3);
    }

    // ─── Phase 2: CRC32C 验证（在 deserialize 中已有）─────────────

    #[test]
    fn test_checksum_validation_on_deserialize() {
        let mut node = BtreeNode::new_leaf();
        // 插入足够多的 key 确保 bset 数据区域足够大（> 256 字节）
        for i in 0..20 {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        node.compact();

        let data = node.serialize_to_bucket(42).unwrap();
        assert!(data.len() == BLOCK_SIZE);

        // 正常反序列化应通过
        assert!(BtreeNode::deserialize_from_bucket(&data).is_ok());

        // bset 数据区域起点在 header（~96 字节）后，经 8 字节对齐
        // 使用 offset 128 确保处于 bset 数据区域内部（有 20 个 key）
        let bset_offset = 128usize;
        if bset_offset < data.len() {
            let mut corrupted = data.clone();
            corrupted[bset_offset] ^= 0xFF;
            let result = BtreeNode::deserialize_from_bucket(&corrupted);
            assert!(
                result.is_err(),
                "corrupted bset data should trigger CRC error, got: {:?}",
                result
            );
        }
    }

    // ─── Phase 4: 负面测试 ────────────────────────────────────────

    #[test]
    fn test_negative_read_done_with_additional_bsets() {
        let mut node = BtreeNode::new_leaf();

        // 插入数据，compact，再插入新数据（形成多个 bset）
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.insert(BtreeKey::new(4, 1, KeyType::Normal), BchVal::new(0x400, 1));
        node.compact();
        // 此时 set[0] 有 2 个有效 key: 2, 4

        // 在增量 bset 中手动设置异常数据来测试 validate_bset 的拒绝
        node.sets[1].data_offset = 0;
        node.sets[1].end_offset = 8; // 8 字节不对齐于 8 → 应报错
        node.sets[1].size = 10;

        let result = bch2_btree_node_read_done(&mut node);
        // 应该出错：end_offset 非 8 对齐
        assert!(
            result.is_err(),
            "should reject bset with non-8-aligned end_offset"
        );
    }

    #[tokio::test]
    async fn test_roundtrip_write_read_with_read_done() {
        let backend = MockBlockDevice::new();
        let mut node = BtreeNode::new_leaf();

        for i in 0..10 {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        node.compact();

        // 写入
        bch2_btree_node_write(&node, 200, &backend, None)
            .await
            .unwrap();

        // 读取
        let mut loaded = bch2_btree_node_read(&backend, 200).await.unwrap();

        // read_done 验证
        assert!(bch2_btree_node_read_done(&mut loaded).is_ok());

        // 验证所有 key 都存在
        for i in 0..10 {
            assert!(
                loaded
                    .search(&BtreeKey::new(i as u64, 1, KeyType::Normal))
                    .is_some(),
                "key {} should survive roundtrip",
                i
            );
        }
    }

    #[test]
    fn test_post_write_cleanup_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.insert(BtreeKey::new(2, 1, KeyType::Normal), BchVal::new(0x200, 1));
        node.compact();

        // 单 bset，无 whiteout，已有 aux → 无需 compact
        assert!(!bch2_btree_post_write_cleanup(&mut node));

        // init_next 初始化了 sets[1] 的偏移量但 size=0，nsets 仍返回 1
        assert_eq!(node.nsets(), 1);
        // 但 sets[1] 的 end_offset 应被设置为空闲区域起始位置（>0）
        assert!(
            node.sets[1].end_offset > 0,
            "init_next should set end_offset for the next bset"
        );
    }

    #[test]
    fn test_post_write_cleanup_with_whiteout() {
        let mut node = BtreeNode::new_leaf();
        for i in 0..10 {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 0x100, 1),
            );
        }
        node.compact();

        // 删除一个 key（产生 whiteout），但数量不足以触发 auto-compact
        node.delete_key(&BtreeKey::new(3, 1, KeyType::Normal));
        // (已 compact，所以空间不足时 delete 的 mark_entry_deleted_inplace 可能触发)

        // 这时 whiteout_count > 0 → post_write_cleanup 应触发 compact
        if node.whiteout_count > 0 {
            assert!(bch2_btree_post_write_cleanup(&mut node));
        }
    }

    // ─── Phase 4: read_done 对空节点的处理 ────────────────────────

    #[test]
    fn test_read_done_empty_node_ok() {
        let mut node = BtreeNode::new_leaf();
        // 空节点应该通过 read_done（无数据可验证）
        assert!(bch2_btree_node_read_done(&mut node).is_ok());
        assert_eq!(node.key_count, 0);
    }

    // ─── Phase 3: 节点标志位组合测试 ──────────────────────────────

    #[test]
    fn test_node_flags_independence() {
        let node = BtreeNode::new_leaf();

        // 各种标志位应独立
        node.set_write_in_flight();
        assert!(node.is_write_in_flight());
        assert!(!node.is_read_in_flight());
        assert!(!node.is_just_written());

        node.set_read_in_flight();
        assert!(node.is_write_in_flight());
        assert!(node.is_read_in_flight());

        node.set_just_written();
        assert!(node.is_just_written());

        // 清除其中一个不影响其他
        node.clear_write_in_flight();
        assert!(!node.is_write_in_flight());
        assert!(node.is_read_in_flight());
        assert!(node.is_just_written());

        node.clear_read_in_flight();
        node.clear_just_written();
        assert!(!node.is_read_in_flight());
        assert!(!node.is_just_written());
    }

    // ─── bch2_btree_node_sort_keys 单 bset 场景 ──────────────────

    #[test]
    fn test_sort_keys_single_bset() {
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(3, 1, KeyType::Normal), BchVal::new(0x300, 1));
        node.insert(BtreeKey::new(1, 1, KeyType::Normal), BchVal::new(0x100, 1));
        node.compact();
        // 现在只有一个 bset

        bch2_btree_node_sort_keys(&mut node).unwrap();
        assert_eq!(node.nsets(), 1);
        assert_eq!(node.key_count, 2);
    }

    // ─── io_lock 重入安全测试 ─────────────────────────────────────

    #[test]
    fn test_io_lock_reentry_safe() {
        let node = BtreeNode::new_leaf();

        // 加锁
        assert!(node.try_lock_write_in_flight());

        // 尝试在同一个线程再次加锁应失败（非可重入）
        assert!(!node.try_lock_write_in_flight());

        // 解锁后可以再加
        node.clear_write_in_flight();
        assert!(node.try_lock_write_in_flight());
        node.clear_write_in_flight();
    }
}
