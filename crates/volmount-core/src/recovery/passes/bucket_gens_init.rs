use crate::alloc::{BchAllocator, BchBucketGens, BUCKET_GENS_PER_KEY};
use crate::btree::{Bpos, Btree, BtreeEngine, BtreeEntry, BtreeId, KeyType};
use crate::recovery::RecoveryState;
use crate::types::StorageError;
use std::collections::HashMap;

/// Pass: bucket_gens 初始化（对应 bcachefs `bch2_bucket_gens_init()`）
///
/// 扫描 allocator 的所有 bucket，按 `(group, bucket_idx / 256)` 聚合，
/// 为每个 chunk 写入一个 `bucket_gens` 记录。
///
/// # 幂等性
/// 该 pass 每次运行都会先重置 `BucketGens` btree，再根据 allocator 状态
/// 重新生成内容，因此可重复执行且不会累积旧 key。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    *state.engine.get_mut(BtreeId::BucketGens) = Btree::new();
    bch2_bucket_gens_init(&mut state.engine, &state.allocator)
}

/// 核心实现：将 allocator 的 bucket 版本写入 bucket_gens btree。
pub(crate) fn bch2_bucket_gens_init(
    engine: &mut BtreeEngine,
    allocator: &BchAllocator,
) -> Result<(), StorageError> {
    let mut grouped: HashMap<(u32, u64), BchBucketGens> = HashMap::new();

    allocator.for_each_bucket(|bucket_idx, bucket| {
        let chunk_idx = bucket_idx / BUCKET_GENS_PER_KEY as u64;
        let slot = (bucket_idx % BUCKET_GENS_PER_KEY as u64) as usize;
        grouped
            .entry((bucket.group, chunk_idx))
            .or_insert_with(BchBucketGens::new)
            .set(slot, bucket.version as u8);
    });

    for ((group, chunk_idx), gens) in grouped {
        let pos = Bpos::new(group as u64, chunk_idx, 0);
        let bytes = bincode::serialize(&gens)
            .map_err(|e| StorageError::InvalidData(format!("serialize bucket gens: {e}")))?;
        engine.insert_entry_raw(
            BtreeId::BucketGens,
            BtreeEntry::raw(pos, KeyType::Normal, bytes),
            0,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::journal::Journal;
    use crate::meta::VolumeMeta;
    use crate::recovery::RecoveryState;
    use crate::storage::superblock::BchSb;
    use crate::types::BackendType;
    use std::sync::Arc;

    fn make_state(allocator: BchAllocator) -> RecoveryState {
        RecoveryState::new(
            BtreeEngine::new(),
            Journal::new(vec![1]),
            Arc::new(MockBlockDevice::new()),
            BchSb::new(VolumeMeta::new(
                "test".into(),
                1,
                "default".into(),
                4096,
                1024 * 1024,
                BackendType::Sparse,
            )),
            allocator,
        )
    }

    #[test]
    fn test_bucket_gens_init_writes_chunked_entries() {
        let allocator = BchAllocator::new(131_072, 131_072, 0);
        allocator.for_each_bucket_mut(|bucket_idx, bucket| match bucket_idx {
            0 => bucket.version = 1,
            255 => bucket.version = 7,
            256 => bucket.version = 3,
            511 => bucket.version = 9,
            _ => {}
        });

        let mut state = make_state(allocator);
        bch2_bucket_gens_init(&mut state.engine, &state.allocator).unwrap();

        let first = state
            .engine
            .get_entry_raw(BtreeId::BucketGens, Bpos::new(0, 0, 0))
            .expect("chunk 0 should exist");
        let second = state
            .engine
            .get_entry_raw(BtreeId::BucketGens, Bpos::new(0, 1, 0))
            .expect("chunk 1 should exist");

        let first_gens: BchBucketGens = bincode::deserialize(&first.value.to_bytes()).unwrap();
        let second_gens: BchBucketGens = bincode::deserialize(&second.value.to_bytes()).unwrap();

        assert_eq!(first_gens.gens[0], 1);
        assert_eq!(first_gens.gens[255], 7);
        assert_eq!(second_gens.gens[0], 3);
        assert_eq!(second_gens.gens[255], 9);
    }

    #[tokio::test]
    async fn test_bucket_gens_init_is_idempotent() {
        let allocator = BchAllocator::new(512, 512, 0);
        let mut state = make_state(allocator);

        run(&mut state).await.unwrap();
        let first_count = state.engine.get(BtreeId::BucketGens).key_count();

        run(&mut state).await.unwrap();
        let second_count = state.engine.get(BtreeId::BucketGens).key_count();

        assert_eq!(first_count, second_count);
    }
}
