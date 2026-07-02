//! Alloc btree — 分配器持久化状态
//!
//! 每个 bucket 的状态存储在 Alloc btree 中，key = bucket_index，value = BchAllocEntry。
//! 与 alloc/mod.rs 的内存分配器配合使用：内存分配器负责并发分配，
//! Alloc btree 负责持久化和恢复。

use serde::{Deserialize, Serialize};

use crate::alloc::bucket::BchDataType;
use crate::types::StorageError;

/// Alloc btree 中存储的每个 bucket 的状态
///
/// Bucket 在 Alloc btree 中用 BchAllocEntry 表示。
/// bpos(bucket_index, 0, 0) -> BchAllocEntry
///
/// bcachefs 对齐：字段布局匹配 C `bch_alloc_v4`（alloc_background.h）：
/// - field_seq (journal_seq), dirty_sectors, cached_sectors, stripe (u16),
///   data_type (derive from sectors), gen (version 的下位 8 位)
/// - volmount 扩展字段：group（分配组 ID）、io_time_read、nr_external_backpointers
///   以 `#[serde(default)]` 向后兼容。
///
/// P0-1: stripe 从 u32 → u16，字段重新排序对齐 C 布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BchAllocEntry {
    /// Journal sequence number — 对应 C bch_alloc_v4.field_seq
    pub journal_seq: u64,
    /// 脏扇区计数 — bcachefs 从 dirty_sectors > 0 推导 data_type
    pub dirty_sectors: u32,
    /// 缓存扇区计数
    pub cached_sectors: u32,
    /// 条带引用计数（C 中为 __u16，P0-1: u32 → u16）
    pub stripe: u16,
    /// Bucket 状态（从 sector 计数推导）
    pub state: BchDataType,
    /// 版本号（防 ABA，每次状态变更递增；C 中为 gen 位域）
    pub version: u32,
    /// 最近一次 read I/O 时间戳（用于 LRU 计算）
    #[serde(default)]
    pub io_time_read: u64,
    /// 外部 backpointer 数量（用于 backpointer 检查）
    #[serde(default)]
    pub nr_external_backpointers: u32,
    /// 所属 allocation group（volmount 扩展字段）
    #[serde(default)]
    pub group: u32,
}

impl BchAllocEntry {
    /// 创建空闲条目
    pub const fn free(group: u32) -> Self {
        Self {
            journal_seq: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            state: BchDataType::Free,
            version: 0,
            io_time_read: 0,
            nr_external_backpointers: 0,
            group,
        }
    }

    /// 是否为空闲
    pub fn is_free(&self) -> bool {
        self.state == BchDataType::Free
    }

    /// 从 Bucket 转换
    pub fn from_bucket(group: u32, state: BchDataType, version: u32) -> Self {
        Self {
            journal_seq: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            state,
            version,
            io_time_read: 0,
            nr_external_backpointers: 0,
            group,
        }
    }

    /// 从 Bucket 转换，携带 journal_seq
    ///
    /// P0-4: journal_seq 写入路径修复——确保 format 字段与 journal entry 兼容。
    /// 在 bcachefs C 中，bch_alloc_v4.field_seq 记录最后引用此桶的 journal entry seq，
    /// 用于分配前安全检查和 crash recovery 时的依赖追踪。
    /// 当 journal_seq > 0 时写入对应 seq；journal_seq == 0 时保持 0（无 journal 追踪）。
    pub fn from_bucket_with_journal_seq(
        group: u32,
        state: BchDataType,
        version: u32,
        journal_seq: u64,
    ) -> Self {
        Self {
            journal_seq,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            state,
            version,
            io_time_read: 0,
            nr_external_backpointers: 0,
            group,
        }
    }
}

/// Alloc btree key helper — 用于从 bucket_index 构造 btree key
pub fn alloc_key(bucket_index: u64) -> crate::btree::BtreeKey {
    crate::btree::BtreeKey::from_bpos(
        // bucket_index 存到 offset 中（BtreeKey 只存 offset+snapshot，不存 vol_id）
        crate::btree::Bpos::new(0, bucket_index, 0),
        crate::btree::KeyType::Normal,
    )
}

/// 将 Alloc entry 编码为 bincode 字节串。
pub fn serialize_alloc_entry(entry: &BchAllocEntry) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(entry)
}

/// 解码 Alloc entry，兼容旧版仅含前置字段的布局。
pub fn deserialize_alloc_entry(bytes: &[u8]) -> Result<BchAllocEntry, StorageError> {
    match bincode::deserialize::<BchAllocEntry>(bytes) {
        Ok(entry) => Ok(entry),
        Err(primary_err) => {
            #[derive(Deserialize)]
            struct LegacyBchAllocEntry {
                journal_seq: u64,
                dirty_sectors: u32,
                cached_sectors: u32,
                stripe: u16,
                state: BchDataType,
                version: u32,
                group: u32,
            }

            match bincode::deserialize::<LegacyBchAllocEntry>(bytes) {
                Ok(legacy) => Ok(BchAllocEntry {
                    journal_seq: legacy.journal_seq,
                    dirty_sectors: legacy.dirty_sectors,
                    cached_sectors: legacy.cached_sectors,
                    stripe: legacy.stripe,
                    state: legacy.state,
                    version: legacy.version,
                    io_time_read: 0,
                    nr_external_backpointers: 0,
                    group: legacy.group,
                }),
                Err(legacy_err) => Err(StorageError::InvalidData(format!(
                    "deserialize alloc entry: {primary_err}; legacy fallback failed: {legacy_err}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_entry_free() {
        let e = BchAllocEntry::free(1);
        assert!(e.is_free());
        assert_eq!(e.group, 1);
        assert_eq!(e.version, 0);
        assert_eq!(e.stripe, 0);
        assert_eq!(e.io_time_read, 0);
        assert_eq!(e.nr_external_backpointers, 0);
    }

    #[test]
    fn test_alloc_entry_from_bucket() {
        let e = BchAllocEntry::from_bucket(0, BchDataType::User, 5);
        assert!(!e.is_free());
        assert_eq!(e.state, BchDataType::User);
        assert_eq!(e.version, 5);
        assert_eq!(e.stripe, 0);
        assert_eq!(e.io_time_read, 0);
        assert_eq!(e.nr_external_backpointers, 0);
    }

    #[test]
    fn test_alloc_entry_serde_roundtrip() {
        let e = BchAllocEntry::free(2);
        let data = bincode::serialize(&e).unwrap();
        let restored: BchAllocEntry = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.state, e.state);
        assert_eq!(restored.group, e.group);
        assert_eq!(restored.stripe, e.stripe);
    }

    /// P0-1: 验证 u16 stripe 序列化往返
    #[test]
    fn test_alloc_entry_stripe_u16() {
        let mut e = BchAllocEntry::free(0);
        e.stripe = 0xFFFF;
        let data = bincode::serialize(&e).unwrap();
        let restored: BchAllocEntry = bincode::deserialize(&data).unwrap();
        assert_eq!(restored.stripe, 0xFFFF);
        // bincode 1.x 将 C-like enum 序列化为 u32（4 字节）：
        // journal_seq(8) + dirty_sectors(4) + cached_sectors(4) + stripe(2)
        //   + state(4) + version(4) + io_time_read(8)
        //   + nr_external_backpointers(4) + group(4) = 42 字节
        // 其中 stripe 从 u32→u16 节省了 2 字节
        // 注：旧版 stripe=u32 时序列化大小为 44 字节
        assert_eq!(
            data.len(),
            42,
            "BchAllocEntry should be compact after stripe u32→u16"
        );
    }

    /// P0-1: 验证空白数据反序列化（#[serde(default)] 字段）
    #[test]
    fn test_alloc_entry_serde_default_fields_from_legacy_shape() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct LegacyBchAllocEntry {
            journal_seq: u64,
            dirty_sectors: u32,
            cached_sectors: u32,
            stripe: u16,
            state: BchDataType,
            version: u32,
            group: u32,
        }

        let legacy = LegacyBchAllocEntry {
            journal_seq: 0,
            dirty_sectors: 0,
            cached_sectors: 0,
            stripe: 0,
            state: BchDataType::Free,
            version: 0,
            group: 7,
        };
        let data = bincode::serialize(&legacy).unwrap();
        let restored = deserialize_alloc_entry(&data).unwrap();
        assert_eq!(restored.state, BchDataType::Free);
        assert_eq!(restored.group, 7);
        assert_eq!(restored.io_time_read, 0);
        assert_eq!(restored.nr_external_backpointers, 0);
    }

    #[test]
    fn test_alloc_key_format() {
        let key = alloc_key(42);
        // BtreeKey 将 bucket_index 存储到 offset(vaddr), vol_id=0
        assert_eq!(key.to_bpos().offset, 42);
        assert_eq!(key.to_bpos().snapshot, 0);
    }
}
