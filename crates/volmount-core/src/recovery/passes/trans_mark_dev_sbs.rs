use crate::alloc::btree::serialize_alloc_entry;
use crate::alloc::btree::BchAllocEntry;
use crate::alloc::{BchAllocator, BchDataType, BLOCKS_PER_BUCKET};
use crate::btree::key::KeyValue;
use crate::btree::{Bpos, BtreeEngine, BtreeEntry, BtreeId, KeyType};
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// TransMarkDevSbs pass — 标记 superblock 和 journal 区域到 Alloc btree
///
/// 对应 bcachefs `bch2_trans_mark_dev_sbs()` (PASS_ALWAYS #6)。
/// 将 superblock bucket (0) 标记为 `BchDataType::Sb`，
/// journal bucket 标记为 `BchDataType::Journal`，防止普通分配使用。
///
/// bcachefs 通过 trigger pipeline (`BTREE_TRIGGER_transactional`) 同步 allocator 状态。
/// volmount 当前直接写 btree 节点 + 手动 `btree_bitmap_mark()`，功能等价。
///
/// # 幂等性
/// 多次写入相同的 BchAllocEntry 结果不变。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // 1. 标记 superblock bucket (0) — 对齐 bcachefs `BCH_DATA_sb`
    mark_alloc_bucket(&mut state.engine, &state.allocator, 0, BchDataType::Sb)?;

    // 2. 标记 journal bucket — 对齐 bcachefs `BCH_DATA_journal`
    let js = state.journal.to_superblock_state();
    for &addr in &js.bucket_addrs {
        let bucket_idx = addr / BLOCKS_PER_BUCKET;
        mark_alloc_bucket(
            &mut state.engine,
            &state.allocator,
            bucket_idx,
            BchDataType::Journal,
        )?;
    }

    Ok(())
}

/// 在 Alloc btree 中标记一个 bucket 为指定类型，同步更新 allocator bitmap
///
/// bcachefs 通过触发 trigger pipeline 同步 allocator 状态。
/// volmount 在当前架构下直接写 btree + 手动 bitmap_mark，功能等价。
fn mark_alloc_bucket(
    engine: &mut BtreeEngine,
    allocator: &BchAllocator,
    bucket_idx: u64,
    data_type: BchDataType,
) -> Result<(), StorageError> {
    let alloc_entry = BchAllocEntry {
        journal_seq: 0,
        dirty_sectors: 0,
        cached_sectors: 0,
        stripe: 0,
        state: data_type,
        version: 0,
        io_time_read: 0,
        nr_external_backpointers: 0,
        group: 0,
    };
    let raw = serialize_alloc_entry(&alloc_entry)
        .map_err(|e| StorageError::InvalidData(format!("serialize alloc entry: {}", e)))?;
    let entry = BtreeEntry {
        pos: Bpos::new(0, bucket_idx, 0),
        key_type: KeyType::Normal,
        value: KeyValue::Raw(raw),
    };
    engine.insert_entry_raw(BtreeId::Alloc, entry, 0);
    allocator.btree_bitmap_mark(bucket_idx);
    Ok(())
}
