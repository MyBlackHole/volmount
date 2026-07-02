//! Bucket 状态管理 — bcachefs 对齐

use serde::{Deserialize, Serialize};

/// Bucket 数据类型 — 对齐 bcachefs `enum bch_data_type`（BCH_DATA_TYPES）
///
/// 数值与 bcachefs C 源码完全一致：
/// - BCH_DATA_free=0, BCH_DATA_sb=1, BCH_DATA_journal=2, BCH_DATA_btree=3,
///   BCH_DATA_user=4, BCH_DATA_cached=5, BCH_DATA_parity=6, BCH_DATA_stripe=7,
///   BCH_DATA_need_gc_gens=8, BCH_DATA_need_discard=9, BCH_DATA_unstriped=10
/// - Reserved（11）是 volmount 内部变体，非 bcachefs 标准。
/// - FreeDiscarded（12）/ FreeAvailable（13）/ SbOnly（14）是扩展的桶状态机变体，
///   不参与 derive_data_type() 的扇区计数推导逻辑，仅用于桶状态机转换。
///
/// P1-5: 新增 free_discarded（已 TRIM 但未入 free_list）、free_available（在 free_list 中且可分配）、
/// sb_only（仅超块占用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BchDataType {
    /// 空闲（BCH_DATA_free = 0）
    Free = 0,
    /// 超块（BCH_DATA_sb = 1）
    Sb = 1,
    /// Journal/WAL（BCH_DATA_journal = 2）
    Journal = 2,
    /// Btree 节点（BCH_DATA_btree = 3）
    Btree = 3,
    /// 用户数据（BCH_DATA_user = 4）
    User = 4,
    /// 缓存（BCH_DATA_cached = 5）
    Cached = 5,
    /// RAID 奇偶校验（BCH_DATA_parity = 6）
    Parity = 6,
    /// RAID 条带（BCH_DATA_stripe = 7）
    Stripe = 7,
    /// 需要 GC 代际更新（BCH_DATA_need_gc_gens = 8）
    NeedGcGens = 8,
    /// 需要丢弃（BCH_DATA_need_discard = 9）
    NeedDiscard = 9,
    /// 非条带数据（BCH_DATA_unstriped = 10）
    Unstriped = 10,
    /// 预留/正在写入（volmount 特有，bcachefs 无直接对应）
    Reserved = 11,
    /// 已 TRIM 但未入 free_list（volmount 特有，P1-5）
    FreeDiscarded = 12,
    /// 在 free_list 中且可立即分配（volmount 特有，P1-5）
    FreeAvailable = 13,
    /// 仅超块占用，不可用于分配（volmount 特有，P1-5）
    SbOnly = 14,
}

/// BCH_DATA_NR — bcachefs 数据类型总数（不含 Reserved）
///
/// bcachefs C 源码（fs/alloc/accounting_format.h:68-76）:
/// ```c
/// enum bch_data_type {
///     BCH_DATA_free=0,  ..., BCH_DATA_unstriped=10,
///     BCH_DATA_NR       // = 11，作为 enum 最后一个条目用作数组尺寸
/// };
/// ```
/// BCH_DATA_NR 在 C 中并非有效的数据类型变体，而是 `BCH_DATA_unstriped + 1 = 11`，
/// 用作 `bch_devs_mask rw_devs[BCH_DATA_NR]` 等数组的静态尺寸。
///
/// volmount 新增 `Reserved = 11` 作为第 12 个变体，其值超出 C 的 BCH_DATA_NR 范围。
pub const BCH_DATA_NR: usize = 11;

/// Bucket 元数据
///
/// bcachefs 对齐：data_type 从 dirty_sectors / cached_sectors / stripe 计数中推导，
/// 而非显式存储。state 字段作为缓存保留，以 sector 计数为真实来源。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Bucket {
    pub state: BchDataType,
    /// 脏扇区计数 — bcachefs 从 dirty_sectors > 0 推导 data_type
    pub dirty_sectors: u32,
    /// 缓存扇区计数
    pub cached_sectors: u32,
    /// 条带引用计数
    pub stripe: u32,
    /// Journal seq（记录最后引用此 bucket 的 journal entry seq），用于分配前安全的 journal 检查
    pub journal_seq: u64,
    /// 所属 allocation group
    pub group: u32,
    /// 版本号（防 ABA）
    pub version: u32,
    /// 所属 bucket index
    pub bucket_idx: u64,
    /// nocow 锁定标记 — 内存运行时标记，不持久化
    /// bcachefs 对应: nocow_locking.h bucket_nocow_is_locked()
    #[serde(default)]
    pub nocow_locked: bool,
}

impl Bucket {
    /// 创建空闲 bucket
    pub const fn free(group: u32, bucket_idx: u64) -> Self {
        Self {
            state: BchDataType::Free,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            journal_seq: 0,
            group,
            version: 0,
            bucket_idx,
            nocow_locked: false,
        }
    }

    /// 是否空闲
    pub fn is_free(&self) -> bool {
        self.state == BchDataType::Free
    }

    /// 标记为已分配
    pub fn mark_allocated(&mut self) {
        self.state = BchDataType::User;
        self.version = self.version.wrapping_add(1);
    }

    /// 标记为空闲
    pub fn mark_free(&mut self) {
        self.state = BchDataType::Free;
        self.version = self.version.wrapping_add(1);
    }

    /// 标记为脏
    pub fn mark_dirty(&mut self) {
        self.state = BchDataType::NeedGcGens;
    }

    /// 标记为预留
    pub fn mark_reserved(&mut self) {
        self.state = BchDataType::Reserved;
    }

    /// 根据扇区计数推导 data_type（对应 bcachefs `alloc_data_type`）
    ///
    /// P0-3: 严格执行 C 优先级顺序 `USER > META > PARITY > RESERVED`。
    /// 当 dirty_sectors > 0 时，跳过 transient 状态（NeedDiscard / NeedGcGens）
    /// 返回等效的实际数据类型。C 语义：stripe > dirty > cached > free。
    pub fn derive_state(&self) -> BchDataType {
        if self.stripe > 0 {
            return BchDataType::Stripe;
        }
        if self.dirty_sectors > 0 {
            // P0-3: 跳过 transient 状态，透传实际数据类型
            match self.state {
                BchDataType::NeedDiscard
                | BchDataType::NeedGcGens
                | BchDataType::FreeDiscarded
                | BchDataType::Reserved
                | BchDataType::SbOnly
                | BchDataType::FreeAvailable
                | BchDataType::Free => {
                    // dirty_sectors > 0 说明桶有数据，退化为 User
                    // bcachefs 中 dirty_sectors > 0 时 data_type 字段记录实际类型，
                    // 若为 transient 状态则返回 User（最通用的脏数据类型）
                    BchDataType::User
                }
                _ => self.state,
            }
        } else if self.cached_sectors > 0 {
            BchDataType::Cached
        } else {
            BchDataType::Free
        }
    }
}

/// 推导 bucket 的数据类型 — 对应 bcachefs `alloc_data_type()` (alloc_background.c)
///
/// bcachefs 使用 dirty_sectors / cached_sectors / stripe 计数推导 data_type：
/// 1. stripe > 0 → BCH_DATA_stripe
/// 2. dirty_sectors > 0 → 透传 data_type，跳过 transient 状态
/// 3. cached_sectors > 0 → BCH_DATA_cached
/// 4. 否则 → BCH_DATA_free
///
/// P0-3: 当 dirty_sectors > 0 时，如果 data_type 是 transient 状态
///（NeedDiscard / NeedGcGens / FreeDiscarded / Reserved / SbOnly / FreeAvailable / Free），
/// 退化为 User——因为扇区计数表明桶中有数据，transient 状态不应掩盖实际数据存在。
/// C bcachefs 的 `alloc_data_type()` 行为相同：dirty_sectors > 0 时使用存储的 data_type 字段。
/// 我们的 transient 状态不在 C 的 data_type 字段中出现，所以退化为 User。
pub fn derive_data_type(
    dirty_sectors: u32,
    cached_sectors: u32,
    stripe: u32,
    data_type: BchDataType,
) -> BchDataType {
    if stripe > 0 {
        return BchDataType::Stripe;
    }
    if dirty_sectors > 0 {
        // P0-3: 跳过 transient 状态，退化为 User
        return match data_type {
            BchDataType::NeedDiscard
            | BchDataType::NeedGcGens
            | BchDataType::FreeDiscarded
            | BchDataType::Reserved
            | BchDataType::SbOnly
            | BchDataType::FreeAvailable
            | BchDataType::Free => BchDataType::User,
            _ => data_type,
        };
    }
    if cached_sectors > 0 {
        return BchDataType::Cached;
    }
    BchDataType::Free
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucket_new_free() {
        let b = Bucket::free(1, 42);
        assert!(b.is_free());
        assert_eq!(b.group, 1);
        assert_eq!(b.bucket_idx, 42);
        assert_eq!(b.version, 0);
    }

    #[test]
    fn test_bucket_mark_allocated() {
        let mut b = Bucket::free(0, 0);
        b.mark_allocated();
        assert_eq!(b.state, BchDataType::User);
        assert_eq!(b.version, 1);
    }

    #[test]
    fn test_bucket_mark_free() {
        let mut b = Bucket::free(2, 10);
        b.mark_allocated();
        b.mark_free();
        assert_eq!(b.state, BchDataType::Free);
        assert_eq!(b.version, 2);
    }

    #[test]
    fn test_bucket_mark_dirty() {
        let mut b = Bucket::free(0, 0);
        b.mark_dirty();
        assert_eq!(b.state, BchDataType::NeedGcGens);
    }

    #[test]
    fn test_bucket_mark_reserved() {
        let mut b = Bucket::free(0, 0);
        b.mark_reserved();
        assert_eq!(b.state, BchDataType::Reserved);
    }

    #[test]
    fn test_bucket_serde_roundtrip() {
        let b = Bucket::free(3, 100);
        let data = bincode::serialize(&b).unwrap();
        let restored: Bucket = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.state, b.state);
        assert_eq!(restored.group, b.group);
        assert_eq!(restored.bucket_idx, b.bucket_idx);
    }

    #[test]
    fn test_derive_data_type_stripe() {
        assert_eq!(
            derive_data_type(0, 0, 1, BchDataType::Free),
            BchDataType::Stripe
        );
        assert_eq!(
            derive_data_type(100, 0, 1, BchDataType::User),
            BchDataType::Stripe
        );
    }

    #[test]
    fn test_derive_data_type_dirty_sectors() {
        assert_eq!(
            derive_data_type(1, 0, 0, BchDataType::User),
            BchDataType::User
        );
        assert_eq!(
            derive_data_type(50, 0, 0, BchDataType::Btree),
            BchDataType::Btree
        );
    }

    #[test]
    fn test_derive_data_type_cached() {
        assert_eq!(
            derive_data_type(0, 1, 0, BchDataType::Free),
            BchDataType::Cached
        );
    }

    #[test]
    fn test_derive_data_type_free() {
        assert_eq!(
            derive_data_type(0, 0, 0, BchDataType::Free),
            BchDataType::Free
        );
    }

    #[test]
    fn test_bucket_derive_state() {
        let mut b = Bucket::free(0, 0);
        assert_eq!(b.derive_state(), BchDataType::Free);

        b.dirty_sectors = 100;
        b.state = BchDataType::User;
        assert_eq!(b.derive_state(), BchDataType::User);

        b.stripe = 1;
        assert_eq!(b.derive_state(), BchDataType::Stripe);

        b.stripe = 0;
        b.dirty_sectors = 0;
        b.cached_sectors = 50;
        assert_eq!(b.derive_state(), BchDataType::Cached);
    }
}
