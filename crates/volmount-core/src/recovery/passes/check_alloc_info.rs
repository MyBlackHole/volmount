use crate::alloc::btree::serialize_alloc_entry;
use crate::alloc::{BchAllocEntry, BchAllocator, BchDataType};
use crate::btree::gc::bch2_check_alloc_info;
use crate::btree::{Bpos, BtreeEngine, BtreeEntry, BtreeId, KeyType};
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: alloc-info 一致性检查（对应 bcachefs `bch2_check_alloc_info()`）
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let discrepancies = bch2_check_alloc_info(&state.engine, &state.allocator)?;
    if discrepancies.is_empty() {
        return Ok(());
    }

    Err(StorageError::InvalidData(format!(
        "alloc-info inconsistencies: {}",
        discrepancies.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::journal::Journal;
    use crate::meta::VolumeMeta;
    use crate::storage::superblock::BchSb;
    use crate::types::BackendType;
    use std::sync::Arc;

    fn seed_alloc_state(
        engine: &mut BtreeEngine,
        allocator: &BchAllocator,
        skip_free: Option<u64>,
    ) {
        allocator.for_each_bucket(|bucket_idx, bucket| {
            let alloc_entry = BchAllocEntry {
                journal_seq: bucket.journal_seq,
                dirty_sectors: bucket.dirty_sectors,
                cached_sectors: bucket.cached_sectors,
                stripe: bucket.stripe as u16,
                state: bucket.state,
                version: bucket.version,
                io_time_read: 0,
                nr_external_backpointers: 0,
                group: bucket.group,
            };
            let alloc_pos = Bpos::new(0, bucket_idx, 0);
            let alloc_value = serialize_alloc_entry(&alloc_entry).unwrap();
            engine.insert_entry_raw(
                BtreeId::Alloc,
                BtreeEntry::raw(alloc_pos, KeyType::Normal, alloc_value),
                0,
            );

            if bucket.state == BchDataType::Free && skip_free != Some(bucket_idx) {
                let freespace_pos = Bpos::new(0, bucket_idx, bucket.version);
                engine.insert_entry_raw(
                    BtreeId::Freespace,
                    BtreeEntry::raw(freespace_pos, KeyType::Normal, vec![]),
                    0,
                );
            }
        });
    }

    fn make_state(allocator: BchAllocator, engine: BtreeEngine) -> RecoveryState {
        RecoveryState::new(
            engine,
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

    #[tokio::test]
    async fn test_check_alloc_info_passes_for_consistent_state() {
        let allocator = BchAllocator::new(512, 512, 0);
        allocator.for_each_bucket_mut(|bucket_idx, bucket| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.version = 3;
            }
        });

        let mut engine = BtreeEngine::new();
        seed_alloc_state(&mut engine, &allocator, None);

        let mut state = make_state(allocator, engine);
        run(&mut state).await.unwrap();
    }

    #[tokio::test]
    async fn test_check_alloc_info_fails_when_free_entry_missing() {
        let allocator = BchAllocator::new(512, 512, 0);
        allocator.for_each_bucket_mut(|bucket_idx, bucket| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.version = 3;
            }
        });

        let mut engine = BtreeEngine::new();
        seed_alloc_state(&mut engine, &allocator, Some(0));

        let mut state = make_state(allocator, engine);
        let err = run(&mut state).await.unwrap_err();
        match err {
            StorageError::InvalidData(msg) => {
                assert!(msg.contains("missing freespace entry"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_check_alloc_info_fails_on_stale_freespace_entry() {
        let allocator = BchAllocator::new(512, 512, 0);
        allocator.for_each_bucket_mut(|bucket_idx, bucket| {
            if bucket_idx == 1 {
                bucket.state = BchDataType::User;
                bucket.version = 3;
            }
        });

        let mut engine = BtreeEngine::new();
        seed_alloc_state(&mut engine, &allocator, None);
        engine.insert_entry_raw(
            BtreeId::Freespace,
            BtreeEntry::raw(Bpos::new(0, 1, 2), KeyType::Normal, vec![]),
            0,
        );

        let mut state = make_state(allocator, engine);
        let err = run(&mut state).await.unwrap_err();
        match err {
            StorageError::InvalidData(msg) => {
                assert!(msg.contains("stale freespace entry"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
