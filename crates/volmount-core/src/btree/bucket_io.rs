//! Multi-block Btree node extent I/O.
//!
//! bcachefs allocates a stable extent for a node, writes `struct btree_node`
//! at offset zero, then appends block-aligned `struct btree_node_entry` records
//! (`fs/btree/write.c:440-527, 584-622`).  This module preserves that boundary:
//! normal writeback changes `sectors_written`, not the physical start address.

use crate::block_device::BlockDevice;
use crate::btree::node::{
    BtreeNode, BtreeNodeHeader, BLOCK_SIZE, BTREE_NODE_MAGIC, BTREE_NODE_VERSION,
    SECTORS_PER_BLOCK, SECTOR_SIZE,
};
use crate::btree::types::BtreePtrV2;
use crate::types::{BlockAddr, StorageError};

/// Read a contiguous range of physical blocks.
pub async fn read_blocks(
    backend: &dyn BlockDevice,
    start_block: u64,
    block_count: usize,
) -> Result<Vec<u8>, StorageError> {
    let mut data = vec![0u8; block_count * BLOCK_SIZE];
    for (index, block) in data.chunks_exact_mut(BLOCK_SIZE).enumerate() {
        backend
            .read_block(BlockAddr::new(start_block + index as u64), block)
            .await?;
    }
    Ok(data)
}

/// Write one block-aligned node record at a sector offset in a stable extent.
pub async fn write_node_record(
    backend: &dyn BlockDevice,
    extent_start: u64,
    sector_offset: u16,
    record: &[u8],
) -> Result<(), StorageError> {
    if sector_offset % SECTORS_PER_BLOCK != 0 || record.len() % BLOCK_SIZE != 0 {
        return Err(StorageError::InvalidData(
            "btree node record I/O must be block aligned".into(),
        ));
    }
    let start_block = extent_start + u64::from(sector_offset / SECTORS_PER_BLOCK);
    for (index, block) in record.chunks_exact(BLOCK_SIZE).enumerate() {
        backend
            .write_block(BlockAddr::new(start_block + index as u64), block)
            .await?;
    }
    Ok(())
}

/// Read exactly the range committed by `ptr.sectors_written` and decode it.
///
/// Checksum validation happens as part of `deserialize_from_extent()` so the
/// load boundary rejects corrupted raw records before the node reaches
/// `read_done`.
pub async fn load_btree_node_with_ptr(
    backend: &dyn BlockDevice,
    ptr: BtreePtrV2,
) -> Result<(BtreeNode, BtreePtrV2), StorageError> {
    if ptr.sectors_written == 0 || ptr.sectors_written % SECTORS_PER_BLOCK != 0 {
        return Err(StorageError::InvalidData(
            "invalid btree pointer sectors_written".into(),
        ));
    }
    let block_count = usize::from(ptr.sectors_written / SECTORS_PER_BLOCK);
    let data = read_blocks(backend, ptr.block_addr, block_count).await?;
    let node = BtreeNode::deserialize_from_extent(&data, ptr)?;
    Ok((node, ptr))
}

pub async fn load_btree_node_from_ptr(
    backend: &dyn BlockDevice,
    ptr: BtreePtrV2,
) -> Result<BtreeNode, StorageError> {
    load_btree_node_with_ptr(backend, ptr)
        .await
        .map(|(node, _)| node)
}

/// Compatibility loader for call sites that only have an address.
///
/// It reads the initial header to construct a one-record pointer.  Recursive
/// recovery must use `load_btree_node_from_ptr()` so append records are bounded
/// by the parent/root pointer rather than guessed from disk contents.
pub async fn load_btree_node_with_addr(
    backend: &dyn BlockDevice,
    bucket_addr: u64,
) -> Result<(BtreeNode, BtreePtrV2), StorageError> {
    let first = read_blocks(backend, bucket_addr, 1).await?;
    if first.len() < std::mem::size_of::<BtreeNodeHeader>() {
        return Err(StorageError::InvalidData(
            "btree initial block is too short".into(),
        ));
    }
    let header: BtreeNodeHeader =
        unsafe { std::ptr::read_unaligned(first.as_ptr().cast::<BtreeNodeHeader>()) };
    let magic = { header.magic };
    let version = { header.version };
    let record_bytes = { header.record_bytes as usize };
    if magic != BTREE_NODE_MAGIC || version != BTREE_NODE_VERSION {
        return Err(StorageError::InvalidData(
            "invalid btree initial record header".into(),
        ));
    }
    if record_bytes == 0 || record_bytes % BLOCK_SIZE != 0 {
        return Err(StorageError::InvalidData(
            "invalid btree initial record length".into(),
        ));
    }

    let data = if record_bytes == BLOCK_SIZE {
        first
    } else {
        read_blocks(backend, bucket_addr, record_bytes / BLOCK_SIZE).await?
    };
    let ptr = BtreePtrV2 {
        block_addr: bucket_addr,
        sectors_written: (record_bytes / SECTOR_SIZE) as u16,
        level: header.level,
        generation: header.generation,
    };
    let node = BtreeNode::deserialize_from_extent(&data, ptr)?;
    Ok((node, ptr))
}

pub async fn load_btree_node(
    backend: &dyn BlockDevice,
    bucket_addr: u64,
) -> Result<BtreeNode, StorageError> {
    load_btree_node_with_addr(backend, bucket_addr)
        .await
        .map(|(node, _)| node)
}

/// Initial-write a node and return the committed physical pointer.
pub async fn write_initial_node(
    node: &BtreeNode,
    bucket_addr: u64,
    generation: u32,
    backend: &dyn BlockDevice,
) -> Result<BtreePtrV2, StorageError> {
    let record = node.serialize_initial_record(bucket_addr, generation)?;
    write_node_record(backend, bucket_addr, 0, &record).await?;
    Ok(BtreePtrV2 {
        block_addr: bucket_addr,
        sectors_written: (record.len() / SECTOR_SIZE) as u16,
        level: node.level,
        generation,
    })
}

/// Existing low-level API retained while callers migrate to full pointers.
pub async fn write_node_to_bucket(
    node: &BtreeNode,
    bucket_addr: u64,
    backend: &dyn BlockDevice,
) -> Result<(), StorageError> {
    write_initial_node(node, bucket_addr, 1, backend)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::{BchVal, BtreeKey, KeyType};
    use crate::btree::types::BtreePtrV2;
    use crate::types::StorageError;

    fn build_filled_node(count: u32) -> BtreeNode {
        let mut node = BtreeNode::new_leaf();
        for i in 0..count {
            node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64, i as u16),
            );
        }
        node
    }

    #[tokio::test]
    async fn test_bucket_io_roundtrip() {
        let backend = MockBlockDevice::new();
        let mut node = build_filled_node(20);
        node.compact();

        let ptr = write_initial_node(&node, 100, 7, &backend).await.unwrap();
        let loaded = load_btree_node_from_ptr(&backend, ptr).await.unwrap();

        assert_eq!(loaded.key_count, node.key_count);
        assert_eq!(loaded.level, node.level);
        for i in 0..20 {
            let key = BtreeKey::new(i, 1, KeyType::Normal);
            assert!(loaded.search(&key).is_some(), "key {} lost in bucket io", i);
        }
    }

    #[tokio::test]
    async fn test_load_btree_node_rejects_corrupt_initial_record() {
        let backend = MockBlockDevice::new();
        let mut node = build_filled_node(20);
        node.compact();

        let mut record = node.serialize_initial_record(100, 7).unwrap();
        let record_mid = record.len() / 2;
        record[record_mid] ^= 0x80;
        write_node_record(&backend, 100, 0, &record).await.unwrap();

        let ptr = BtreePtrV2 {
            block_addr: 100,
            sectors_written: (record.len() / SECTOR_SIZE) as u16,
            level: node.level,
            generation: 7,
        };
        let result = load_btree_node_from_ptr(&backend, ptr).await;
        assert!(matches!(result, Err(StorageError::ChecksumMismatch { .. })));
    }

    #[tokio::test]
    async fn test_multi_block_initial_record_roundtrip() {
        let backend = MockBlockDevice::new();
        let mut node = build_filled_node(180);
        node.compact();

        let ptr = write_initial_node(&node, 500, 3, &backend).await.unwrap();
        assert!(ptr.sectors_written > SECTORS_PER_BLOCK);
        let loaded = load_btree_node_from_ptr(&backend, ptr).await.unwrap();
        assert_eq!(loaded.key_count, node.key_count);
        assert!(loaded
            .search(&BtreeKey::new(179, 1, KeyType::Normal))
            .is_some());
    }

    #[tokio::test]
    async fn test_load_btree_node_rejects_corrupt_append_record() {
        let backend = MockBlockDevice::new();
        let mut node = build_filled_node(20);
        node.compact();
        let initial = write_initial_node(&node, 900, 4, &backend).await.unwrap();

        node.insert(
            BtreeKey::new(1000, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1),
        );
        let mut append = node
            .serialize_append_record(4, initial.sectors_written)
            .unwrap();
        let append_mid = append.len() / 2;
        append[append_mid] ^= 0x20;
        write_node_record(
            &backend,
            initial.block_addr,
            initial.sectors_written,
            &append,
        )
        .await
        .unwrap();

        let ptr = BtreePtrV2 {
            sectors_written: initial.sectors_written + (append.len() / SECTOR_SIZE) as u16,
            ..initial
        };
        let result = load_btree_node_from_ptr(&backend, ptr).await;
        assert!(matches!(result, Err(StorageError::ChecksumMismatch { .. })));
    }

    #[tokio::test]
    async fn test_append_record_respects_pointer_boundary() {
        let backend = MockBlockDevice::new();
        let mut node = build_filled_node(20);
        node.compact();
        let initial_ptr = write_initial_node(&node, 900, 4, &backend).await.unwrap();

        node.insert(
            BtreeKey::new(1000, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1),
        );
        let append = node
            .serialize_append_record(4, initial_ptr.sectors_written)
            .unwrap();
        write_node_record(
            &backend,
            initial_ptr.block_addr,
            initial_ptr.sectors_written,
            &append,
        )
        .await
        .unwrap();

        let before_commit = load_btree_node_from_ptr(&backend, initial_ptr)
            .await
            .unwrap();
        assert!(before_commit
            .search(&BtreeKey::new(1000, 1, KeyType::Normal))
            .is_none());

        let committed_ptr = BtreePtrV2 {
            sectors_written: initial_ptr.sectors_written + (append.len() / SECTOR_SIZE) as u16,
            ..initial_ptr
        };
        let after_commit = load_btree_node_from_ptr(&backend, committed_ptr)
            .await
            .unwrap();
        assert!(after_commit
            .search(&BtreeKey::new(1000, 1, KeyType::Normal))
            .is_some());
    }
}
