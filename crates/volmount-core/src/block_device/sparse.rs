use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// Simplified sparse file block device for testing.
///
/// Unlike FileBlockDevice, this uses tokio async I/O and does NOT
/// support fallocate for trim — trim is implemented as zero-fill.
#[derive(Debug)]
pub struct SparseFileBlockDevice {
    path: PathBuf,
    file: Arc<tokio::sync::Mutex<tokio::fs::File>>,
    block_size: u64,
    capacity_blocks: u64,
}

impl SparseFileBlockDevice {
    pub async fn create(
        path: impl AsRef<Path>,
        capacity_blocks: u64,
        block_size: u64,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await.map_err(StorageError::Io)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .await
            .map_err(StorageError::Io)?;
        let total_size = capacity_blocks * block_size;
        file.set_len(total_size).await.map_err(StorageError::Io)?;
        Ok(Self {
            path,
            file: Arc::new(tokio::sync::Mutex::new(file)),
            block_size,
            capacity_blocks,
        })
    }

    pub async fn open(path: impl AsRef<Path>, block_size: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .map_err(StorageError::Io)?;
        let metadata = file.metadata().await.map_err(StorageError::Io)?;
        let file_size = metadata.len();
        if file_size % block_size != 0 {
            return Err(StorageError::InvalidBlockSize(file_size));
        }
        let capacity_blocks = file_size / block_size;
        Ok(Self {
            path,
            file: Arc::new(tokio::sync::Mutex::new(file)),
            block_size,
            capacity_blocks,
        })
    }

    fn offset(&self, addr: BlockAddr) -> u64 {
        addr.raw * self.block_size
    }

    pub fn capacity_blocks(&self) -> u64 {
        self.capacity_blocks
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl BlockDevice for SparseFileBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let offset = self.offset(addr);
        let mut file = self.file.lock().await;
        let metadata = file.metadata().await.map_err(StorageError::Io)?;
        if offset >= metadata.len() {
            buf.fill(0);
            return Ok(());
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let n = file.read(buf).await?;
        if n < buf.len() {
            buf[n..].fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let offset = self.offset(addr);
        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        self.trim_block(addr).await
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        let offset = self.offset(addr);
        let zeros = vec![0u8; self.block_size as usize];
        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(&zeros).await?;
        file.flush().await?;
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        let file = self.file.lock().await;
        file.sync_all().await?;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        match self.file.lock().await.metadata().await {
            Ok(_) => Ok(HealthStatus::Healthy),
            Err(e) => Ok(HealthStatus::Unreachable {
                reason: format!("{:?}: {e}", self.path),
            }),
        }
    }

    async fn used_space(&self) -> Result<u64> {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::fs::MetadataExt;
            let file = self.file.lock().await;
            let meta = file.metadata().await.map_err(StorageError::Io)?;
            let blocks = meta.st_blocks();
            if blocks > 0 {
                return Ok(blocks * 512);
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            let file = self.file.lock().await;
            let meta = file.metadata().await.map_err(StorageError::Io)?;
            let blocks = meta.st_blocks();
            if blocks > 0 {
                return Ok(blocks * 512);
            }
        }
        let meta = std::fs::metadata(&self.path).map_err(StorageError::Io)?;
        Ok(meta.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BLOCK_SIZE: u64 = 4096;
    const CAPACITY_BLOCKS: u64 = 1024;

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    async fn create_test_backend() -> (SparseFileBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let backend = SparseFileBlockDevice::create(&path, CAPACITY_BLOCKS, BLOCK_SIZE)
            .await
            .unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_create_open() {
        let (backend, _dir) = create_test_backend().await;
        assert_eq!(backend.capacity_blocks(), CAPACITY_BLOCKS);
        assert_eq!(backend.block_size(), BLOCK_SIZE);
        let path = backend.path().to_path_buf();
        drop(backend);
        let reopened = SparseFileBlockDevice::open(&path, BLOCK_SIZE)
            .await
            .unwrap();
        assert_eq!(reopened.capacity_blocks(), CAPACITY_BLOCKS);
    }

    #[tokio::test]
    async fn test_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(10);
        let data = b"hello sparse file";
        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_read_unwritten_zeros() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(500);
        let mut buf = vec![0xFFu8; 32];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 32]);
    }

    #[tokio::test]
    async fn test_trim() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(7);
        backend.write_block(addr, b"trim target").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"trim target");
        backend.trim_block(addr).await.unwrap();
        buf.fill(0xFF);
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 11]);
    }

    #[tokio::test]
    async fn test_trim_then_write() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(20);
        backend.write_block(addr, b"original").await.unwrap();
        backend.trim_block(addr).await.unwrap();
        backend.write_block(addr, b"rewritten").await.unwrap();
        let mut buf = vec![0u8; 9];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"rewritten");
    }

    #[tokio::test]
    async fn test_used_space() {
        let (backend, _dir) = create_test_backend().await;
        let used = backend.used_space().await.unwrap();
        assert!(used <= CAPACITY_BLOCKS * BLOCK_SIZE);
    }
}
