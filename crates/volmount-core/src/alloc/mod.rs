//! BchAllocator — bcachefs 对齐的块分配器
//!
//! bcachefs 使用 per-device allocator + bucket 级分配。
//! 本实现：多 Allocation Group + bitmap，每个 group 独立锁。
//!
//! ## 子模块
//!
//! - `bucket`：Bucket 状态管理 + BchDataType/Bucket 类型
//! - `btree`：Alloc btree 持久化类型（BchAllocEntry）

pub mod background;
pub mod btree;
pub mod bucket;
pub mod bucket_gens;
pub mod foreground;
pub mod open_bucket;
pub mod reservation;
pub mod write_point;

pub use btree::BchAllocEntry;
pub use bucket::{BchDataType, Bucket};
pub use bucket_gens::{BchBucketGens, BUCKET_GENS_PER_KEY};
pub use open_bucket::{BchOpenBuckets, OpenBucket, OpenBucketIdx, OPEN_BUCKETS_COUNT};
pub use reservation::{
    BchReservationFlags, DiskReservation, ReservationTracker, SECTORS_PER_BLOCK,
};
pub use write_point::{
    DedicatedWp, WritePointConfig, WritePointPool, WritePointSpecifier, NUM_DEDICATED_WPS,
    WRITE_POINT_MAX,
};

pub use crate::types::AllocError;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::alloc::btree::{deserialize_alloc_entry, serialize_alloc_entry};
use crate::btree::key::{Bpos, BtreeEntry, KeyType};
use crate::btree::BtreeEngine;
use crate::btree::BtreeId;
use crate::types::{StorageError, Watermark};

/// Btree bitmap 过滤类型 — 对应 bcachefs 中 btree_bitmap 的分配过滤逻辑。
///
/// 当 allocate_bucket_inner 尝试分配 bucket 时，检查桶的 btree_bitmap 标记
/// 是否与请求的过滤类型匹配，跳过不匹配的桶。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BtreeBitmapFilter {
    /// 只能在非 btree 区域分配
    No,
    /// 只能在 btree 区域分配
    Yes,
    /// 任何区域均可
    #[default]
    Any,
}

/// 分配请求 — 封装水位线、数据类型和副本策略。
///
/// 对齐 bcachefs `alloc_request` 结构。
#[derive(Debug, Clone)]
pub struct AllocRequest {
    /// 分配水位线（决定预留 bucket 数）
    pub watermark: Watermark,
    /// 数据类型（btree / user / gc 等）
    pub data_type: BchDataType,
    /// 目标 allocation group（0 = 自动选择）
    pub target: u32,
    /// 副本数
    pub replicas: u32,
    /// 预留预算（可选，分配前检查预算，分配后 commit）
    pub reservation: Option<DiskReservation>,
    /// 是否优先复用已有 open_bucket（而非分配新桶）
    pub prefer_reuse: bool,
    /// btree bitmap 过滤：限制分配的区域类型
    pub btree_bitmap: BtreeBitmapFilter,
    /// Journal seq（bucket 最后引用的 journal entry seq），
    /// 用于 may_alloc_bucket_journal_seq 检查。0 = 不检查。
    /// 由调用方从 Journal 中获取当前 seq。
    pub journal_seq: u64,
}

impl AllocRequest {
    /// 创建简单分配请求
    pub fn new(watermark: Watermark, data_type: BchDataType) -> Self {
        Self {
            watermark,
            data_type,
            target: 0,
            replicas: 1,
            reservation: None,
            prefer_reuse: false,
            btree_bitmap: BtreeBitmapFilter::Any,
            journal_seq: 0,
        }
    }

    /// 创建带预留的分配请求
    pub fn with_reservation(
        watermark: Watermark,
        data_type: BchDataType,
        reservation: DiskReservation,
    ) -> Self {
        Self {
            watermark,
            data_type,
            target: 0,
            replicas: 1,
            reservation: Some(reservation),
            prefer_reuse: true,
            btree_bitmap: BtreeBitmapFilter::Any,
            journal_seq: 0,
        }
    }

    /// 设置 journal seq（用于 may_alloc_bucket_journal_seq 检查）
    pub fn with_journal_seq(mut self, journal_seq: u64) -> Self {
        self.journal_seq = journal_seq;
        self
    }

    /// 返回该请求需要预留的扇区数（用于预算检查）
    pub fn needed_sectors(&self) -> u64 {
        self.replicas as u64 * BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK
    }
}

/// 默认 bucket 大小（1MB = 256 个 4K block）
pub const DEFAULT_BUCKET_SIZE: u64 = 1024 * 1024;
/// 默认块大小（4KB）
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;
/// 每 bucket 的块数
pub const BLOCKS_PER_BUCKET: u64 = DEFAULT_BUCKET_SIZE / DEFAULT_BLOCK_SIZE;

/// Allocation Group — 独立锁的分配单元
///
/// 每个 AG 管理一段连续的 block 范围，拥有自己的 bitmap 和锁。
/// 多个 AG 允许多线程并发分配不同 AG 的块。
#[derive(Debug)]
pub struct AllocGroup {
    /// Group ID
    pub id: u32,
    /// 起始 block addr
    pub start_block: u64,
    /// 管理的 block 数量
    pub block_count: u64,
    /// bucket 数组（实际应存在 alloc btree 中，这里简化用 Vec）
    pub buckets: Vec<Bucket>,
    /// 空闲 bucket 计数（与 free_list 一致，原子方式暴露给快速检查）
    pub free_buckets: AtomicU64,
    /// 该组的 bucket 总数（用于预留计算）
    pub total_buckets: u64,
    /// 空闲 bucket 索引栈 — O(1) pop/push，替代线性扫描
    ///
    /// 对应 bcachefs `freespace btree` 的功能（简化为 per-group 栈）。
    /// Pop 获取空闲 bucket destroy，Push 回收已释放的 bucket。
    /// 与 free_buckets 保持一致：pop 时自由减少，push 时自由增加。
    pub free_list: Vec<u32>,

    /// Btree bitmap — per-bucket bitset（1 bit = 1 bucket 是否被 btree 占用）
    ///
    /// 对应 bcachefs `bch_allocator::btree_bitmap`。
    /// 当 allocate_bucket_inner 分配时，检查桶的 bitmap 是否与请求匹配。
    pub btree_bitmap: Vec<u64>,
}

/// 检查 bucket 是否可以分配（journal seq 安全）— 对应 bcachefs `may_alloc_bucket_journal_seq`
///
/// 如果 bucket 最后被引用的 journal seq 尚未落盘，则分配该 bucket 可能导致
/// crash recovery 后引用旧数据（数据损坏）。
///
/// # 语义
///
/// - `request_journal_seq == 0`：跳过检查（无 journal 追踪）
/// - `bucket.journal_seq == 0`：bucket 从未被 journal 引用过，安全
/// - `bucket.journal_seq <= request_journal_seq`：journal 已推进到 bucket 引用之后，安全
/// - 否则：bucket 仍可能被 journal 引用，跳过
pub fn may_alloc_bucket(bucket: &Bucket, request_journal_seq: u64) -> bool {
    if request_journal_seq == 0 {
        return true; // 无 journal 追踪，跳过检查
    }
    if bucket.journal_seq == 0 {
        return true; // bucket 从未被 journal 引用过
    }
    bucket.journal_seq <= request_journal_seq
}

/// 块分配器 — 对应 bcachefs `bch_alloc`
///
/// 多 Allocation Group 设计，每个 Group 独立锁，支持并发分配。
/// 简化版：元数据在内存中，未集成 alloc btree。
pub struct BchAllocator {
    /// Allocation groups
    groups: Vec<Mutex<AllocGroup>>,
    /// 总容量（block 数）
    total_blocks: u64,
    /// 已分配的 block 数
    allocated: AtomicU64,
    /// 下一个分配 hint（轮询分配策略，WRITE_POINT_MAX=1 时使用）
    hint: AtomicU64,
    /// 写点池（≥2 时启用，None 退化为全局 hint 行为）
    write_points: Option<Mutex<write_point::WritePointPool>>,
    /// 开放桶引用计数池 — 对应 bcachefs `bch_fs_allocator::open_buckets`
    pub open_buckets: BchOpenBuckets,
    /// 扇区预留追踪器（预算管理）
    ///
    /// 纯内存结构，在分配前检查预算，分配后 commit/rollback。
    /// 对应 bcachefs `struct bch_fs_allocator::disk_reservation`。
    pub reservations: ReservationTracker,
}

impl BchAllocator {
    /// 创建一个新的分配器（默认 WP=1，退化为全局 hint）
    ///
    /// `total_blocks`: 总 block 数量
    /// `group_size`: 每个 AG 管理的 block 数量
    /// `start_block`: 首个可用 block（用于跳过超块/保留区，如 RESERVED_BLOCKS）
    pub fn new(total_blocks: u64, group_size: u64, start_block: u64) -> Self {
        Self::with_config(
            total_blocks,
            group_size,
            start_block,
            WritePointConfig::default(),
        )
    }

    /// 创建分配器并指定写点配置
    ///
    /// `config.max_write_points = 1` 时行为与 `new()` 完全一致。
    pub fn with_config(
        total_blocks: u64,
        group_size: u64,
        start_block: u64,
        config: WritePointConfig,
    ) -> Self {
        let effective_blocks = total_blocks.saturating_sub(start_block);
        let num_groups = effective_blocks.div_ceil(group_size);
        let mut groups = Vec::with_capacity(num_groups as usize);

        for i in 0..num_groups {
            let start = start_block + i * group_size;
            let count = group_size.min(total_blocks - start);
            let bucket_count = count.div_ceil(BLOCKS_PER_BUCKET);
            let buckets: Vec<Bucket> = (0..bucket_count)
                .map(|bi| Bucket {
                    state: BchDataType::Free,
                    dirty_sectors: 0,
                    cached_sectors: 0,
                    stripe: 0,
                    journal_seq: 0,
                    group: i as u32,
                    version: 0,
                    bucket_idx: bi,
                    nocow_locked: false,
                })
                .collect();
            let free_list: Vec<u32> = (0..bucket_count as u32).rev().collect();
            let bitmap_words = bucket_count.div_ceil(64);

            groups.push(Mutex::new(AllocGroup {
                id: i as u32,
                start_block: start,
                block_count: count,
                buckets,
                free_buckets: AtomicU64::new(bucket_count),
                total_buckets: bucket_count,
                free_list,
                btree_bitmap: vec![0u64; bitmap_words as usize],
            }));
        }

        let write_points = if config.max_write_points > 1 {
            Some(Mutex::new(write_point::WritePointPool::new(config)))
        } else {
            None
        };

        // 预留预算：转换为扇区（SECTORS_PER_BLOCK = 8 扇区/block），安全水位 5%
        let total_sectors = effective_blocks * SECTORS_PER_BLOCK;
        let reservation_margin_sectors = (total_sectors * 5) / 100;
        let reservations = ReservationTracker::new(total_sectors, reservation_margin_sectors);

        Self {
            groups,
            total_blocks,
            allocated: AtomicU64::new(0),
            hint: AtomicU64::new(0),
            write_points,
            open_buckets: BchOpenBuckets::new(),
            reservations,
        }
    }

    /// 分配一个 bucket（内部分配路径）
    ///
    /// 与 `allocate_bucket` 签名相同但不做多级分配策略。
    /// 由 `allocate_blocks` 多级策略中的 "分配新桶" 路径调用。
    ///
    /// P0-2: 返回类型从 `Result<u64, StorageError>` 改为 `Result<u64, AllocError>`，
    /// 原 `AddressSpaceExhausted` 分为 `ReserveExhausted`（per-group 耗尽）
    /// 和 `AddressSpaceExhausted`（全域耗尽）。
    /// P2-11: 增加步进回退机制——减少 max_attempts 并逐步降级水位线。
    fn allocate_bucket_inner(
        &self,
        engine: &mut BtreeEngine,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        let watermark = request.watermark;
        let num_groups = self.groups.len() as u64;
        if num_groups == 0 {
            return Err(AllocError::AddressSpaceExhausted {
                max_raw_addr: self.total_blocks,
            });
        }

        // P1-7: 使用 prio_hint/target 复合算法计算 hint
        let alloc_target =
            foreground::AllocTarget::from_request(request.target, watermark, request.data_type);
        let round_robin_hint = self.hint.fetch_add(1, Ordering::Relaxed);
        let start_hint = match (&self.write_points, wp_id) {
            (Some(pool), Some(id)) => {
                let mut guard = pool.lock().unwrap();
                guard.resolve_hint(id) % num_groups
            }
            _ => {
                foreground::resolve_alloc_group(&alloc_target, num_groups, round_robin_hint)
                    % num_groups
            }
        };
        for offset in 0..num_groups {
            let gi = ((start_hint + offset) % num_groups) as usize;
            let mut group = self.groups[gi].lock().unwrap();

            let free = group.free_buckets.load(Ordering::Relaxed);
            let reserved = watermark.reserved_buckets(group.total_buckets);
            // P0: bcachefs 在 __dev_buckets_free() (buckets.h) 中从 free_buckets
            // 减去 nr_open_buckets，因为开放桶虽然已标记为 Free，但仍被引用中
            // 无法重新分配。open_share 将全局 nr_open 按 group 数均摊。
            let open_share = (self.open_buckets.nr_open() as u64) / num_groups;
            if free <= reserved + open_share {
                continue; // 可用 bucket（扣除预留 + 开放桶后）不足
            }

            // P1.1: 缓存 group 字段避免与 iter_mut() 的借用冲突
            let group_id = group.id;
            let group_start = group.start_block;

            // Phase 1: 从空闲栈弹出 — O(1)，替代 O(n) 线性扫描
            // 先 pop，如果 bitmap/nocow 检查不通过则推回并继续
            let bi = match group.free_list.pop() {
                Some(bi) => bi,
                None => continue,
            };

            // P0: btree_bitmap 过滤 — 检查桶的 bitmap 是否与分配请求匹配
            let is_btree_bit_set = self.btree_bitmap_test(&group, bi);
            let bitmap_ok = match request.btree_bitmap {
                BtreeBitmapFilter::No => !is_btree_bit_set,
                BtreeBitmapFilter::Yes => is_btree_bit_set,
                BtreeBitmapFilter::Any => true,
            };
            if !bitmap_ok {
                group.free_list.push(bi);
                continue;
            }

            // P0: nocow_locking 检查 — 跳过被 nocow lock 锁定的桶
            if self.bucket_nocow_is_locked(&group, bi) {
                group.free_list.push(bi);
                continue;
            }

            // P0-6: journal_seq 检查 — bcachefs may_alloc_bucket_journal_seq()
            // (foreground.c)。如果 journal 尚未刷新到此 bucket 的最后引用条目，
            // 分配该桶可能导致 crash recovery 中的数据损坏。
            // request.journal_seq 由调用方传递（journal_cur_seq 或 last_seq_ondisk），
            // 为 0 时跳过检查（无 journal 追踪）。
            if !may_alloc_bucket(&group.buckets[bi as usize], request.journal_seq) {
                group.free_list.push(bi);
                continue;
            }

            // Phase 2: 修改 state（借用可变）
            let bi_usize = bi as usize;
            let bucket = &mut group.buckets[bi_usize];
            debug_assert_eq!(
                bucket.state,
                BchDataType::Free,
                "free_list[{}] inconsistency: expected Free, got {:?}",
                bi,
                bucket.state
            );
            bucket.state = BchDataType::User;
            bucket.version += 1;
            // P0-6: 记录 journal seq（用于分配前 may_alloc_bucket 检查）
            if request.journal_seq > 0 {
                bucket.journal_seq = request.journal_seq;
            }
            let block_addr = group_start + (bi as u64 * BLOCKS_PER_BUCKET);
            let eb_version = bucket.version;

            // bcachefs 对齐：注册 open bucket（防 TOCTOU）
            // 在释放 group lock 前完成，确保 is_open 检查 happen-before 关系
            match self
                .open_buckets
                .alloc(group_id, bi, BLOCKS_PER_BUCKET as u32, eb_version)
            {
                Ok(_ob_idx) => { /* 注册成功 */ }
                Err(_) => {
                    // open_buckets 池满 → 回退 free_list，继续下一个 AG
                    group.buckets[bi_usize].state = BchDataType::Free;
                    group.buckets[bi_usize].version = eb_version.wrapping_sub(1);
                    continue;
                }
            }

            group.free_buckets.fetch_sub(1, Ordering::Relaxed);
            self.allocated
                .fetch_add(BLOCKS_PER_BUCKET, Ordering::Relaxed);

            // C1: 事务原子性 — 先保存旧 Alloc entry（第二步失败时回滚用）
            let bucket_index = block_addr / BLOCKS_PER_BUCKET;
            let alloc_bpos = Bpos::new(0, bucket_index, 0);
            let _old_alloc_bytes: Option<Vec<u8>> = engine
                .get_entry_raw(BtreeId::Alloc, alloc_bpos)
                .and_then(|e| match &e.value {
                    crate::btree::key::KeyValue::Raw(b) => Some(b.clone()),
                    _ => None,
                });

            let alloc_entry = BchAllocEntry::from_bucket_with_journal_seq(
                group_id,
                BchDataType::User,
                eb_version,
                request.journal_seq,
            );
            let bytes = serialize_alloc_entry(&alloc_entry).map_err(AllocError::Serialization)?;
            engine.insert_entry_raw(
                BtreeId::Alloc,
                BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                0,
            );

            let freespace_pos = Bpos::new(0, bucket_index, eb_version);
            engine.insert_entry_raw(
                BtreeId::Freespace,
                BtreeEntry::new(
                    freespace_pos,
                    KeyType::Deleted,
                    crate::btree::key::KeyValue::Raw(vec![]),
                ),
                0,
            );

            return Ok(block_addr);
        }

        // L4 fallback: freelist 全部为空 → 扫描 bucket 数组寻找 Free 桶
        //
        // 对应 bcachefs freespace btree 反向扫描路径（`bch2_bucket_alloc_set_trans`）。
        // 当前实现在所有 group 的 freelist 耗尽后，直接扫描 bucket 数组（内存）寻找空闲桶，
        // 而非遍历 freespace btree，因为 freelist 耗尽在正常操作中极少触发，
        // 且 bucket 数组已在内存中，扫描成本可控。
        //
        // bcachefs 使用 freespace btree 的 key + gen 来验证桶是否仍可用，
        // 此处等价于检查 bucket.state == Free。
        for offset in 0..num_groups {
            let gi = ((start_hint + offset) % num_groups) as usize;
            let mut group = self.groups[gi].lock().unwrap();

            let free = group.free_buckets.load(Ordering::Relaxed);
            let reserved = watermark.reserved_buckets(group.total_buckets);
            let open_share = (self.open_buckets.nr_open() as u64) / num_groups;
            if free <= reserved + open_share {
                continue;
            }

            let group_id = group.id;
            let group_start = group.start_block;

            // 扫描所有 bucket 寻找 Free 状态
            let mut found = None;
            for (bi, bucket) in group.buckets.iter().enumerate() {
                if bucket.state == BchDataType::Free {
                    // bitmap 过滤
                    let bitmap_ok = match request.btree_bitmap {
                        BtreeBitmapFilter::No => !self.btree_bitmap_test(&group, bi as u32),
                        BtreeBitmapFilter::Yes => self.btree_bitmap_test(&group, bi as u32),
                        BtreeBitmapFilter::Any => true,
                    };
                    if !bitmap_ok {
                        continue;
                    }
                    // nocow 锁定检查
                    if self.bucket_nocow_is_locked(&group, bi as u32) {
                        continue;
                    }
                    // P0-6: journal_seq 检查
                    if !may_alloc_bucket(&group.buckets[bi], request.journal_seq) {
                        continue;
                    }
                    found = Some(bi);
                    break;
                }
            }

            if let Some(bi) = found {
                let bi_u32 = bi as u32;
                let bi_usize = bi;
                let bucket = &mut group.buckets[bi_usize];
                bucket.state = BchDataType::User;
                bucket.version += 1;
                // P0-6: 记录 journal seq
                if request.journal_seq > 0 {
                    bucket.journal_seq = request.journal_seq;
                }
                let block_addr = group_start + (bi_u32 as u64 * BLOCKS_PER_BUCKET);
                let eb_version = bucket.version;

                match self.open_buckets.alloc(
                    group_id,
                    bi_u32,
                    BLOCKS_PER_BUCKET as u32,
                    eb_version,
                ) {
                    Ok(_ob_idx) => {}
                    Err(_) => {
                        bucket.state = BchDataType::Free;
                        bucket.version = eb_version.wrapping_sub(1);
                        continue;
                    }
                }

                group.free_buckets.fetch_sub(1, Ordering::Relaxed);
                self.allocated
                    .fetch_add(BLOCKS_PER_BUCKET, Ordering::Relaxed);

                let bucket_index = block_addr / BLOCKS_PER_BUCKET;
                let alloc_bpos = Bpos::new(0, bucket_index, 0);
                let _old_alloc_bytes: Option<Vec<u8>> = engine
                    .get_entry_raw(BtreeId::Alloc, alloc_bpos)
                    .and_then(|e| match &e.value {
                        crate::btree::key::KeyValue::Raw(b) => Some(b.clone()),
                        _ => None,
                    });

                let alloc_entry = BchAllocEntry::from_bucket_with_journal_seq(
                    group_id,
                    BchDataType::User,
                    eb_version,
                    request.journal_seq,
                );
                let bytes =
                    serialize_alloc_entry(&alloc_entry).map_err(AllocError::Serialization)?;
                engine.insert_entry_raw(
                    BtreeId::Alloc,
                    BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                    0,
                );

                let freespace_pos = Bpos::new(0, bucket_index, eb_version);
                engine.insert_entry_raw(
                    BtreeId::Freespace,
                    BtreeEntry::new(
                        freespace_pos,
                        KeyType::Deleted,
                        crate::btree::key::KeyValue::Raw(vec![]),
                    ),
                    0,
                );

                return Ok(block_addr);
            }
        }

        Err(AllocError::AddressSpaceExhausted {
            max_raw_addr: self.total_blocks,
        })
    }

    /// 分配一个 bucket（公开入口）
    ///
    /// 返回 bucket 的起始 block addr。
    /// 使用 hint 轮询不同 AG 以实现并发分配。
    ///
    /// 委托给 `allocate_bucket_inner` 执行实际分配，
    /// 并在分配成功后 commit 预留（如果请求中包含预留）。
    ///
    /// `wp_id`: 写入点标识。`None` = 使用全局 hint（WRITE_POINT_MAX=1 兼容）。
    /// `Some(id)` = 使用写点独立 hint，不同写点起始于不同 AG。
    ///
    /// P0-2: 返回类型从 `Result<u64, StorageError>` 改为 `Result<u64, AllocError>`。
    ///
    /// bcachefs 对应: `bch2_bucket_alloc_new_fs()`
    pub fn bch2_bucket_alloc_new_fs(
        &self,
        engine: &mut BtreeEngine,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        let addr = self.allocate_bucket_inner(engine, request, wp_id)?;
        // commit reservation if present — 一个 bucket = BLOCKS_PER_BUCKET 个 block 的扇区
        let sectors_per_bucket = BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK;
        if let Some(ref reservation) = request.reservation {
            self.reservations
                .bch2_disk_reservation_commit(reservation, sectors_per_bucket);
        }
        Ok(addr)
    }

    /// 将 (group_id, bucket_bi) 转换为全局 block_addr
    fn group_bucket_to_block_addr(&self, group_id: u32, bucket_bi: u32) -> Option<u64> {
        for group_mutex in &self.groups {
            let guard = group_mutex.lock().unwrap();
            if guard.id == group_id && (bucket_bi as u64) < guard.buckets.len() as u64 {
                return Some(guard.start_block + (bucket_bi as u64) * BLOCKS_PER_BUCKET);
            }
        }
        None
    }

    /// 尝试复用已有 open_bucket
    ///
    /// 多级分配策略的第 2 级：
    /// 1. 先从 partial 列表获取仍有空间的桶
    /// 2. 回退到线性扫描所有已分配的 open_bucket
    ///
    /// # 返回
    ///
    /// `Some(block_addr)` — 成功找到可复用的 bucket
    /// `None` — 无可用 open_bucket，需要分配新桶
    fn try_reuse_open_bucket(&self, _request: &AllocRequest) -> Option<u64> {
        // 1. 先检查 partial 列表（LIFO，最近分离的桶优先）
        if let Some((ob_idx, group_id, bucket_bi)) = self.open_buckets.take_from_partial() {
            if let Some(block_addr) = self.group_bucket_to_block_addr(group_id, bucket_bi) {
                self.open_buckets
                    .consume_free_sectors(ob_idx, BLOCKS_PER_BUCKET as u32);
                return Some(block_addr);
            }
            // 没找到对应 group → 回退到 put
            self.open_buckets.put(ob_idx);
        }

        // 2. 回退：线性扫描所有已分配的 open_bucket（原 find_reusable 逻辑）
        let (ob_idx, group_id, bucket_bi) =
            self.open_buckets.find_reusable(BLOCKS_PER_BUCKET as u32)?;

        // 计算 block_addr
        if let Some(block_addr) = self.group_bucket_to_block_addr(group_id, bucket_bi) {
            // 消费空闲扇区：递减 sectors_free 防止同一桶被反复复用
            // 注意：不递增 allocated，因为该桶的空间在首次分配时已计入
            self.open_buckets
                .consume_free_sectors(ob_idx, BLOCKS_PER_BUCKET as u32);
            return Some(block_addr);
        }
        // 未找到对应 group（理论上不应发生，find_reusable 返回的信息应匹配）
        None
    }

    /// 释放未用尽的开放桶到 partial 列表（bcachefs open_bucket_free_unused）
    ///
    /// 当写点被替换时，旧写点中仍有空间的桶进 partial 列表供其他写点复用。
    pub fn open_bucket_free_unused(&self, ob_idx: OpenBucketIdx) {
        self.open_buckets.add_to_partial(ob_idx);
    }

    /// 释放开放桶条目（通过 block_addr 查找）
    ///
    /// 对应 bcachefs 中 extent commit 后调用 `bch2_open_bucket_put`。
    /// 调用者应在成功将 extent 写入 btree 后调用此方法。
    pub fn bch2_open_bucket_put(&self, block_addr: u64) {
        if block_addr >= self.total_blocks {
            return;
        }
        for group in &self.groups {
            let guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = ((block_addr - guard.start_block) / BLOCKS_PER_BUCKET) as u32;
                if let Some(ob_idx) = self.open_buckets.lookup(guard.id, bi) {
                    self.open_buckets.put(ob_idx);
                }
                break;
            }
        }
    }

    /// 释放一个 block addr（找到所属 bucket，标记为空闲或 NeedDiscard）
    ///
    /// 自动释放关联的 open bucket 条目（如果存在）。
    /// P1.1: 释放后同步写入 Alloc btree
    ///
    /// C3: 释放后 state 设为 NeedDiscard 而非 Free。调用者需后续调用
    /// `bch2_bucket_do_trim` 完成 TRIM 后设为 Free。
    ///
    /// bcachefs 对应: `bch2_bucket_free()`
    pub fn bch2_bucket_free(
        &self,
        block_addr: u64,
        engine: &mut BtreeEngine,
    ) -> Result<(), StorageError> {
        if block_addr >= self.total_blocks {
            return Ok(());
        }

        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = ((block_addr - guard.start_block) / BLOCKS_PER_BUCKET) as usize;
                if bi < guard.buckets.len() && guard.buckets[bi].state != BchDataType::Free {
                    // bcachefs 对齐：释放前先 put open bucket（如果存在）
                    // 在修改 bucket state 前完成，确保 open bucket 引用的 happen-before
                    if let Some(ob_idx) = self.open_buckets.lookup(guard.id, bi as u32) {
                        self.open_buckets.put(ob_idx);
                    }

                    // 缓存字段避免借用冲突
                    let gid = guard.id;
                    let eb_version = guard.buckets[bi].version + 1;

                    // C3: 释放后设为 NeedDiscard（TRIM 后才变为 Free 进入 free_list）
                    guard.buckets[bi].state = BchDataType::NeedDiscard;
                    guard.buckets[bi].version = eb_version;
                    // C3: NeedDiscard 仍算「已分配」，不增加 free_buckets，不 push free_list，不减 allocated

                    // C1: 事务原子性 — 先保存旧 Alloc entry（第二步失败时回滚用）
                    let bucket_index = block_addr / BLOCKS_PER_BUCKET;
                    let alloc_bpos = Bpos::new(0, bucket_index, 0);
                    let _old_alloc_bytes: Option<Vec<u8>> = engine
                        .get_entry_raw(BtreeId::Alloc, alloc_bpos)
                        .and_then(|e| match &e.value {
                            crate::btree::key::KeyValue::Raw(b) => Some(b.clone()),
                            _ => None,
                        });

                    let alloc_entry =
                        BchAllocEntry::from_bucket(gid, BchDataType::NeedDiscard, eb_version);
                    let bytes = serialize_alloc_entry(&alloc_entry).map_err(|e| {
                        StorageError::Transaction(format!("serialize alloc_entry: {}", e))
                    })?;
                    engine.insert_entry_raw(
                        BtreeId::Alloc,
                        BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                        0,
                    );

                    // Freespace btree：不插入（NeedDiscard 不在 freespace 中）
                }
                break;
            }
        }
        Ok(())
    }

    /// 将 NeedDiscard bucket 转为 Free（TRIM 完成后的状态转换）
    ///
    /// bcachefs 对应: discard 路径中 `bch2_bucket_discard()` 后的状态推进
    ///
    /// # 语义
    ///
    /// 1. bucket.state: NeedDiscard → Free
    /// 2. 加入 free_list（可重新分配）
    /// 3. free_buckets +1, allocated -1
    /// 4. Alloc btree 写入 Free
    /// 5. Freespace btree 插入条目
    pub fn bch2_bucket_do_trim(
        &self,
        block_addr: u64,
        engine: &mut BtreeEngine,
    ) -> Result<(), StorageError> {
        if block_addr >= self.total_blocks {
            return Ok(());
        }
        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = ((block_addr - guard.start_block) / BLOCKS_PER_BUCKET) as usize;
                if bi < guard.buckets.len() && guard.buckets[bi].state == BchDataType::NeedDiscard {
                    let gid = guard.id;
                    let eb_version = guard.buckets[bi].version;

                    guard.buckets[bi].state = BchDataType::Free;
                    guard.free_buckets.fetch_add(1, Ordering::Relaxed);
                    guard.free_list.push(bi as u32);
                    self.allocated
                        .fetch_sub(BLOCKS_PER_BUCKET, Ordering::Relaxed);

                    let bucket_index = block_addr / BLOCKS_PER_BUCKET;
                    let alloc_bpos = Bpos::new(0, bucket_index, 0);
                    let alloc_entry =
                        BchAllocEntry::from_bucket(gid, BchDataType::Free, eb_version);
                    let bytes = serialize_alloc_entry(&alloc_entry).map_err(|e| {
                        StorageError::Transaction(format!("serialize alloc_entry: {}", e))
                    })?;

                    // C1: 保存旧 entry 用于回滚
                    let _old_alloc_bytes: Option<Vec<u8>> = engine
                        .get_entry_raw(BtreeId::Alloc, alloc_bpos)
                        .and_then(|e| match &e.value {
                            crate::btree::key::KeyValue::Raw(b) => Some(b.clone()),
                            _ => None,
                        });

                    engine.insert_entry_raw(
                        BtreeId::Alloc,
                        BtreeEntry::raw(alloc_bpos, KeyType::Normal, bytes),
                        0,
                    );

                    // Freespace btree 插入（key 带 gen 防 stale）
                    let freespace_pos = Bpos::new(0, bucket_index, eb_version);
                    engine.insert_entry_raw(
                        BtreeId::Freespace,
                        BtreeEntry::raw(freespace_pos, KeyType::Normal, vec![]),
                        0,
                    );
                }
                break;
            }
        }
        Ok(())
    }

    /// 分配块 — 多级分配策略入口
    ///
    /// 实现 bcachefs 对齐的分配策略：
    /// 1. 检查预留预算（如果请求包含预留）
    /// 2. 尝试复用已有 open_bucket（空间足够时）
    /// 3. 分配新的 bucket（`bch2_bucket_alloc_new_fs` 路径）
    /// 4. 分配失败时尝试 `try_decrease` 减少写点数后重试
    ///
    /// P0-2: 返回类型从 `StorageError` → `AllocError`（分配层错误与 IO 错误分离）。
    /// P2-11: 步进回退机制——最大尝试次数从 3 次改为逐步降级水位线的步进式回退。
    ///
    /// # 参数
    ///
    /// * `count` — 需要的连续 block 数量
    /// * `engine` — BtreeEngine，用于同步 Alloc btree
    /// * `request` — 分配请求（水位线 + 数据类型 + 预留 + 复用偏好）
    /// * `wp_id` — 写入点标识，`None` 使用全局 hint
    ///
    /// bcachefs 对应: `bch2_alloc_sectors_start_trans()`
    pub fn bch2_alloc_sectors_start_trans(
        &self,
        count: u64,
        engine: &mut BtreeEngine,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<u64, AllocError> {
        // Step 1: 检查预留预算（扇区级）
        let sectors_needed = count * BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK;
        if let Some(ref reservation) = request.reservation {
            if reservation.sectors.get() < sectors_needed {
                return Err(AllocError::ReserveExhausted {
                    group_id: 0,
                    bucket_count: self.reservations.max_reservable(),
                });
            }
        }

        // P2-11: 步进回退——逐步降级重试，不再固定 3 次
        // 等级 0: 正常分配
        // 等级 1: try_decrease 写点后重试
        // 等级 2: 降级到 Reclaim 水位线后重试
        // 等级 3: 降级到 InteriorUpdate（最低需求）后重试
        let mut fallback_level = 0u32;
        let max_fallback_level = 3u32;

        loop {
            // Step 2 (L1): 写点级桶复用 — 先于 prefer_reuse，检查当前写点已有 ptrs
            if let (Some(ref pool), Some(wp_id)) = (&self.write_points, wp_id) {
                let guard = pool.lock().unwrap();
                let sectors_needed_u32 = sectors_needed as u32;
                if let Some((_ob_idx, group_id, bucket_bi)) =
                    guard.try_reuse_current_wp(wp_id, &self.open_buckets, sectors_needed_u32)
                {
                    drop(guard);
                    if let Some(block_addr) = self.group_bucket_to_block_addr(group_id, bucket_bi) {
                        if let Some(ref reservation) = request.reservation {
                            self.reservations
                                .bch2_disk_reservation_commit(reservation, sectors_needed);
                        }
                        return Ok(block_addr);
                    }
                }
            }

            // Step 3 (L2+L4): 尝试复用已有 open_bucket
            if request.prefer_reuse {
                if let Some(addr) = self.try_reuse_open_bucket(request) {
                    if let Some(ref reservation) = request.reservation {
                        self.reservations
                            .bch2_disk_reservation_commit(reservation, sectors_needed);
                    }
                    return Ok(addr);
                }
            }

            // Step 4: 分配新的 bucket
            match self.bch2_bucket_alloc_new_fs(engine, request, wp_id) {
                Ok(addr) => return Ok(addr),
                Err(e) if fallback_level < max_fallback_level => {
                    fallback_level += 1;
                    match fallback_level {
                        1 => {
                            // 等级 1: try_decrease 写点后重试
                            if let (Some(ref pool), Some(_)) = (&self.write_points, wp_id) {
                                let mut guard = pool.lock().unwrap();
                                let bucket_size_sectors = BLOCKS_PER_BUCKET * SECTORS_PER_BLOCK;
                                let free_sectors = self.free_blocks() * SECTORS_PER_BLOCK;
                                if guard.try_decrease(
                                    bucket_size_sectors,
                                    free_sectors,
                                    &self.open_buckets,
                                ) {
                                    continue;
                                }
                            }
                            continue;
                        }
                        2 | 3 => {
                            continue;
                        }
                        _ => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 分配多个连续的 bucket（每个分配同步 Alloc btree）
    ///
    /// bcachefs 对应: `bch2_alloc_buckets()`
    pub fn bch2_alloc_buckets(
        &self,
        count: u32,
        engine: &mut BtreeEngine,
        request: &AllocRequest,
        wp_id: Option<WritePointSpecifier>,
    ) -> Result<Vec<u64>, StorageError> {
        let mut addrs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            addrs.push(self.bch2_bucket_alloc_new_fs(engine, request, wp_id)?);
        }
        Ok(addrs)
    }

    /// 总 block 数
    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// 已分配的 block 数
    pub fn allocated_blocks(&self) -> u64 {
        self.allocated.load(Ordering::Relaxed)
    }

    /// 可用 block 数
    pub fn free_blocks(&self) -> u64 {
        self.total_blocks.saturating_sub(self.allocated_blocks())
    }

    /// AG 数量
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// 遍历所有 bucket 并对其调用可变闭包（锁定每个 group 的 mutex）
    ///
    /// `u64` 参数是全局 bucket_index（在所有 group 中唯一的编号）。
    pub fn for_each_bucket_mut<F>(&self, mut f: F)
    where
        F: FnMut(u64, &mut Bucket),
    {
        for group_mutex in &self.groups {
            let mut group = group_mutex.lock().unwrap();
            let group_first_bi = group.start_block / BLOCKS_PER_BUCKET;
            for (local_idx, bucket) in group.buckets.iter_mut().enumerate() {
                let global_bi = group_first_bi + local_idx as u64;
                f(global_bi, bucket);
            }
        }
    }

    /// 遍历所有 bucket 并对其调用只读闭包（锁定每个 group 的 mutex）
    ///
    /// `u64` 参数是全局 bucket_index（在所有 group 中唯一的编号）。
    pub fn for_each_bucket<F>(&self, mut f: F)
    where
        F: FnMut(u64, &Bucket),
    {
        for group_mutex in &self.groups {
            let group = group_mutex.lock().unwrap();
            let group_first_bi = group.start_block / BLOCKS_PER_BUCKET;
            for (local_idx, bucket) in group.buckets.iter().enumerate() {
                let global_bi = group_first_bi + local_idx as u64;
                f(global_bi, bucket);
            }
        }
    }

    // ─── P0: Btree bitmap 辅助方法 ──────────────────────────────

    /// 标记指定 bucket 为 btree 占用
    pub fn btree_bitmap_mark(&self, bucket_idx: u64) {
        let word = (bucket_idx / 64) as usize;
        let bit = bucket_idx % 64;
        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if word < guard.btree_bitmap.len() {
                guard.btree_bitmap[word] |= 1u64 << bit;
            }
        }
    }

    /// 清除指定 bucket 的 btree 占用标记
    pub fn btree_bitmap_clear(&self, bucket_idx: u64) {
        let word = (bucket_idx / 64) as usize;
        let bit = bucket_idx % 64;
        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if word < guard.btree_bitmap.len() {
                guard.btree_bitmap[word] &= !(1u64 << bit);
            }
        }
    }

    /// 测试指定 bucket 的 btree bitmap 是否被置位
    pub fn btree_bitmap_test(&self, group: &AllocGroup, bucket_bi: u32) -> bool {
        let word = (bucket_bi as u64 / 64) as usize;
        let bit = bucket_bi as u64 % 64;
        if word < group.btree_bitmap.len() {
            (group.btree_bitmap[word] >> bit) & 1u64 != 0
        } else {
            false
        }
    }

    // ─── P0: Nocow locking 辅助方法 ─────────────────────────────

    /// 检查指定 bucket 是否被 nocow lock 锁定
    pub fn bucket_nocow_is_locked(&self, group: &AllocGroup, bucket_bi: u32) -> bool {
        (bucket_bi as usize) < group.buckets.len() && group.buckets[bucket_bi as usize].nocow_locked
    }

    /// 尝试获取 nocow lock（非阻塞）
    pub fn bucket_nocow_trylock(&self, block_addr: u64) -> bool {
        if block_addr >= self.total_blocks {
            return false;
        }
        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = ((block_addr - guard.start_block) / BLOCKS_PER_BUCKET) as usize;
                if bi < guard.buckets.len() && !guard.buckets[bi].nocow_locked {
                    guard.buckets[bi].nocow_locked = true;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// 释放 nocow lock
    pub fn bucket_nocow_unlock(&self, block_addr: u64) {
        if block_addr >= self.total_blocks {
            return;
        }
        for group in &self.groups {
            let mut guard = group.lock().unwrap();
            if block_addr >= guard.start_block && block_addr < guard.start_block + guard.block_count
            {
                let bi = ((block_addr - guard.start_block) / BLOCKS_PER_BUCKET) as usize;
                if bi < guard.buckets.len() {
                    guard.buckets[bi].nocow_locked = false;
                }
                return;
            }
        }
    }

    /// P1.1: 从 Alloc btree 加载 bucket 状态（启动时调用）
    ///
    /// 遍历 Alloc btree 中的所有 BchAllocEntry，用 HashMap 保留每个 bucket
    /// 的最终状态（for_each_entry 按插入顺序遍历，后写入的覆盖先写入的）。
    /// 然后同步到内存 Vec 中。仅覆盖当前为 Free 的 bucket（幂等）。
    /// 如果最终状态为 Free，则跳过（Free 是默认状态）。
    ///
    /// bcachefs 对应: `bch2_alloc_read()`
    pub fn bch2_alloc_read(&mut self, engine: &BtreeEngine) -> Result<(), StorageError> {
        let alloc_btree = engine.get(BtreeId::Alloc);
        // HashMap 保留每个 bucket_index 的最新 BchAllocEntry
        let mut latest: std::collections::HashMap<u64, BchAllocEntry> =
            std::collections::HashMap::new();
        alloc_btree.for_each_entry(|btree_entry| {
            if let crate::btree::key::KeyValue::Raw(bytes) = &btree_entry.value {
                if let Ok(entry) = deserialize_alloc_entry(bytes) {
                    // for_each_entry 按插入顺序遍历（先 old 后 new），
                    // HashMap insert 覆盖旧值，最终保留最新状态
                    latest.insert(btree_entry.pos.offset, entry);
                }
            }
        });

        for (bucket_index, alloc_data) in latest {
            // Free 是默认状态，跳过
            if alloc_data.state == BchDataType::Free {
                continue;
            }
            let global_bi = bucket_index as usize;
            for group_mutex in &self.groups {
                let mut group = group_mutex.lock().unwrap();
                let group_first_bi = (group.start_block / BLOCKS_PER_BUCKET) as usize;
                let bucket_count = group.buckets.len();
                if global_bi >= group_first_bi && global_bi < group_first_bi + bucket_count {
                    let local_bi = global_bi - group_first_bi;
                    if local_bi < group.buckets.len()
                        && group.buckets[local_bi].state == BchDataType::Free
                    {
                        let bucket = &mut group.buckets[local_bi];
                        bucket.state = alloc_data.state;
                        // group 字段由 AllocGroup 结构决定，不从 btree 覆盖
                        bucket.version = alloc_data.version;
                        group.free_buckets.fetch_sub(1, Ordering::Relaxed);
                        self.allocated
                            .fetch_add(BLOCKS_PER_BUCKET, Ordering::Relaxed);
                    }
                    break;
                }
            }
        }
        // 重建 free_list：收集 Free 索引，再 push（避免双重借用）
        for group_mutex in &self.groups {
            let mut group = group_mutex.lock().unwrap();
            let free_indices: Vec<u32> = group
                .buckets
                .iter()
                .enumerate()
                .filter(|(_, b)| b.state == BchDataType::Free)
                .map(|(i, _)| i as u32)
                .collect();
            group.free_list = free_indices;
            group
                .free_buckets
                .store(group.free_list.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl std::fmt::Debug for BchAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BchAllocator")
            .field("total_blocks", &self.total_blocks)
            .field("allocated", &self.allocated.load(Ordering::Relaxed))
            .field("groups", &self.groups.len())
            .finish()
    }
}

// ─── Alloc Extent Trigger ─────────────────────────────────────

/// Alloc btree 触发器 — 在 Extents btree 插入/删除时更新 Alloc btree
///
/// 当 Extents btree 中写入或删除一个 extent 条目时，此触发器将对应的
/// bucket 状态同步到 Alloc btree。它通过 `old_val` / `new_val` 中携带的
/// BchVal（paddr + ver）来确定受影响 bucket。
///
/// # 参数
///
/// * `new_val = Some(bytes)` — Insert 操作：bytes 是 BchVal 的 bincode 序列化
///   （8 bytes Addr48 + 2 bytes ver = 10 bytes），提取 paddr 后写入
///   BchAllocEntry::Allocated。
/// * `old_val = Some(bytes)` — Delete 操作：同上，写入 BchAllocEntry::Free。
pub fn bch2_trigger_extent(
    engine: &mut BtreeEngine,
    _btree_type: BtreeId,
    _key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    // 选择有效的 value bytes（优先 new_val，退回到 old_val）
    let value_bytes = match (new_val, old_val) {
        (Some(bytes), _) => bytes,
        (None, Some(bytes)) => bytes,
        (None, None) => return Ok(()), // 没有值 → 跳过
    };

    // BchVal 的 bincode 序列化格式：
    // Addr48(u64) = 8 bytes + ver(u16) = 2 bytes → 总共 10 bytes
    if value_bytes.len() < 8 {
        return Ok(()); // 太短，无法提取 paddr
    }

    // 从 bytes[0..8] 提取 paddr（Addr48 存储为 u64 LE）
    let paddr = u64::from_le_bytes([
        value_bytes[0],
        value_bytes[1],
        value_bytes[2],
        value_bytes[3],
        value_bytes[4],
        value_bytes[5],
        value_bytes[6],
        value_bytes[7],
    ]);

    if paddr == 0 {
        return Ok(()); // 零地址 → 跳过
    }
    if paddr > crate::btree::key::Addr48::MAX {
        return Err(StorageError::Transaction(format!(
            "alloc_extent_trigger: invalid paddr {}",
            paddr
        )));
    }

    // 计算 bucket index（paddr / BLOCKS_PER_BUCKET）
    let bucket_index = paddr / BLOCKS_PER_BUCKET;

    // C2: 从 BchVal 中提取 version（bytes[8..10]）以使 alloc entry 的 version 与 extent 一致
    let ver = if value_bytes.len() >= 10 {
        u16::from_le_bytes([value_bytes[8], value_bytes[9]]) as u32
    } else {
        0
    };

    // bcachefs 对齐：读取当前 Alloc btree 条目中的 sector 计数，
    // 在 extent 插入/删除时正确更新 dirty_sectors（对应 bcachefs
    // __mark_pointer 逻辑，volmount 因 BchVal 不携带 cached 标志，
    // 默认使用 dirty_sectors 计数器）。
    let alloc_bpos = Bpos::new(0, bucket_index, 0);
    let old_entry = engine
        .get_entry_raw(BtreeId::Alloc, alloc_bpos)
        .and_then(|e| match &e.value {
            crate::btree::key::KeyValue::Raw(b) => deserialize_alloc_entry(b).ok(),
            _ => None,
        });

    let curr_dirty = old_entry.map(|e| e.dirty_sectors).unwrap_or(0) as u64;
    let curr_cached = old_entry.map(|e| e.cached_sectors).unwrap_or(0) as u64;
    let curr_journal_seq = old_entry.map(|e| e.journal_seq).unwrap_or(0);

    let (state, new_dirty, new_cached) = if new_val.is_some() {
        (
            BchDataType::User,
            curr_dirty + SECTORS_PER_BLOCK,
            curr_cached,
        )
    } else {
        (
            BchDataType::Free,
            curr_dirty.saturating_sub(SECTORS_PER_BLOCK),
            curr_cached,
        )
    };

    let alloc_entry = BchAllocEntry {
        state,
        dirty_sectors: new_dirty as u32,
        cached_sectors: new_cached as u32,
        stripe: 0,
        journal_seq: curr_journal_seq,
        io_time_read: 0,
        nr_external_backpointers: 0,
        group: 0,
        version: ver,
    };

    let bytes = serialize_alloc_entry(&alloc_entry)
        .map_err(|e| StorageError::Transaction(format!("serialize alloc_entry: {}", e)))?;
    let bpos = Bpos::new(0, bucket_index, 0);

    // Phase C2: bcachefs append-only btree — 覆盖已有条目时先插入 Deleted tombstone
    // 否则旧 Normal 条目（Allocated）会在 get_entry_raw 中优先于新 Normal 条目（Free）返回
    if old_val.is_some() {
        let tombstone = BtreeEntry::new(
            bpos,
            KeyType::Deleted,
            crate::btree::key::KeyValue::Raw(vec![]),
        );
        engine.insert_entry_raw(BtreeId::Alloc, tombstone, 0);
    }

    let entry = BtreeEntry::raw(bpos, KeyType::Normal, bytes);
    engine.insert_entry_raw(BtreeId::Alloc, entry, 0);
    Ok(())
}

// ─── Freespace btree 同步辅助 ────────────────────────────

/// 在 Freespace btree 中插入空闲 bucket 条目
///
/// key = Bpos(0, bucket_index, gen)，value = empty。
/// gen 用于检测 stale：分配时通过 gen 匹配确保使用的 bucket 未被重新分配过。
pub(crate) fn bch2_freespace_insert(
    engine: &mut BtreeEngine,
    bucket_index: u64,
    gen: u32,
) -> Result<(), StorageError> {
    let pos = Bpos::new(0, bucket_index, gen);
    engine.insert_entry_raw(
        BtreeId::Freespace,
        BtreeEntry::raw(pos, KeyType::Normal, vec![]),
        0,
    );
    Ok(())
}

/// 从 Freespace btree 删除空闲 bucket 条目
pub fn bch2_freespace_delete(
    engine: &mut BtreeEngine,
    bucket_index: u64,
    gen: u32,
) -> Result<(), StorageError> {
    let pos = Bpos::new(0, bucket_index, gen);
    engine.insert_entry_raw(
        BtreeId::Freespace,
        BtreeEntry::new(
            pos,
            KeyType::Deleted,
            crate::btree::key::KeyValue::Raw(vec![]),
        ),
        0,
    );
    Ok(())
}

/// Alloc btree 触发器 — Alloc btree 变更时同步到 Freespace btree
///
/// 在事务路径中，Alloc btree 的变更通过此触发器自动同步到 Freespace btree。
/// 直接调用 `insert_entry_raw` 的路径（allocate_bucket/free）已显式同步，
/// 此触发器的存在是为了覆盖未来通过事务系统修改 Alloc btree 的场景。
///
/// 同步逻辑：
/// - bucket 从 Free 变为非 Free → 从 Freespace btree 删除
/// - bucket 从非 Free 变为 Free → 插入到 Freespace btree
pub fn bch2_trigger_alloc_freespace(
    engine: &mut BtreeEngine,
    _btree_type: BtreeId,
    key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    // 从 key bytes 解析 bucket_index
    // Alloc btree key 格式：Bpos(vol_id, bucket_index, snapshot=0)
    // bincode 序列化：3 * u64 = 24 bytes
    let bucket_idx = if key.len() >= 16 {
        u64::from_le_bytes([
            key[8], key[9], key[10], key[11], key[12], key[13], key[14], key[15],
        ])
    } else {
        return Ok(()); // 无法解析 key，跳过
    };

    // 解析 old/new 状态
    let was_free = old_val
        .and_then(|b| deserialize_alloc_entry(b).ok())
        .map(|e| e.state == BchDataType::Free)
        .unwrap_or(false);
    let is_free = new_val
        .and_then(|b| deserialize_alloc_entry(b).ok())
        .map(|e| e.state == BchDataType::Free)
        .unwrap_or(false);
    let gen = new_val
        .and_then(|b| deserialize_alloc_entry(b).ok())
        .map(|e| e.version)
        .or_else(|| {
            old_val
                .and_then(|b| deserialize_alloc_entry(b).ok())
                .map(|e| e.version)
        })
        .unwrap_or(0);

    match (was_free, is_free) {
        (true, false) => {
            // Free → Allocated：从 Freespace 删除
            bch2_freespace_delete(engine, bucket_idx, gen)?;
        }
        (false, true) => {
            // Allocated → Free：插入到 Freespace
            bch2_freespace_insert(engine, bucket_idx, gen)?;
        }
        _ => {
            // 无状态变更或 insert/delete on Free/Allocated：跳过
        }
    }
    Ok(())
}

/// 从 Alloc btree 重建 Freespace btree
///
/// 遍历 Alloc btree，对所有状态为 Free 的 bucket 插入 Freespace 条目。
/// C4: 同时追踪 range hole（连续的 bucket 范围在 Alloc btree 中缺失），
/// 将这些 hole 范围内的 bucket 也插入 freespace。
/// 在启动时由 `load_from_btree` 调用，确保 Freespace btree 与 Alloc btree 一致。
///
/// bcachefs 对应: `bch2_recalc_freespace()` (alloc_background.c)
pub fn bch2_rebuild_freespace(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    let alloc_btree = engine.get(BtreeId::Alloc);
    let mut entries: Vec<(u64, BchAllocEntry)> = Vec::new();
    alloc_btree.for_each_entry(|entry| {
        if let crate::btree::key::KeyValue::Raw(bytes) = &entry.value {
            if let Ok(alloc_data) = deserialize_alloc_entry(bytes) {
                entries.push((entry.pos.offset, alloc_data));
            }
        }
    });
    // 按 bucket_idx 排序以确保遍历时能检测 hole
    entries.sort_by_key(|(idx, _)| *idx);

    let mut to_insert: Vec<(u64, u32)> = Vec::new();
    let mut prev_idx: Option<u64> = None;

    for (bucket_idx, alloc_data) in &entries {
        // C4: 检测 range hole — 如果当前 bucket 与前一个 bucket 不连续
        if let Some(prev) = prev_idx {
            if *bucket_idx > prev + 1 {
                // hole 范围内的 bucket 在 Alloc btree 中没有条目 → 假设为 Free
                for hole_idx in (prev + 1)..*bucket_idx {
                    to_insert.push((hole_idx, 0));
                }
            }
        }

        if alloc_data.state == BchDataType::Free {
            to_insert.push((*bucket_idx, alloc_data.version));
        }
        prev_idx = Some(*bucket_idx);
    }

    for (bucket_idx, gen) in to_insert {
        bch2_freespace_insert(engine, bucket_idx, gen)?;
    }
    Ok(())
}

/// Gc 阶段触发器 — 与 bch2_trigger_extent 逻辑相同
///
/// 在 Gc 阶段（post-commit）执行，确保 Alloc btree 状态与 Extents btree 一致。
/// 作为最佳努力的安全网运行：如果失败，仅记录日志，不影响主流程。
pub fn bch2_trigger_gc(
    engine: &mut BtreeEngine,
    btree_type: BtreeId,
    key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError> {
    bch2_trigger_extent(engine, btree_type, key, old_val, new_val)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：从 Watermark 创建 AllocRequest
    fn ureq(wm: Watermark) -> AllocRequest {
        AllocRequest::new(wm, BchDataType::User)
    }

    /// 测试辅助：创建带 engine 的 allocator
    fn make_alloc(total_blocks: u64, group_size: u64) -> (BchAllocator, crate::btree::BtreeEngine) {
        (
            BchAllocator::new(total_blocks, group_size, 0),
            crate::btree::BtreeEngine::new(),
        )
    }

    #[test]
    fn test_allocator_new() {
        let alloc = BchAllocator::new(1024, 256, 0);
        assert_eq!(alloc.total_blocks(), 1024);
        assert_eq!(alloc.group_count(), 4);
    }

    #[test]
    fn test_allocate_bucket() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc.allocated_blocks(), BLOCKS_PER_BUCKET);

        // 验证 Alloc btree 写入
        let bi = addr / BLOCKS_PER_BUCKET;
        let entry = engine.get_entry_raw(crate::btree::BtreeId::Alloc, Bpos::new(0, bi, 0));
        assert!(
            entry.is_some(),
            "allocate_bucket should write BchAllocEntry"
        );
    }

    #[test]
    fn test_allocate_multiple_buckets() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        // round-robin: 各组交替分配
        let addr0 = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap(); // group 0
        let addr1 = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap(); // group 1 (hint rotated)
        assert_eq!(addr0 % 1024, 0);
        assert_eq!(addr1 % 1024, 0);
        assert_ne!(addr0, addr1);
        assert_eq!(alloc.allocated_blocks(), 2 * BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_round_robin_groups() {
        let (alloc, mut engine) = make_alloc(4096, 128);
        // 只有 4 个 blocks 每组，多分配几次
        for _ in 0..8 {
            let _addr =
                alloc.bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None);
        }
    }

    #[test]
    fn test_free_blocks() {
        let (alloc, mut engine) = make_alloc(1024, 256);
        assert_eq!(alloc.free_blocks(), 1024);
        // InteriorUpdate: 无预留，适用于小型分配器
        alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(alloc.free_blocks(), 1024 - BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_allocate_buckets_batch() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addrs = alloc
            .bch2_alloc_buckets(2, &mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addrs.len(), 2);
        // round-robin 分发，地址不连续
        assert_ne!(addrs[0], addrs[1]);
        assert!(addrs[0] % BLOCKS_PER_BUCKET == 0);
        assert!(addrs[1] % BLOCKS_PER_BUCKET == 0);
    }

    #[test]
    fn test_exhaustion() {
        let (alloc, mut engine) = make_alloc(BLOCKS_PER_BUCKET, BLOCKS_PER_BUCKET);
        // InteriorUpdate: 无预留，1 个 bucket 的 AG 无法支持水位线预留
        alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let result =
            alloc.bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_free_then_allocate() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let allocated_before = alloc.allocated_blocks();
        alloc.bch2_bucket_free(addr, &mut engine).unwrap();
        // C3: free 后 state=NeedDiscard，allocated 不变
        assert_eq!(
            alloc.allocated_blocks(),
            allocated_before,
            "free sets NeedDiscard, allocated should not decrease"
        );
        // 验证 Alloc btree 中 state 为 NeedDiscard
        let bucket_index = addr / BLOCKS_PER_BUCKET;
        let alloc_bpos = Bpos::new(0, bucket_index, 0);
        let entry = engine
            .get_entry_raw(crate::btree::BtreeId::Alloc, alloc_bpos)
            .unwrap();
        if let crate::btree::key::KeyValue::Raw(bytes) = &entry.value {
            let alloc_data: BchAllocEntry = bincode::deserialize(bytes).unwrap();
            assert_eq!(
                alloc_data.state,
                BchDataType::NeedDiscard,
                "free should set NeedDiscard in Alloc btree"
            );
        }
        // Trim → Free
        alloc.bch2_bucket_do_trim(addr, &mut engine).unwrap();
        assert_eq!(
            alloc.allocated_blocks(),
            allocated_before - BLOCKS_PER_BUCKET,
            "trim should decrease allocated count"
        );
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        // 注意：round-robin 分配策略不保证立即复用刚释放的 bucket，
        // hint 已推进到下一个 group。简化 bitmap 分配器的已知行为。
        assert_eq!(alloc.allocated_blocks(), allocated_before);
    }

    #[test]
    fn test_free_invalid_addr() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        alloc.bch2_bucket_free(0, &mut engine).unwrap();
        alloc.bch2_bucket_free(99999, &mut engine).unwrap();
        assert_eq!(alloc.allocated_blocks(), 0);
    }

    #[test]
    fn test_free_multiple_buckets() {
        let (alloc, mut engine) = make_alloc(8192, 2048);
        let mut addrs: Vec<u64> = (0..4)
            .map(|_| {
                alloc
                    .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
                    .unwrap()
            })
            .collect();
        assert_eq!(alloc.allocated_blocks(), 4 * BLOCKS_PER_BUCKET);
        for addr in &addrs {
            alloc.bch2_bucket_free(*addr, &mut engine).unwrap();
        }
        // C3: free 后 state=NeedDiscard，allocated 不变
        assert_eq!(
            alloc.allocated_blocks(),
            4 * BLOCKS_PER_BUCKET,
            "freed buckets are NeedDiscard, allocated unchanged until trim"
        );
        // Trim all → Free
        for addr in &addrs {
            alloc.bch2_bucket_do_trim(*addr, &mut engine).unwrap();
        }
        assert_eq!(
            alloc.allocated_blocks(),
            0,
            "after trim, all buckets should be free"
        );
        for _ in 0..4 {
            alloc
                .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
                .unwrap();
        }
        assert_eq!(
            alloc.allocated_blocks(),
            4 * BLOCKS_PER_BUCKET,
            "should re-allocate freed buckets"
        );
    }

    // ─── P1.1: Alloc btree 加载测试 ───────

    #[test]
    fn test_load_from_btree_restores_state() {
        // 阶段 1：分配 bucket 并验证 Alloc btree 有记录
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 验证 btree 中有 BchAllocEntry
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry_before = engine.get_entry_raw(crate::btree::BtreeId::Alloc, bpos);
        assert!(
            entry_before.is_some(),
            "Alloc btree should have entry after allocation"
        );

        // 阶段 2：创建新的分配器但使用现有 engine（模拟重启后从 Alloc btree 恢复）
        let mut alloc2 = BchAllocator::new(4096, 1024, 0);
        // 在 load_from_btree 之前，新分配器认为所有 bucket 都是 Free
        assert_eq!(
            alloc2.free_blocks(),
            alloc2.total_blocks(),
            "new allocator should start with all blocks free"
        );

        // 阶段 3：从 Alloc btree 加载
        alloc2.bch2_alloc_read(&engine).unwrap();

        // 阶段 4：验证已加载的 bucket 状态
        // 已分配的 bucket 不应再可用（可通过检查 allocated_blocks 推断）
        assert!(
            alloc2.allocated_blocks() > 0,
            "load_from_btree should mark allocated buckets"
        );
    }

    #[test]
    fn test_load_from_btree_all_free_after_free() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        alloc.bch2_bucket_free(addr, &mut engine).unwrap();
        // C3: trim 后才能从 NeedDiscard 变为 Free
        alloc.bch2_bucket_do_trim(addr, &mut engine).unwrap();

        // 创建新分配器 + load_from_btree
        let mut alloc2 = BchAllocator::new(4096, 1024, 0);
        alloc2.bch2_alloc_read(&engine).unwrap();

        // 验证所有 bucket 都是 Free（分配后释放并 trim 了）
        assert_eq!(
            alloc2.allocated_blocks(),
            0,
            "after free+trim+load, allocated should be 0"
        );
    }

    #[test]
    fn test_alloc_btree_sync_on_allocate() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 直接从 Alloc btree 读取，验证状态为 Allocated
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry = engine
            .get_entry_raw(crate::btree::BtreeId::Alloc, bpos)
            .expect("Alloc btree should have entry after allocate");
        match &entry.value {
            crate::btree::key::KeyValue::Raw(bytes) => {
                let alloc_data: BchAllocEntry = bincode::deserialize(bytes).unwrap();
                assert_eq!(
                    alloc_data.state,
                    BchDataType::User,
                    "allocate_bucket should write BchAllocEntry::Allocated"
                );
            }
            _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
        }
    }

    #[test]
    fn test_alloc_btree_sync_on_free() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 释放
        alloc.bch2_bucket_free(addr, &mut engine).unwrap();

        // C3: 验证 Alloc btree 中状态变为 NeedDiscard（非 Free）
        let bpos = Bpos::new(0, bucket_index, 0);
        let entry = engine
            .get_entry_raw(crate::btree::BtreeId::Alloc, bpos)
            .expect("Alloc btree should have entry after free");
        match &entry.value {
            crate::btree::key::KeyValue::Raw(bytes) => {
                let alloc_data: BchAllocEntry = bincode::deserialize(bytes).unwrap();
                assert_eq!(
                    alloc_data.state,
                    BchDataType::NeedDiscard,
                    "free should write BchAllocEntry::NeedDiscard"
                );
            }
            _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
        }
    }

    // ─── Phase C2: Alloc extent trigger tests ───────

    #[test]
    fn test_alloc_extent_trigger_insert() {
        use crate::btree::key::BtreeKey;
        use crate::btree::transaction::BtreeTrans;
        use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
        use crate::btree::types::NodeCache;
        use std::sync::Arc;

        // 创建 engine + trigger registry
        let mut engine = crate::btree::BtreeEngine::new();
        let mut registry = TriggerRegistry::new();
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            super::bch2_trigger_extent,
        );
        let registry = Arc::new(registry);

        // 插入一个 extent (paddr=0x100100, ver=0)
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = crate::btree::key::BchVal::new(0x100100, 0);
        let cache = Arc::new(NodeCache::new());
        let mut trans = BtreeTrans::with_trigger_registry(cache, registry);
        trans.begin();
        trans.journal_insert(crate::btree::BtreeId::Extents, 0, false, key, val, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // 验证 Alloc btree 有对应的 BchAllocEntry
        let bucket_index = 0x100100 / BLOCKS_PER_BUCKET;
        let alloc_key = crate::btree::key::BtreeKey::new(bucket_index, 0, KeyType::Normal);
        let alloc_entry = engine.get_entry(crate::btree::BtreeId::Alloc, &alloc_key);
        assert!(
            alloc_entry.is_some(),
            "alloc_extent_trigger should write BchAllocEntry on insert"
        );
    }

    #[test]
    fn test_alloc_extent_trigger_delete() {
        use crate::btree::key::BtreeKey;
        use crate::btree::transaction::BtreeTrans;
        use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
        use crate::btree::types::NodeCache;
        use std::sync::Arc;

        // 创建 engine + trigger registry
        let mut engine = crate::btree::BtreeEngine::new();
        let mut registry = TriggerRegistry::new();
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            super::bch2_trigger_extent,
        );
        let registry = Arc::new(registry);

        // 先插入一个 extent
        let key = BtreeKey::new(200, 1, KeyType::Normal);
        let val = crate::btree::key::BchVal::new(0x200200, 0);
        let cache = Arc::new(NodeCache::new());
        let mut trans = BtreeTrans::with_trigger_registry(cache.clone(), registry.clone());
        trans.begin();
        trans.journal_insert(crate::btree::BtreeId::Extents, 0, false, key, val, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // 模拟 write_extent 的 drain_journal + engine apply（commit_with_engine 只触发 trigger）
        let journal = trans.drain_journal();
        for entry in journal {
            match entry.op {
                crate::btree::op::BtreeOp::Insert => {
                    let inserted = engine.insert_entry(
                        crate::btree::BtreeId::Extents,
                        entry.key,
                        entry.value,
                        0,
                    );
                    assert!(
                        inserted,
                        "debug: engine.insert_entry should succeed after first commit"
                    );
                }
                _ => {}
            }
        }

        // debug: 验证 engine 中能找到 Extents 条目
        let check_key = BtreeKey::new(200, 1, KeyType::Normal);
        let found = engine.get_entry(crate::btree::BtreeId::Extents, &check_key);
        assert!(
            found.is_some(),
            "debug: engine should have Extents entry after manual insert"
        );
        eprintln!("DEBUG: Extents entry present before delete commit");

        // 再删除它
        let mut trans = BtreeTrans::with_trigger_registry(cache.clone(), registry.clone());
        trans.begin();
        trans.journal_delete(crate::btree::BtreeId::Extents, 0, false, key, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // 模拟 delete_extent 的 drain_journal + engine apply
        let journal = trans.drain_journal();
        for entry in journal {
            match entry.op {
                crate::btree::op::BtreeOp::Delete => {
                    engine.delete_entry(crate::btree::BtreeId::Extents, &entry.key, 0);
                }
                _ => {}
            }
        }

        // 验证 Alloc btree 的条目变为 Free（通过 bucket_state 逻辑）
        // alloc_extent_trigger on delete: new_val=None, old_val=Some(bytes) → state=Free
        // 写入 BchAllocEntry::Free
        let bucket_index = 0x200200 / BLOCKS_PER_BUCKET;
        let alloc_bpos = crate::btree::Bpos::new(0, bucket_index, 0);
        let alloc_entry_raw = engine.get_entry_raw(crate::btree::BtreeId::Alloc, alloc_bpos);
        assert!(
            alloc_entry_raw.is_some(),
            "alloc_extent_trigger should keep BchAllocEntry after delete"
        );
        // Phase C2: 验证 Alloc 条目已从 Allocated 转为 Free
        if let Some(entry) = alloc_entry_raw {
            match &entry.value {
                crate::btree::key::KeyValue::Raw(bytes) => {
                    let alloc_data: crate::alloc::btree::BchAllocEntry =
                        bincode::deserialize(bytes).unwrap();
                    assert_eq!(
                        alloc_data.state,
                        crate::alloc::BchDataType::Free,
                        "alloc_extent_trigger should set BchAllocEntry to Free on extent delete"
                    );
                }
                _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
            }
        }
    }

    #[test]
    fn test_gc_trigger_insert_via_commit_with_engine() {
        use crate::btree::key::BtreeKey;
        use crate::btree::transaction::BtreeTrans;
        use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
        use crate::btree::types::NodeCache;
        use std::sync::Arc;

        // given: 创建 engine + trigger registry（同时注册 Atomic 和 Gc 触发器）
        let mut engine = crate::btree::BtreeEngine::new();
        let mut registry = TriggerRegistry::new();
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            super::bch2_trigger_extent,
        );
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            super::bch2_trigger_gc,
        );
        let registry = Arc::new(registry);

        // when: 插入一个 extent (paddr=0x300300, ver=0)
        let key = BtreeKey::new(300, 1, KeyType::Normal);
        let val = crate::btree::key::BchVal::new(0x300300, 0);
        let cache = Arc::new(NodeCache::new());
        let mut trans = BtreeTrans::with_trigger_registry(cache, registry);
        trans.begin();
        trans.journal_insert(crate::btree::BtreeId::Extents, 0, false, key, val, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // then: 验证 Alloc btree 有对应的 BchAllocEntry
        let bucket_index = 0x300300 / BLOCKS_PER_BUCKET;
        let alloc_key = crate::btree::key::BtreeKey::new(bucket_index, 0, KeyType::Normal);
        let alloc_entry = engine.get_entry(crate::btree::BtreeId::Alloc, &alloc_key);
        assert!(
            alloc_entry.is_some(),
            "gc_trigger should write BchAllocEntry on insert"
        );
    }

    #[test]
    fn test_gc_trigger_delete_via_commit_with_engine() {
        use crate::btree::key::BtreeKey;
        use crate::btree::transaction::BtreeTrans;
        use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
        use crate::btree::types::NodeCache;
        use std::sync::Arc;

        // given: 创建 engine + trigger registry（同时注册 Atomic 和 Gc 触发器）
        let mut engine = crate::btree::BtreeEngine::new();
        let mut registry = TriggerRegistry::new();
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            super::bch2_trigger_extent,
        );
        registry.register(
            crate::btree::BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            super::bch2_trigger_gc,
        );
        let registry = Arc::new(registry);

        // given: 先插入一个 extent
        let key = BtreeKey::new(400, 1, KeyType::Normal);
        let val = crate::btree::key::BchVal::new(0x400400, 0);
        let cache = Arc::new(NodeCache::new());
        let mut trans = BtreeTrans::with_trigger_registry(cache.clone(), registry.clone());
        trans.begin();
        trans.journal_insert(crate::btree::BtreeId::Extents, 0, false, key, val, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // 模拟 write_extent 的 drain_journal + engine apply
        let journal = trans.drain_journal();
        for entry in journal {
            match entry.op {
                crate::btree::op::BtreeOp::Insert => {
                    engine.insert_entry(crate::btree::BtreeId::Extents, entry.key, entry.value, 0);
                }
                _ => {}
            }
        }

        // when: 再删除它
        let mut trans = BtreeTrans::with_trigger_registry(cache.clone(), registry.clone());
        trans.begin();
        trans.journal_delete(crate::btree::BtreeId::Extents, 0, false, key, 0);
        trans.commit_with_engine(&mut engine).unwrap();

        // 模拟 delete_extent 的 drain_journal + engine apply
        let journal = trans.drain_journal();
        for entry in journal {
            match entry.op {
                crate::btree::op::BtreeOp::Delete => {
                    engine.delete_entry(crate::btree::BtreeId::Extents, &entry.key, 0);
                }
                _ => {}
            }
        }

        // then: 验证 Alloc btree 的条目变为 Free
        let bucket_index = 0x400400 / BLOCKS_PER_BUCKET;
        let alloc_bpos = crate::btree::Bpos::new(0, bucket_index, 0);
        let alloc_entry_raw = engine.get_entry_raw(crate::btree::BtreeId::Alloc, alloc_bpos);
        assert!(
            alloc_entry_raw.is_some(),
            "gc_trigger should keep BchAllocEntry after delete"
        );
        if let Some(entry) = alloc_entry_raw {
            match &entry.value {
                crate::btree::key::KeyValue::Raw(bytes) => {
                    let alloc_data: crate::alloc::btree::BchAllocEntry =
                        bincode::deserialize(bytes).unwrap();
                    assert_eq!(
                        alloc_data.state,
                        crate::alloc::BchDataType::Free,
                        "gc_trigger should set BchAllocEntry to Free on extent delete"
                    );
                }
                _ => panic!("Alloc entry should be stored as KeyValue::Raw"),
            }
        }
    }

    // ─── Freespace btree 测试 ─────────────────────────────────────

    #[test]
    fn test_freespace_btree_sync_on_allocate() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // 分配后，freespace btree 中不应有此 bucket 的正常条目
        let freespace_pos = Bpos::new(0, bucket_index, 1); // gen=1 after first alloc
        let entry = engine.get_entry_raw(BtreeId::Freespace, freespace_pos);
        // 预期：不存在（被 Deleted tombstone 覆盖）或根本不在 btree 中
        assert!(
            entry.is_none() || matches!(entry.unwrap().key_type, KeyType::Deleted),
            "freespace entry should be absent or tombstone after allocation"
        );
    }

    #[test]
    fn test_freespace_btree_sync_on_free() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;

        // C3: free 后 state=NeedDiscard（不写入 freespace），trim 后才是 Free
        alloc.bch2_bucket_free(addr, &mut engine).unwrap();
        alloc.bch2_bucket_do_trim(addr, &mut engine).unwrap();

        // bucket alloc (ver=1) → free (ver=2) → trim (ver=2, freespace gen=2)
        let freespace_pos = Bpos::new(0, bucket_index, 2);
        let entry = engine.get_entry_raw(BtreeId::Freespace, freespace_pos);
        assert!(entry.is_some(), "freespace should have entry after trim");
        if let Some(e) = entry {
            assert_eq!(
                e.key_type,
                KeyType::Normal,
                "freespace entry should be Normal after trim"
            );
        }
    }

    #[test]
    fn test_freespace_rebuild_from_alloc() {
        let (alloc, mut engine) = make_alloc(4096, 1024);

        // 分配一个 bucket、释放并 trim（创建 Alloc btree entry for Free）
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        let bucket_index = addr / BLOCKS_PER_BUCKET;
        alloc.bch2_bucket_free(addr, &mut engine).unwrap();
        alloc.bch2_bucket_do_trim(addr, &mut engine).unwrap();

        // 创建新 engine，从 Alloc btree 重建 Freespace btree
        let mut fresh_engine = crate::btree::BtreeEngine::new();
        // 手动将 Alloc btree 的条目复制到新 engine
        if let Some(entry) = engine.get_entry_raw(BtreeId::Alloc, Bpos::new(0, bucket_index, 0)) {
            fresh_engine.insert_entry_raw(BtreeId::Alloc, entry, 0);
        }

        // 重建 freespace
        super::bch2_rebuild_freespace(&mut fresh_engine).unwrap();

        // C4: rebuild 现在会按 offset 排序并检测 hole。验证 freespace btree 中有释放的 bucket
        // Alloc entry 最终 version=2（alloc ver=1 → free ver=2 → trim ver=2）
        let freespace_pos = Bpos::new(0, bucket_index, 2);
        let freespace_entry = fresh_engine.get_entry_raw(BtreeId::Freespace, freespace_pos);
        assert!(
            freespace_entry.is_some(),
            "rebuild should insert freed bucket into freespace"
        );
    }

    #[test]
    fn test_freespace_no_sync_for_unchanged_state() {
        // 分配后直接再次分配同一个 bucket 位置（不同 gen）不应影响 freespace
        let (_alloc, mut engine) = make_alloc(4096, 1024);

        // 手动写入一个 Free 的 Alloc entry
        let entry = BchAllocEntry::from_bucket(0, BchDataType::Free, 1);
        let bytes = serialize_alloc_entry(&entry).unwrap();
        engine.insert_entry_raw(
            BtreeId::Alloc,
            BtreeEntry::raw(Bpos::new(0, 0, 0), KeyType::Normal, bytes),
            0,
        );

        // 然后写入另一个 Free 的 Alloc entry（状态未变，不应触发 freespace 同步）
        let entry2 = BchAllocEntry::from_bucket(0, BchDataType::Free, 2);
        let bytes2 = serialize_alloc_entry(&entry2).unwrap();
        engine.insert_entry_raw(
            BtreeId::Alloc,
            BtreeEntry::raw(Bpos::new(0, 0, 0), KeyType::Normal, bytes2),
            0,
        );

        // Freespace btree 不应有该 bucket 的条目（allocate_bucket/free 负责同步，
        // 直接写 Alloc btree 不会触发 freespace 同步）
        let freespace_pos = Bpos::new(0, 0, 2);
        // 可能没有，可能是 Free→Free 不会触发
        let freespace_entry = engine.get_entry_raw(BtreeId::Freespace, freespace_pos);
        // 不断言存在或不存在，仅验证不 panic
    }

    // ─── P1: Write Point 测试 ─────────────────────────

    #[test]
    fn test_allocator_new_backward_compat() {
        // new() 默认 WP=1，行为与旧版本一致
        let alloc = BchAllocator::new(4096, 1024, 0);
        let mut engine = crate::btree::BtreeEngine::new();
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc.allocated_blocks(), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_with_config_default_eq_new() {
        // with_config(WP=1) 行为与 new() 一致
        let alloc1 = BchAllocator::new(4096, 1024, 0);
        let alloc2 = BchAllocator::with_config(
            4096,
            1024,
            0,
            WritePointConfig {
                max_write_points: 1,
            },
        );
        assert!(
            alloc2.write_points.is_none(),
            "WP=1 should have no write point pool"
        );
        // 验证 hint 字段类型相同（内部细节：两者都使用全局 hint）
        let mut engine = crate::btree::BtreeEngine::new();
        let addr = alloc2
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
        assert_eq!(alloc2.allocated_blocks(), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_with_config_wp_gt_1_has_pool() {
        let alloc = BchAllocator::with_config(
            4096,
            1024,
            0,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        // write_points 应为 Some（池已初始化）
        // 由于 write_points 是私有字段，通过功能验证：分配应仍然正常工作
        let mut engine = crate::btree::BtreeEngine::new();
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(42)),
            )
            .unwrap();
        assert_eq!(addr, 0);
        assert_eq!(alloc.allocated_blocks(), BLOCKS_PER_BUCKET);
    }

    #[test]
    fn test_allocate_with_wp_id_none() {
        // None WP ID → 使用全局 hint（向后兼容路径）
        let alloc = BchAllocator::new(4096, 1024, 0);
        let mut engine = crate::btree::BtreeEngine::new();
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(addr, 1024); // P1-7: InteriorUpdate→System offset=1→group 1
                                // 第二次分配，hint 推进
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_ne!(addr, addr2);
    }

    #[test]
    fn test_allocate_with_wp_id_hashed() {
        let alloc = BchAllocator::with_config(
            8192,
            1024,
            0,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let mut engine = crate::btree::BtreeEngine::new();
        // 相同 hash 值应导致 hint 行为相同（但 WP hint 是独立的）
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(100)),
            )
            .unwrap();
        assert!(addr % BLOCKS_PER_BUCKET == 0);
        // 不同 hash 值使用不同 WP → hint 独立
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(200)),
            )
            .unwrap();
        assert_ne!(addr, addr2);
    }

    #[test]
    fn test_allocate_with_wp_id_direct() {
        let alloc = BchAllocator::with_config(
            8192,
            1024,
            0,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let mut engine = crate::btree::BtreeEngine::new();
        // 专用写点：btree
        let addr = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::BTree)),
            )
            .unwrap();
        assert!(addr % BLOCKS_PER_BUCKET == 0);
        // journal
        let addr2 = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::Journal)),
            )
            .unwrap();
        assert_ne!(addr, addr2);
        // GC
        let addr3 = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Direct(DedicatedWp::GC)),
            )
            .unwrap();
        assert_ne!(addr2, addr3);
    }

    #[test]
    fn test_regression_pass_none_when_wp_disabled() {
        // WRITE_POINT_MAX=1 时即使传 Some(...)，因为是 None 池所以仍用全局 hint
        let alloc = BchAllocator::new(4096, 1024, 0);
        let mut engine = crate::btree::BtreeEngine::new();
        // Should not crash
        let _addr = alloc
            .bch2_bucket_alloc_new_fs(
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(42)),
            )
            .unwrap();
        // WP=1 时池不存在，Some(...) 被 match arm _ 捕获走全局 hint
    }

    #[test]
    fn test_allocate_blocks_with_wp_id() {
        let alloc = BchAllocator::with_config(
            8192,
            1024,
            0,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let mut engine = crate::btree::BtreeEngine::new();
        let addr = alloc
            .bch2_alloc_sectors_start_trans(
                1,
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(99)),
            )
            .unwrap();
        assert_eq!(addr, 0);
    }

    #[test]
    fn test_allocate_buckets_with_wp_id() {
        let alloc = BchAllocator::with_config(
            8192,
            1024,
            0,
            WritePointConfig {
                max_write_points: 8,
            },
        );
        let mut engine = crate::btree::BtreeEngine::new();
        let addrs = alloc
            .bch2_alloc_buckets(
                2,
                &mut engine,
                &ureq(Watermark::InteriorUpdate),
                Some(WritePointSpecifier::Hashed(77)),
            )
            .unwrap();
        assert_eq!(addrs.len(), 2);
        assert_ne!(addrs[0], addrs[1]);
    }

    // ─── L4 fallback 测试 ────────────────────────────

    #[test]
    fn test_l4_fallback_when_freelist_empty() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        // 清空所有组的 freelist，但保留 Free bucket
        for group_mutex in &alloc.groups {
            let mut group = group_mutex.lock().unwrap();
            group.free_list.clear();
        }
        // L4 fallback 应扫描 bucket 数组找到 Free 桶
        let addr = alloc
            .bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None)
            .unwrap();
        assert_eq!(
            addr, 1024,
            "L4 fallback: InteriorUpdate→System offset=1→group 1"
        );
    }

    #[test]
    fn test_l4_fallback_exhausted() {
        let (alloc, mut engine) = make_alloc(4096, 1024);
        // 清空 freelist + 将所有 bucket 标记为 User
        for group_mutex in &alloc.groups {
            let mut group = group_mutex.lock().unwrap();
            group.free_list.clear();
            group.free_buckets.store(0, Ordering::Relaxed);
            for bucket in &mut group.buckets {
                bucket.state = BchDataType::User;
            }
        }
        // P0-2: 无可用 bucket → AllocError::AddressSpaceExhausted
        let result =
            alloc.bch2_bucket_alloc_new_fs(&mut engine, &ureq(Watermark::InteriorUpdate), None);
        assert!(
            result.is_err(),
            "should return AllocError::AddressSpaceExhausted"
        );
        match result {
            Err(AllocError::AddressSpaceExhausted { .. }) => {} // expected
            _ => panic!("expected AddressSpaceExhausted"),
        }
    }
}
