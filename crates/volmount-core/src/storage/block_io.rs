//! 后端块 I/O 工具函数
//!
//! 提供在 BlockDevice 上分配/写入/读取序列化数据的通用函数。
//! 这些函数用于把变长序列化 payload 写入后端块设备，并在读取时恢复原始字节。

use crate::alloc::{AllocRequest, BchAllocator, BchDataType, DedicatedWp, WritePointSpecifier};
use crate::block_device::BlockDevice;
use crate::btree::BtreeEngine;
use crate::storage::superblock::SUPERBLOCK_SIZE;
use crate::types::{BlockAddr, StorageError, Watermark};

/// 将序列化数据写入后端块
///
/// 1. 通过 `allocate_bucket` 分配一个 bucket（256 块连续空间）
/// 2. 第一个块前 8 字节存放 u64 数据长度，随后填充数据
/// 3. 后续块继续填充剩余数据
///
/// # 返回值
///
/// `(start_addr, blocks_used)` — 起始块地址和实际使用的块数。
pub async fn write_data_to_blocks(
    backend: &dyn BlockDevice,
    allocator: &BchAllocator,
    engine: &mut BtreeEngine,
    data: &[u8],
) -> Result<(u64, u32), StorageError> {
    let data_len = data.len();
    let header_size = 8u64; // u64 length prefix
    let total_bytes = header_size + data_len as u64;
    let blocks_needed = total_bytes.div_ceil(SUPERBLOCK_SIZE as u64);

    // 分配 bucket（256 块连续空间）
    let start_addr = allocator.bch2_bucket_alloc_new_fs(
        engine,
        &AllocRequest::new(Watermark::Normal, BchDataType::User),
        Some(WritePointSpecifier::Direct(DedicatedWp::GC)),
    )?;

    // 构建写入缓冲区：第一个块含长度前缀，后续块仅数据
    let mut first_block = vec![0u8; SUPERBLOCK_SIZE];
    first_block[..8].copy_from_slice(&(data_len as u64).to_le_bytes());
    let copy_end = (SUPERBLOCK_SIZE - 8).min(data_len);
    first_block[8..8 + copy_end].copy_from_slice(&data[..copy_end]);
    backend
        .write_block(BlockAddr::new(start_addr), &first_block)
        .await?;

    // 写入后续块
    let mut offset = copy_end;
    for i in 1..blocks_needed {
        let block_addr = start_addr + i;
        let remaining = data_len - offset;
        let chunk_size = SUPERBLOCK_SIZE.min(remaining);
        let mut block = vec![0u8; chunk_size];
        block.copy_from_slice(&data[offset..offset + chunk_size]);
        backend
            .write_block(BlockAddr::new(block_addr), &block)
            .await?;
        offset += chunk_size;
    }

    Ok((start_addr, blocks_needed as u32))
}

/// 从后端块读取序列化数据
///
/// 读取由 `write_data_to_blocks` 写入的数据。
/// `blocks == 0` 表示无数据，返回空 Vec。
pub async fn read_data_from_blocks(
    backend: &dyn BlockDevice,
    addr: u64,
    blocks: u32,
) -> Result<Vec<u8>, StorageError> {
    if blocks == 0 {
        return Ok(Vec::new());
    }

    // 读取第一个块（含长度前缀）
    let mut first_block = vec![0u8; SUPERBLOCK_SIZE];
    backend
        .read_block(BlockAddr::new(addr), &mut first_block)
        .await?;

    // 解析 u64 数据长度
    let data_len = u64::from_le_bytes(first_block[..8].try_into().unwrap()) as usize;

    // 计算实际需要读取的块数
    let total_bytes = 8u64 + data_len as u64;
    let needed_blocks = total_bytes.div_ceil(SUPERBLOCK_SIZE as u64);
    let needed_blocks = (needed_blocks as u32).min(blocks);

    if needed_blocks == 0 {
        return Ok(Vec::new());
    }

    // 分配输出缓冲区
    let mut result = Vec::with_capacity(data_len);

    // 从第一个块提取数据（跳过 8 字节头部）
    let first_data_end = (SUPERBLOCK_SIZE - 8).min(data_len);
    result.extend_from_slice(&first_block[8..8 + first_data_end]);

    // 读取后续块
    for i in 1..needed_blocks {
        let block_addr = addr + i as u64;
        let remaining = data_len - result.len();
        let chunk_size = SUPERBLOCK_SIZE.min(remaining);
        let mut block = vec![0u8; chunk_size];
        backend
            .read_block(BlockAddr::new(block_addr), &mut block)
            .await?;
        result.extend_from_slice(&block[..chunk_size]);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::{AllocRequest, BchAllocator, BchDataType, DedicatedWp, WritePointSpecifier};
    use crate::block_device::MockBlockDevice;
    use crate::btree::BtreeEngine;
    use crate::storage::superblock::SUPERBLOCK_SIZE;

    async fn setup() -> (MockBlockDevice, BchAllocator, BtreeEngine) {
        let backend = MockBlockDevice::new();
        // 分配器从 block 0 开始分配；Volume::create 会预留超块区。
        // 这里保留一个足够大的单 AG，避免 Normal watermark 的最小预留
        // 在过小的 group 上把测试配置卡死。
        let total_blocks = 65536;
        let group_size = total_blocks;
        let allocator = BchAllocator::new(
            total_blocks,
            group_size,
            crate::storage::superblock::RESERVED_BLOCKS,
        );
        let engine = BtreeEngine::new();

        (backend, allocator, engine)
    }

    #[tokio::test]
    async fn test_write_read_small_data() {
        let (backend, allocator, mut engine) = setup().await;
        let data = b"hello block storage!";

        let (addr, blocks) = write_data_to_blocks(&backend, &allocator, &mut engine, data)
            .await
            .unwrap();

        assert!(blocks >= 1);

        let read_back = read_data_from_blocks(&backend, addr, blocks).await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_write_read_large_data() {
        let (backend, allocator, mut engine) = setup().await;
        let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        let (addr, blocks) = write_data_to_blocks(&backend, &allocator, &mut engine, &data)
            .await
            .unwrap();
        let read_back = read_data_from_blocks(&backend, addr, blocks).await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_write_read_empty_data() {
        let (backend, allocator, mut engine) = setup().await;
        let data: Vec<u8> = vec![];

        let (addr, blocks) = write_data_to_blocks(&backend, &allocator, &mut engine, &data)
            .await
            .unwrap();
        let read_back = read_data_from_blocks(&backend, addr, blocks).await.unwrap();
        assert_eq!(read_back, data);
    }

    #[tokio::test]
    async fn test_read_no_checkpoint() {
        let backend = MockBlockDevice::new();
        let result = read_data_from_blocks(&backend, 0, 0).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_write_read_exactly_one_block() {
        let (backend, allocator, mut engine) = setup().await;
        // 刚好填满一个块的数据量（去掉 8 字节头部后）
        let data = vec![0xABu8; SUPERBLOCK_SIZE - 8];

        let (addr, blocks) = write_data_to_blocks(&backend, &allocator, &mut engine, &data)
            .await
            .unwrap();
        assert_eq!(blocks, 1);

        let read_back = read_data_from_blocks(&backend, addr, blocks).await.unwrap();
        assert_eq!(read_back, data);
    }
}
