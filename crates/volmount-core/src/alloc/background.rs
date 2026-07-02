use crate::alloc::btree::BchAllocEntry;
use crate::alloc::bucket::Bucket;
use crate::alloc::BchDataType;

const LRU_TIME_BITS: u32 = 48;
const LRU_TIME_MAX: u64 = (1u64 << LRU_TIME_BITS) - 1;
const FRAGMENTATION_LRU_SCALE: u64 = 1 << 31;

fn data_type_movable(data_type: BchDataType) -> bool {
    matches!(
        data_type,
        BchDataType::Btree | BchDataType::User | BchDataType::Stripe
    )
}

/// GC generation 位图大小（每个 bucket 1 bit）
pub const BITMAP_SIZE: usize = 1024;

/// bucket_gens 脏写阈值——达到此数量后触发批量写回
pub const BUCKET_GENS_DIRTY_THRESHOLD: usize = 64;

// ─── P1-6: 懒惰 bucket_gens 脏写 + 批量写回 ─────────

/// GC generation 追踪器
///
/// P1-6: 从全量 set-version 改为懒惰 dirty write + 批量写回。
/// 只在 bucket 的 gen 实际变更时标记为 dirty，积攒到阈值后批量写入。
pub struct GensTracker {
    /// 每个 bucket 的当前 gen 值（索引 = bucket_index）
    gens: Vec<u32>,
    /// 标记为 dirty 的 bucket 索引集合
    dirty: Vec<u64>,
    /// 当前 dirty 计数
    dirty_count: usize,
}

impl GensTracker {
    /// 创建新的 gens 追踪器
    pub fn new(bucket_count: u64) -> Self {
        Self {
            gens: vec![0u32; bucket_count as usize],
            dirty: Vec::with_capacity(BUCKET_GENS_DIRTY_THRESHOLD * 2),
            dirty_count: 0,
        }
    }

    /// 获取指定 bucket 的 gen
    pub fn get_gen(&self, bucket_idx: u64) -> u32 {
        self.gens.get(bucket_idx as usize).copied().unwrap_or(0)
    }

    /// 递增指定 bucket 的 gen 并标记为 dirty
    ///
    /// 返回新的 gen 值。如果 gen 变更，bucket 被加入 dirty 集合。
    /// 当 dirty_count 达到阈值时，返回 true 表示需要批量写回。
    pub fn inc_gen(&mut self, bucket_idx: u64) -> (u32, bool) {
        let idx = bucket_idx as usize;
        if idx >= self.gens.len() {
            return (0, false);
        }
        let old = self.gens[idx];
        let new = old.wrapping_add(1);
        self.gens[idx] = new;

        if !self.dirty.contains(&bucket_idx) {
            self.dirty.push(bucket_idx);
            self.dirty_count += 1;
        }

        let needs_batch_write = self.dirty_count >= BUCKET_GENS_DIRTY_THRESHOLD;
        (new, needs_batch_write)
    }

    /// 检查并触发批量写回（写回所有 dirty gens）
    ///
    /// 返回需要写回的 (bucket_index, new_gen) 列表。
    /// 调用者应将此列表通过 Alloc btree 写回磁盘。
    /// 调用后 dirty 列表清空。
    pub fn flush_dirty(&mut self) -> Vec<(u64, u32)> {
        let result: Vec<(u64, u32)> = self
            .dirty
            .iter()
            .map(|&idx| (idx, self.gens[idx as usize]))
            .collect();
        self.dirty.clear();
        self.dirty_count = 0;
        result
    }

    /// 从磁盘加载时设置 gen 值
    pub fn set_gen(&mut self, bucket_idx: u64, gen: u32) {
        let idx = bucket_idx as usize;
        if idx < self.gens.len() {
            self.gens[idx] = gen;
        }
    }
}

// ─── P1-9: gc_gens 回收范围扩展到完整 BITMAP_SIZE ─────────

/// GC generation 位图——追踪哪些 bucket 的 gen 需要 GC 更新
///
/// P1-9: 回收范围从部分扫描扩展到完整 BITMAP_SIZE。
/// 每个 bit 对应一个 bucket：1 = 需要 GC gen 更新，0 = 已更新。
pub struct GcGensBitmap {
    /// 位图存储（每个 u64 存储 64 个 bucket 的标记）
    bitmap: Vec<u64>,
    /// 当前扫描位置（用于分步处理）
    scan_pos: usize,
}

impl GcGensBitmap {
    /// 创建新的 GC gens 位图
    pub fn new(bucket_count: u64) -> Self {
        let words = (bucket_count as usize).div_ceil(64);
        Self {
            bitmap: vec![0u64; words.max(1)],
            scan_pos: 0,
        }
    }

    /// 标记指定 bucket 需要 GC gen 更新
    pub fn mark_need_gc(&mut self, bucket_idx: u64) {
        let word = (bucket_idx as usize) / 64;
        let bit = bucket_idx % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] |= 1u64 << bit;
        }
    }

    /// 标记指定 bucket 的 GC 已更新
    pub fn mark_gc_done(&mut self, bucket_idx: u64) {
        let word = (bucket_idx as usize) / 64;
        let bit = bucket_idx % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] &= !(1u64 << bit);
        }
    }

    /// 检查指定 bucket 是否需要 GC gen 更新
    pub fn needs_gc(&self, bucket_idx: u64) -> bool {
        let word = (bucket_idx as usize) / 64;
        let bit = bucket_idx % 64;
        if word < self.bitmap.len() {
            (self.bitmap[word] >> bit) & 1u64 != 0
        } else {
            false
        }
    }

    /// 扫描完整 BITMAP_SIZE 范围，收集需要 GC gen 更新的 bucket
    ///
    /// P1-9: 完整扫描 BITMAP_SIZE（之前可能仅部分扫描）。
    /// 扫描从 `scan_pos` 开始，最多扫描 `batch_size` 个 word。
    /// 返回需要更新的 bucket_index 列表。
    /// 多次调用可逐步扫描全量位图。
    pub fn scan_full_range(&mut self, batch_size: usize) -> Vec<u64> {
        if self.bitmap.is_empty() {
            return Vec::new();
        }

        let start = self.scan_pos;
        let end = (start + batch_size).min(self.bitmap.len());
        let mut result = Vec::new();

        for word_idx in start..end {
            let mut bits = self.bitmap[word_idx];
            while bits != 0 {
                let bit = bits.trailing_zeros();
                let bucket_idx = (word_idx * 64) + bit as usize;
                result.push(bucket_idx as u64);
                bits &= bits - 1; // clear lowest set bit
            }
        }

        self.scan_pos = if end >= self.bitmap.len() {
            // 一轮完成，重置扫描位置
            0
        } else {
            end
        };

        result
    }

    /// 重置扫描位置
    pub fn reset_scan(&mut self) {
        self.scan_pos = 0;
    }

    /// 返回位图是否完全为空（无待处理项）
    pub fn is_idle(&self) -> bool {
        self.bitmap.iter().all(|&w| w == 0)
    }
}

// ─── P2-10: bucket_mark 批量写回时 0 号桶初始化补全 ─────────

/// 在批量写回时确保 0 号 bucket 的 mark 已初始化
///
/// P2-10: bcachefs 在初始化时确保 bucket 0 的 alloc entry 存在且正确标记。
/// volmount 首次分配时如果 0 号桶未初始化，可能有状态不一致。
/// 此函数在批量写回路径中被调用：如果 0 号桶的 state 为 Free（未初始化），
/// 将其标记为 Sb（超块占用）以确保一致性。
pub fn ensure_bucket0_marked(buckets: &mut [Bucket]) {
    if let Some(bucket0) = buckets.first_mut() {
        if bucket0.state == BchDataType::Free || bucket0.state == BchDataType::FreeAvailable {
            bucket0.state = BchDataType::Sb;
        }
    }
}

/// 读 LRU 索引——对应 bcachefs `alloc_lru_idx_read()`
pub fn alloc_lru_idx_read(entry: &BchAllocEntry) -> u64 {
    if entry.state == BchDataType::Cached {
        entry.io_time_read & LRU_TIME_MAX
    } else {
        0
    }
}

/// 片段 LRU 索引——对应 bcachefs `alloc_lru_idx_fragmentation()`
pub fn alloc_lru_idx_fragmentation(entry: &BchAllocEntry, bucket_size: u64) -> u64 {
    if bucket_size == 0 || !data_type_movable(entry.state) {
        return 0;
    }

    let used = u64::from(entry.dirty_sectors);
    if used == 0 {
        return 0;
    }

    let capped = used.min(bucket_size);
    capped.saturating_mul(FRAGMENTATION_LRU_SCALE) / bucket_size
}

/// 外部 backpointer 数量访问器——未来 backpointer 检查可直接复用
pub fn alloc_nr_external_backpointers(entry: &BchAllocEntry) -> u32 {
    entry.nr_external_backpointers
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── P1-6: GensTracker tests ───────────────────────────

    #[test]
    fn test_gens_tracker_new() {
        let gt = GensTracker::new(100);
        assert_eq!(gt.gens.len(), 100);
        assert_eq!(gt.dirty_count, 0);
    }

    #[test]
    fn test_gens_tracker_inc_gen() {
        let mut gt = GensTracker::new(10);
        let (gen, _) = gt.inc_gen(3);
        assert_eq!(gen, 1);
        assert_eq!(gt.get_gen(3), 1);
        assert_eq!(gt.dirty_count, 1);
    }

    #[test]
    fn test_gens_tracker_dirty_dedup() {
        let mut gt = GensTracker::new(10);
        gt.inc_gen(3);
        gt.inc_gen(3); // 同一桶两次 inc
        assert_eq!(gt.dirty_count, 1, "same bucket should not double-count");
    }

    #[test]
    fn test_gens_tracker_flush_empty() {
        let mut gt = GensTracker::new(10);
        let result = gt.flush_dirty();
        assert!(result.is_empty());
    }

    #[test]
    fn test_gens_tracker_flush() {
        let mut gt = GensTracker::new(100);
        gt.inc_gen(0);
        gt.inc_gen(5);
        let result = gt.flush_dirty();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (0, 1));
        assert_eq!(result[1], (5, 1));
        assert_eq!(gt.dirty_count, 0);
    }

    // ─── P1-9: GcGensBitmap tests ──────────────────────────

    #[test]
    fn test_gc_gens_bitmap_new() {
        let bm = GcGensBitmap::new(64);
        assert!(!bm.bitmap.is_empty());
        assert!(bm.is_idle());
    }

    #[test]
    fn test_gc_gens_bitmap_mark_and_check() {
        let mut bm = GcGensBitmap::new(128);
        bm.mark_need_gc(42);
        assert!(bm.needs_gc(42));
        assert!(!bm.needs_gc(43));
        bm.mark_gc_done(42);
        assert!(!bm.needs_gc(42));
    }

    #[test]
    fn test_gc_gens_bitmap_scan_full_range() {
        let mut bm = GcGensBitmap::new(256);
        bm.mark_need_gc(10);
        bm.mark_need_gc(100);
        bm.mark_need_gc(200);

        let result = bm.scan_full_range(4);
        assert_eq!(result.len(), 3, "should find all marked buckets");
        assert!(result.contains(&10));
        assert!(result.contains(&100));
        assert!(result.contains(&200));

        // 第二轮扫描应空（已全部清理）
        // note: scan 只收集，不清理标记
        let second = bm.scan_full_range(4);
        // scan 仅收集不清理，所以第二轮仍能找到标记
        assert!(second.len() >= 3);
    }

    // ─── P2-10: ensure_bucket0_marked tests ────────────────

    #[test]
    fn test_ensure_bucket0_marked_free() {
        let mut buckets = vec![Bucket {
            state: BchDataType::Free,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            journal_seq: 0,
            group: 0,
            version: 0,
            bucket_idx: 0,
            nocow_locked: false,
        }];
        ensure_bucket0_marked(&mut buckets);
        assert_eq!(buckets[0].state, BchDataType::Sb);
    }

    #[test]
    fn test_ensure_bucket0_marked_already_set() {
        let mut buckets = vec![Bucket {
            state: BchDataType::Sb,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            journal_seq: 0,
            group: 0,
            version: 0,
            bucket_idx: 0,
            nocow_locked: false,
        }];
        ensure_bucket0_marked(&mut buckets);
        assert_eq!(buckets[0].state, BchDataType::Sb, "already Sb, no change");
    }

    #[test]
    fn test_alloc_lru_idx_read_uses_cached_time() {
        let mut entry = BchAllocEntry::free(0);
        entry.state = BchDataType::Cached;
        entry.io_time_read = LRU_TIME_MAX | 0x1234_0000_0000_0000;
        assert_eq!(alloc_lru_idx_read(&entry), LRU_TIME_MAX);
    }

    #[test]
    fn test_alloc_lru_idx_fragmentation_scales_used_bytes() {
        let mut entry = BchAllocEntry::free(0);
        entry.state = BchDataType::User;
        entry.dirty_sectors = 64;
        entry.cached_sectors = 32;
        let idx = alloc_lru_idx_fragmentation(&entry, 256);
        assert!(idx > 0);
        assert!(idx <= FRAGMENTATION_LRU_SCALE);
    }

    #[test]
    fn test_alloc_nr_external_backpointers_accessor() {
        let mut entry = BchAllocEntry::free(0);
        entry.nr_external_backpointers = 9;
        assert_eq!(alloc_nr_external_backpointers(&entry), 9);
    }
}
