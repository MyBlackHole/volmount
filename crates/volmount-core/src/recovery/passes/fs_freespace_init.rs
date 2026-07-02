use crate::alloc::{BchAllocator, BchDataType, Bucket};
use crate::btree::{BtreeEngine, BtreeId};
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: Freespace btree 初始化（对应 bcachefs `bch2_fs_freespace_init()`）
///
/// 遍历 allocator 所有 bucket，将空闲 bucket 写入 Freespace btree。
/// 对应 bcachefs `bch2_fs_freespace_init()` 的 alloc btree 扫描路径。
///
/// # 幂等性
/// Freespace btree key 包含 bucket_index + gen，重复写入相同 key 结果不变。
/// 本 pass 可多次运行不产生副作用。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    if state.engine.get(BtreeId::Freespace).key_count() > 0 {
        return Ok(());
    }
    bch2_fs_freespace_init(&mut state.engine, &state.allocator)
}

/// 核心实现 — 由 fs_freespace_init pass 调用
///
/// 遍历 allocator 所有 bucket，对每个 Free 状态的 bucket 调用
/// `bch2_freespace_insert()` 写入 Freespace btree。
pub(crate) fn bch2_fs_freespace_init(
    engine: &mut BtreeEngine,
    allocator: &BchAllocator,
) -> Result<(), StorageError> {
    allocator.for_each_bucket(|bucket_idx, bucket: &Bucket| {
        if bucket.state == BchDataType::Free {
            bch2_freespace_insert_core(engine, bucket_idx, bucket.version);
        }
    });
    Ok(())
}

/// 在 Freespace btree 中插入空闲 bucket 条目
///
/// key = Bpos(0, bucket_index, gen)，value = empty。
/// gen 用于检测 stale：分配时通过 gen 匹配确保使用的 bucket 未被重新分配过。
fn bch2_freespace_insert_core(engine: &mut BtreeEngine, bucket_index: u64, gen: u32) {
    use crate::btree::key::KeyType;
    use crate::btree::{Bpos, BtreeEntry};
    let pos = Bpos::new(0, bucket_index, gen);
    if engine.get_entry_raw(BtreeId::Freespace, pos).is_some() {
        return;
    }
    engine.insert_entry_raw(
        BtreeId::Freespace,
        BtreeEntry::raw(pos, KeyType::Normal, vec![]),
        0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::{Bpos, BtreeEngine, BtreeId};
    use crate::journal::Journal;
    use crate::meta::VolumeMeta;
    use crate::recovery::RecoveryState;
    use crate::types::BackendType;
    use std::sync::Arc;

    fn make_allocator() -> BchAllocator {
        let allocator = BchAllocator::new(4096, 4096, 0);
        allocator.for_each_bucket_mut(|bucket_idx, bucket| {
            if bucket_idx == 3 {
                bucket.state = BchDataType::User;
                bucket.version = 7;
            }
        });
        allocator
    }

    #[test]
    fn test_fs_freespace_init_inserts_only_free_buckets() {
        let allocator = make_allocator();
        let mut engine = BtreeEngine::new();

        bch2_fs_freespace_init(&mut engine, &allocator).unwrap();

        let free_entry = engine.get_entry_raw(BtreeId::Freespace, Bpos::new(0, 0, 0));
        assert!(free_entry.is_some(), "free bucket should be inserted");

        let allocated_entry = engine.get_entry_raw(BtreeId::Freespace, Bpos::new(0, 3, 7));
        assert!(
            allocated_entry.is_none(),
            "non-free bucket should not be inserted"
        );
    }

    #[tokio::test]
    async fn test_fs_freespace_init_is_idempotent() {
        let allocator = make_allocator();
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![100]);
        let sb = crate::storage::superblock::BchSb::new(VolumeMeta::new(
            "test".into(),
            1,
            "default".into(),
            4096,
            1024 * 1024,
            BackendType::Sparse,
        ));
        let mut state = RecoveryState::new(BtreeEngine::new(), journal, backend, sb, allocator);

        run(&mut state).await.unwrap();
        let first_count = state.engine.get(BtreeId::Freespace).key_count();

        run(&mut state).await.unwrap();
        let second_count = state.engine.get(BtreeId::Freespace).key_count();

        assert_eq!(first_count, second_count, "second rebuild must be a no-op");
    }
}
