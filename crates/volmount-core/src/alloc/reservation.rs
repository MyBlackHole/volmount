//! DiskReservation — bcachefs 对齐的扇区预留系统
//!
//! 对应 bcachefs `struct disk_reservation`（`buckets.h:341-401`）+ `__bch2_disk_reservation_add()`
//!（`buckets.c:1215-1240`）+ `disk_reservation_recalc_sectors_available()`（`buckets.c:1190-1213`）。
//!
//! ## 作用
//!
//! 在分配操作之前预留一定数量的扇区，确保分配不会因空间不足失败。
//! 预留生命周期：init → add 预留扇区 → 分配操作 → commit 消耗已用扇区 → put 释放剩余
//!
//! ## 结构
//!
//! - `ReservationTracker`：管理预留预算（总容量 - 安全水位），提供 add/commit/put
//! - `DiskReservation`：单次预留的记录（扇区数 + 副本数 + 数据类型）
//!
//! ## bcachefs 对照
//!
//! | bcachefs | volmount |
//! |----------|----------|
//! | `bch2_disk_reservation_add()` | `ReservationTracker::bch2_disk_reservation_add()` |
//! | `bch2_disk_reservation_put()` | `ReservationTracker::bch2_disk_reservation_put()` |
//! | `bch2_disk_reservation_init()` | `DiskReservation::init()` |
//! | `bch2_disk_reservation_get()` | `ReservationTracker::bch2_disk_reservation_get()` |
//! | `struct disk_reservation` | `DiskReservation` |
//!
//! 参考: `fs/alloc/buckets.h:341-401`, `fs/alloc/buckets.c:1186-1240`

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::alloc::bucket::BchDataType;
use crate::types::StorageError;

/// 每 block 的扇区数（4KB / 512B = 8）
pub const SECTORS_PER_BLOCK: u64 = 8;

/// 预留标志 — 对应 bcachefs `enum bch_reservation_flags`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BchReservationFlags {
    /// 无特殊标志
    None = 0,
    /// 不允许失败 — 即使空间不足也强制分配（用于不可回滚的元数据写入）
    Nofail = 1,
    /// 允许部分分配 — 能分配多少算多少
    Partial = 2,
}

/// 扇区预留 — 对应 bcachefs `struct disk_reservation`
///
/// 纯内存结构，不序列化。记录一次分配操作预留的扇区数和数据类型。
///
/// 生命周期：
/// 1. `init()` — 创建空预留
/// 2. `bch2_disk_reservation_add()` — 追加预留扇区
/// 3. 分配操作实际使用扇区 → 调用 `commit()` 消耗已用空间
/// 4. 分配完成或失败 → 调用 `put()` 释放剩余预留
#[derive(Debug, Clone)]
pub struct DiskReservation {
    /// 预留的扇区数（0 = 无预留）
    pub sectors: Cell<u64>,
    /// 副本数
    pub nr_replicas: u32,
    /// 预留的数据类型
    pub data_type: BchDataType,
}

impl DiskReservation {
    /// 空预留（无扇区预留）— 对应 bcachefs `bch2_disk_reservation_init()`
    pub const fn init(nr_replicas: u32) -> Self {
        Self {
            sectors: Cell::new(0),
            nr_replicas,
            data_type: BchDataType::Free,
        }
    }

    /// 创建带扇区数和数据类型的预留
    pub fn new(sectors: u64, data_type: BchDataType) -> Self {
        Self {
            sectors: Cell::new(sectors),
            nr_replicas: 1,
            data_type,
        }
    }

    /// 是否有效（有预留扇区数）
    pub fn is_valid(&self) -> bool {
        self.sectors.get() > 0
    }

    /// 是否为空（无预留）
    pub fn is_empty(&self) -> bool {
        self.sectors.get() == 0
    }
}

/// 预留追踪器 — 管理预留预算
///
/// 对应 bcachefs 的 disk_reservation 系统（纯内存简化版，不包含 per-CPU 优化）。
///
/// 预算机制：
/// - `max_reserved`：最多可预留的扇区数（总容量 - 安全水位）
/// - `total_reserved`：当前已预留但尚未 commit/put 的扇区数
/// - `available() = max_reserved - total_reserved`
///
/// 生命周期：
/// - `bch2_disk_reservation_add(sectors, data_type, flags)` → 追加预留扇区到 `DiskReservation`
/// - `bch2_disk_reservation_get(sectors, nr_replicas, flags)` → 初始化并预留（init + add 组合）
/// - 分配操作使用空间 → `commit(&reservation, used_sectors)` 消耗已用扇区
/// - 分配完成 → `put(&reservation)` 释放剩余预留（将 res.sectors = 0）
pub struct ReservationTracker {
    /// 当前已预留但未消费的扇区数
    total_reserved: AtomicU64,
    /// 最大可预留扇区数（总容量减去安全水位）
    max_reserved: u64,
}

impl ReservationTracker {
    /// 创建新的预留追踪器
    ///
    /// # 参数
    ///
    /// * `total_sectors` — 总扇区数
    /// * `reserved_margin` — 安全水位（保留不被预留的扇区数）
    pub fn new(total_sectors: u64, reserved_margin: u64) -> Self {
        Self {
            total_reserved: AtomicU64::new(0),
            max_reserved: total_sectors.saturating_sub(reserved_margin),
        }
    }

    /// 当前可用预留预算
    ///
    /// 返回还可预留的扇区数（max_reserved - total_reserved）。
    pub fn available(&self) -> u64 {
        self.max_reserved
            .saturating_sub(self.total_reserved.load(Ordering::Relaxed))
    }

    /// 获取最大可预留扇区数
    pub fn max_reservable(&self) -> u64 {
        self.max_reserved
    }

    /// 预留指定扇区数 — 追加到已有 reservation
    ///
    /// 对应 bcachefs `bch2_disk_reservation_add()`（`buckets.h:358-378`）。
    ///
    /// 检查预算是否足够：
    /// - 足够 → 增加 total_reserved，res.sectors 累加，返回 Ok
    /// - 不足 + NOFAIL → 仍然增加 total_reserved（强制预留），返回 Ok
    /// - 不足 + PARTIAL → 取可用最小值，返回 Ok
    /// - 不足 + 无标志 → 返回 Err
    ///
    /// # 参数
    ///
    /// * `sectors` — 需要预留的扇区数
    /// * `data_type` — 分配操作的数据类型
    /// * `flags` — 预留标志（NOFAIL/PARTIAL）
    pub fn bch2_disk_reservation_add(
        &self,
        res: &mut DiskReservation,
        sectors: u64,
        data_type: BchDataType,
        flags: BchReservationFlags,
    ) -> Result<(), StorageError> {
        if sectors == 0 {
            return Ok(());
        }

        let current = self.total_reserved.fetch_add(sectors, Ordering::AcqRel);
        let would_be = current + sectors;

        if would_be > self.max_reserved {
            match flags {
                BchReservationFlags::Nofail => {
                    // NOFAIL: 即使超预算也强制预留（用于不可回滚的元数据写入）
                    // total_reserved 已经增加了 sectors，保持现状
                    res.sectors.set(res.sectors.get() + sectors);
                    res.data_type = data_type;
                    return Ok(());
                }
                BchReservationFlags::Partial => {
                    // PARTIAL: 取可用最大值
                    let available = self.max_reserved.saturating_sub(current);
                    if available == 0 {
                        // 完全不可用 → 回滚本次增加
                        self.total_reserved.fetch_sub(sectors, Ordering::Release);
                        return Err(StorageError::AddressSpaceExhausted {
                            max_raw_addr: self.max_reserved,
                        });
                    }
                    // 回滚本次增加，换用可用量
                    self.total_reserved.fetch_sub(sectors, Ordering::Release);
                    let actual = available;
                    self.total_reserved.fetch_add(actual, Ordering::AcqRel);
                    res.sectors.set(res.sectors.get() + actual);
                    res.data_type = data_type;
                    return Ok(());
                }
                BchReservationFlags::None => {
                    // 无标志：超预算 → 回滚并返回错误
                    self.total_reserved.fetch_sub(sectors, Ordering::Release);
                    return Err(StorageError::AddressSpaceExhausted {
                        max_raw_addr: self.max_reserved,
                    });
                }
            }
        }

        res.sectors.set(res.sectors.get() + sectors);
        res.data_type = data_type;
        Ok(())
    }

    /// 初始化并预留 — 对应 bcachefs `bch2_disk_reservation_get()`
    ///
    /// 组合了 `init()` + `add()`，一次完成预留创建。
    pub fn bch2_disk_reservation_get(
        &self,
        sectors: u64,
        nr_replicas: u32,
        data_type: BchDataType,
        flags: BchReservationFlags,
    ) -> Result<DiskReservation, StorageError> {
        let mut res = DiskReservation::init(nr_replicas);
        self.bch2_disk_reservation_add(&mut res, sectors * nr_replicas as u64, data_type, flags)?;
        Ok(res)
    }

    /// 确认已用空间（部分消耗预留）
    ///
    /// 对应 bcachefs 中 `bch2_trans_account_disk_usage_change()` 将事务中已用的扇区
    /// 从 reservation 中减去的逻辑（`buckets.c:594-596`）。
    ///
    /// 与 `put()` 的区别：commit 按实际使用量消耗并递减 res.sectors；put 释放全部剩余并清零。
    ///
    /// # 参数
    ///
    /// * `used_sectors` — 实际已使用的扇区数
    pub fn bch2_disk_reservation_commit(&self, res: &DiskReservation, used_sectors: u64) {
        if used_sectors == 0 || res.sectors.get() == 0 {
            return;
        }
        let actual = used_sectors.min(res.sectors.get());
        self.total_reserved.fetch_sub(actual, Ordering::Release);
        res.sectors.set(res.sectors.get() - actual);
    }

    /// 释放预留 — 对应 bcachefs `bch2_disk_reservation_put()`（`buckets.h:341-348`）
    ///
    /// 释放整个 reservation 的所有剩余扇区并将 res.sectors 清零。
    ///
    /// 与 `commit()` 的区别：put 释放全部并清零；commit 只按实际用量部分消耗。
    pub fn bch2_disk_reservation_put(&self, res: &DiskReservation) {
        let remaining = res.sectors.replace(0);
        if remaining == 0 {
            return;
        }
        self.total_reserved.fetch_sub(remaining, Ordering::Release);
    }
}

impl std::fmt::Debug for ReservationTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReservationTracker")
            .field(
                "total_reserved",
                &self.total_reserved.load(Ordering::Relaxed),
            )
            .field("max_reserved", &self.max_reserved)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reservation_empty() {
        let r = DiskReservation::init(1);
        assert!(r.is_empty());
        assert!(!r.is_valid());
        assert_eq!(r.sectors.get(), 0);
        assert_eq!(r.nr_replicas, 1);
    }

    #[test]
    fn test_reservation_new() {
        let r = DiskReservation::new(256, BchDataType::User);
        assert!(!r.is_empty());
        assert!(r.is_valid());
        assert_eq!(r.sectors.get(), 256);
        assert_eq!(r.data_type, BchDataType::User);
        assert_eq!(r.nr_replicas, 1);
    }

    #[test]
    fn test_tracker_new() {
        let t = ReservationTracker::new(4096, 512);
        assert_eq!(t.max_reservable(), 3584);
        assert_eq!(t.available(), 3584);
    }

    #[test]
    fn test_reserve_and_commit() {
        let t = ReservationTracker::new(4096, 0);
        let mut res = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut res, 256, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        assert_eq!(res.sectors.get(), 256);
        assert_eq!(t.available(), 4096 - 256);

        // commit 消耗部分 — total_reserved 减小 128，res.sectors 也减 128
        t.bch2_disk_reservation_commit(&res, 128);
        assert_eq!(
            res.sectors.get(),
            128,
            "commit should decrement res.sectors"
        );
        assert_eq!(t.available(), 4096 - 256 + 128);

        // put 释放全部剩余
        t.bch2_disk_reservation_put(&res);
        assert_eq!(res.sectors.get(), 0, "put should zero out res.sectors");
        assert_eq!(t.available(), 4096);
    }

    #[test]
    fn test_reserve_and_put() {
        let t = ReservationTracker::new(4096, 0);
        let mut res = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut res, 128, BchDataType::Btree, BchReservationFlags::None)
            .unwrap();
        assert_eq!(t.available(), 4096 - 128);

        // put 释放全部（不可变 API）
        t.bch2_disk_reservation_put(&res);
        assert_eq!(t.available(), 4096);
    }

    #[test]
    fn test_reserve_exhaustion() {
        let t = ReservationTracker::new(256, 0);
        let mut r1 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r1, 200, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        let mut r2 = DiskReservation::init(1);
        let result =
            t.bch2_disk_reservation_add(&mut r2, 100, BchDataType::User, BchReservationFlags::None);
        assert!(result.is_err(), "should fail when over budget");
    }

    #[test]
    fn test_reserve_zero() {
        let t = ReservationTracker::new(4096, 0);
        let mut res = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut res, 0, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn test_available_after_commit_and_new_reserve() {
        let t = ReservationTracker::new(4096, 1024);
        assert_eq!(t.available(), 3072);

        let mut r1 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r1, 512, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        assert_eq!(t.available(), 3072 - 512);

        t.bch2_disk_reservation_commit(&r1, 512);
        assert_eq!(t.available(), 3072);

        let mut r2 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r2, 1024, BchDataType::Btree, BchReservationFlags::None)
            .unwrap();
        assert_eq!(t.available(), 3072 - 1024);

        t.bch2_disk_reservation_put(&r2);
        assert_eq!(t.available(), 3072);
    }

    #[test]
    fn test_reserve_nofail_over_budget() {
        let t = ReservationTracker::new(256, 0);
        let mut r1 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r1, 200, BchDataType::User, BchReservationFlags::None)
            .unwrap();

        // NOFAIL 即使超预算也成功
        let mut r2 = DiskReservation::init(1);
        let result = t.bch2_disk_reservation_add(
            &mut r2,
            100,
            BchDataType::Btree,
            BchReservationFlags::Nofail,
        );
        assert!(
            result.is_ok(),
            "NOFAIL should succeed even when over budget"
        );
        assert_eq!(r2.sectors.get(), 100);
    }

    #[test]
    fn test_reserve_partial_over_budget() {
        let t = ReservationTracker::new(256, 0);
        let mut r1 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r1, 200, BchDataType::User, BchReservationFlags::None)
            .unwrap();

        // PARTIAL: 可用 = 56, 请求 = 100, 应取 56
        let mut r2 = DiskReservation::init(1);
        let result = t.bch2_disk_reservation_add(
            &mut r2,
            100,
            BchDataType::Btree,
            BchReservationFlags::Partial,
        );
        assert!(
            result.is_ok(),
            "PARTIAL should succeed when some space available"
        );
        assert_eq!(
            r2.sectors.get(),
            56,
            "PARTIAL should take available amount (256-200=56)"
        );
    }

    #[test]
    fn test_reserve_partial_totally_exhausted() {
        let t = ReservationTracker::new(256, 0);
        let mut r1 = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut r1, 256, BchDataType::User, BchReservationFlags::None)
            .unwrap();

        // PARTIAL 但完全不可用
        let mut r2 = DiskReservation::init(1);
        let result = t.bch2_disk_reservation_add(
            &mut r2,
            100,
            BchDataType::Btree,
            BchReservationFlags::Partial,
        );
        assert!(
            result.is_err(),
            "PARTIAL should fail when totally exhausted"
        );
    }

    #[test]
    fn test_disk_reservation_get_with_replicas() {
        let t = ReservationTracker::new(4096, 0);
        // request 100 sectors with 3 replicas → need 300
        let res = t
            .bch2_disk_reservation_get(100, 3, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        assert_eq!(res.sectors.get(), 300);
        assert_eq!(res.nr_replicas, 3);
        assert_eq!(t.available(), 4096 - 300);

        t.bch2_disk_reservation_put(&res);
    }

    #[test]
    fn test_commit_does_not_consume_all() {
        let t = ReservationTracker::new(4096, 0);
        let mut res = DiskReservation::init(1);
        t.bch2_disk_reservation_add(&mut res, 256, BchDataType::User, BchReservationFlags::None)
            .unwrap();
        assert_eq!(t.available(), 4096 - 256);

        // commit 部分（100 sectors）— total_reserved 减 100，res.sectors 也减 100
        t.bch2_disk_reservation_commit(&res, 100);
        assert_eq!(
            res.sectors.get(),
            156,
            "commit should decrement remaining sectors"
        );
        assert_eq!(
            t.available(),
            4096 - 256 + 100,
            "commit freed 100 sectors from budget"
        );

        // put 释放剩余（剩余 156）
        t.bch2_disk_reservation_put(&res);
        assert_eq!(res.sectors.get(), 0, "put should zero res.sectors");
        assert_eq!(t.available(), 4096, "put freed all remaining");
    }
}
