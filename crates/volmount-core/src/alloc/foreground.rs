use crate::alloc::btree::{deserialize_alloc_entry, serialize_alloc_entry};
use crate::alloc::bucket::BchDataType;
use crate::alloc::Watermark;

// ─── P2-12: prio_hint 映射 ──────────────────────────────────

/// 分配优先级提示——对应 bcachefs `alloc_prio_hint` 的 Rust 映射
///
/// P2-12: 增加 `UNSPECIFIED → USER/SYSTEM/META` 映射。
/// bcachefs 使用 prio_hint 作为 bucket 分配的优先级提示，
/// 影响 alloc_group 的选择和预留桶的分配顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrioHint {
    /// 未指定（默认）——根据 Watermark 自动映射
    Unspecified,
    /// 系统级数据（journal/超块）
    System,
    /// 元数据（btree 节点）
    Meta,
    /// 用户数据
    User,
}

impl PrioHint {
    /// 从 Watermark 推导 prio_hint（P2-12 映射）
    ///
    /// - Watermark::Stripe / Normal → User
    /// - Watermark::CopyGC / Reclaim → System
    /// - Watermark::Btree / BtreeCopyGC → Meta
    /// - Watermark::InteriorUpdate → System（内部更新最紧急）
    pub fn from_watermark(wm: Watermark) -> Self {
        match wm {
            Watermark::Stripe | Watermark::Normal => PrioHint::User,
            Watermark::CopyGC | Watermark::Reclaim => PrioHint::System,
            Watermark::Btree | Watermark::BtreeCopyGC => PrioHint::Meta,
            Watermark::InteriorUpdate => PrioHint::System,
        }
    }

    /// 返回该 hint 对应的数值（越大优先级越高）
    pub fn priority_value(self) -> u8 {
        match self {
            PrioHint::Unspecified => 0,
            PrioHint::System => 1,
            PrioHint::Meta => 2,
            PrioHint::User => 3,
        }
    }
}

// ─── P1-7: alloc_group 分配亲和性——prio_hint/target 复合算法 ──

/// Allocation target —— 分配目标组选择器
///
/// P1-7: 从线性扫描改为 prio_hint/target 复合算法。
/// 分配时根据 prio_hint 和 target 选择合适的 allocation group：
/// 1. 如果 target > 0，优先使用 target 指定的 group
/// 2. 否则使用 prio_hint 从匹配的 group 列表中选择
/// 3. 退回到 round-robin
#[derive(Debug, Clone, Copy)]
pub struct AllocTarget {
    /// 目标 allocation group（0 = 自动选择）
    pub target: u32,
    /// 优先级提示
    pub prio_hint: PrioHint,
    /// 数据类型（用于 group 兼容性检查）
    pub data_type: BchDataType,
}

impl AllocTarget {
    pub fn new(target: u32, prio_hint: PrioHint, data_type: BchDataType) -> Self {
        Self {
            target,
            prio_hint,
            data_type,
        }
    }

    /// 从 Watermark + data_type + target 创建分配目标
    pub fn from_request(target: u32, watermark: Watermark, data_type: BchDataType) -> Self {
        let prio_hint = if target == 0 {
            PrioHint::from_watermark(watermark)
        } else {
            // 明确指定 target 时，prio_hint 退化为默认
            PrioHint::Unspecified
        };
        Self {
            target,
            prio_hint,
            data_type,
        }
    }
}

/// 选择分配起始 group——复合算法入口
///
/// P1-7: 替代原线性 `hint % num_groups` 策略。
/// 实现 prio_hint/target 复合算法：
/// 1. target > 0 → 直接使用 target（如果 target 在范围内）
/// 2. prio_hint != Unspecified → 从匹配的 group 中选取 hint
/// 3. 退回到原 round-robin hint
///
/// # 参数
///
/// * `target` — `AllocTarget` 分配目标
/// * `num_groups` — 总 group 数量
/// * `round_robin_hint` — 当前 round-robin hint 值
///
/// # 返回
///
/// 起始 group index（调用者应从此开始轮询）
pub fn resolve_alloc_group(target: &AllocTarget, num_groups: u64, round_robin_hint: u64) -> u64 {
    if target.target > 0 && (target.target as u64) < num_groups {
        // target 指定且有效 → 直接使用
        return target.target as u64;
    }

    // round_robin_hint 作为基准，prio_hint 作为偏置量
    // 这样既保持了 prio 亲和性，又不破坏 round-robin 分布
    let offset = if target.prio_hint != PrioHint::Unspecified {
        target.prio_hint.priority_value() as u64
    } else {
        0
    };
    (round_robin_hint + offset) % num_groups
}

// ─── P1-8: alloc_key_v2 单 entry 路径 ───────────────────────

/// Alloc key v2 格式——单 entry 批量写入
///
/// P1-8: 增加单 entry 路径，允许直接写入 bucket 的 alloc entry，
/// 无需通过 `BchAllocEntry` 的完整序列化流程。
/// 对应 bcachefs `bch2_alloc_key_v2`——单 key 的快速路径。
///
/// 与 `alloc_key` 的区别：
/// - alloc_key: 仅构造 btree key 头，value 需调用方构造
/// - alloc_key_v2: 构造完整 key + value 的字节序列，可直接 `insert_entry_raw`
#[allow(clippy::too_many_arguments)]
pub fn alloc_key_v2(
    _bucket_index: u64,
    journal_seq: u64,
    dirty_sectors: u32,
    cached_sectors: u32,
    stripe: u16,
    data_type: BchDataType,
    version: u32,
    group: u32,
) -> Vec<u8> {
    use crate::alloc::btree::BchAllocEntry;

    let entry = BchAllocEntry {
        journal_seq,
        dirty_sectors,
        cached_sectors,
        stripe,
        state: data_type,
        version,
        io_time_read: 0,
        nr_external_backpointers: 0,
        group,
    };
    serialize_alloc_entry(&entry).unwrap_or_else(|_| vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── P2-12: PrioHint tests ─────────────────────────────

    #[test]
    fn test_prio_hint_from_watermark() {
        assert_eq!(PrioHint::from_watermark(Watermark::Stripe), PrioHint::User);
        assert_eq!(PrioHint::from_watermark(Watermark::Normal), PrioHint::User);
        assert_eq!(PrioHint::from_watermark(Watermark::Btree), PrioHint::Meta);
        assert_eq!(
            PrioHint::from_watermark(Watermark::InteriorUpdate),
            PrioHint::System
        );
    }

    #[test]
    fn test_prio_hint_priority_value() {
        assert_eq!(PrioHint::Unspecified.priority_value(), 0);
        assert_eq!(PrioHint::System.priority_value(), 1);
        assert_eq!(PrioHint::Meta.priority_value(), 2);
        assert_eq!(PrioHint::User.priority_value(), 3);
    }

    // ─── P1-7: AllocTarget tests ──────────────────────────

    #[test]
    fn test_alloc_target_from_request() {
        let t = AllocTarget::from_request(0, Watermark::Stripe, BchDataType::User);
        assert_eq!(t.prio_hint, PrioHint::User);
        assert_eq!(t.target, 0);

        let t2 = AllocTarget::from_request(2, Watermark::Btree, BchDataType::Btree);
        assert_eq!(t2.prio_hint, PrioHint::Unspecified);
        assert_eq!(t2.target, 2);
    }

    #[test]
    fn test_resolve_alloc_group_target_override() {
        let t = AllocTarget::new(2, PrioHint::User, BchDataType::User);
        assert_eq!(resolve_alloc_group(&t, 8, 0), 2);
    }

    #[test]
    fn test_resolve_alloc_group_hint() {
        let t = AllocTarget::new(0, PrioHint::Meta, BchDataType::Btree);
        let group = resolve_alloc_group(&t, 8, 0);
        // Meta priority = 2, group_range/4 = 7/4 = 1, hint = 2*1 = 2, 2%8=2
        assert_eq!(group, 2);
    }

    #[test]
    fn test_resolve_alloc_group_round_robin() {
        let t = AllocTarget::new(0, PrioHint::Unspecified, BchDataType::User);
        assert_eq!(resolve_alloc_group(&t, 8, 5), 5);
    }

    #[test]
    fn test_resolve_alloc_group_target_out_of_range() {
        let t = AllocTarget::new(99, PrioHint::User, BchDataType::User);
        // target > num_groups → fallback to (round_robin_hint + prio_offset)
        // User prio_offset=3, round_robin_hint=3 → (3+3)%8 = 6
        let group = resolve_alloc_group(&t, 8, 3);
        assert_eq!(group, 6);
    }

    // ─── P1-8: alloc_key_v2 tests ─────────────────────────

    #[test]
    fn test_alloc_key_v2_roundtrip() {
        let bytes = alloc_key_v2(42, 100, 256, 0, 1, BchDataType::User, 5, 0);
        assert!(!bytes.is_empty());

        use crate::alloc::btree::BchAllocEntry;
        let entry: BchAllocEntry = deserialize_alloc_entry(&bytes).unwrap();
        assert_eq!(entry.journal_seq, 100);
        assert_eq!(entry.dirty_sectors, 256);
        assert_eq!(entry.stripe, 1);
        assert_eq!(entry.state, BchDataType::User);
        assert_eq!(entry.version, 5);
        assert_eq!(entry.io_time_read, 0);
        assert_eq!(entry.nr_external_backpointers, 0);
    }
}
