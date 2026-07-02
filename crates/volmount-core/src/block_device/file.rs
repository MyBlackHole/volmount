//! Sparse file block device backend for btree/journal persistent storage.

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::debug;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

#[derive(Debug)]
pub struct FileBlockDevice {
    path: PathBuf,
    file: std::fs::File,
    block_size: u64,
    capacity_blocks: u64,
}

impl FileBlockDevice {
    /// Create a new sparse file block device.
    ///
    /// Pre-allocates `capacity_blocks * block_size` bytes via `set_len` (sparse,
    /// no physical space consumed immediately).
    pub async fn create(
        path: impl AsRef<Path>,
        capacity_blocks: u64,
        block_size: u64,
    ) -> Result<Self> {
        let path = path.as_ref();
        debug!(
            "file create: path={:?}, capacity_blocks={}, block_size={}",
            path, capacity_blocks, block_size
        );

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(StorageError::Io)?;
            }
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(StorageError::Io)?;

        let total_size = capacity_blocks * block_size;
        file.set_len(total_size).map_err(StorageError::Io)?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            block_size,
            capacity_blocks,
        })
    }

    /// Open an existing sparse file block device.
    ///
    /// Derives capacity from file size: `capacity_blocks = file_size / block_size`.
    pub async fn open(path: impl AsRef<Path>, block_size: u64) -> Result<Self> {
        let path = path.as_ref();
        debug!("file open: path={:?}, block_size={}", path, block_size);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(StorageError::Io)?;

        let metadata = file.metadata().map_err(StorageError::Io)?;
        let file_size = metadata.len();

        if file_size % block_size != 0 {
            return Err(StorageError::InvalidBlockSize(file_size));
        }

        let capacity_blocks = file_size / block_size;

        Ok(Self {
            path: path.to_path_buf(),
            file,
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
impl BlockDevice for FileBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "file read: path={:?}, addr={:?}, offset={}",
            self.path, addr, offset
        );

        let n = self.file.read_at(buf, offset).map_err(StorageError::Io)?;
        if n < buf.len() {
            buf[n..].fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "file write: path={:?}, addr={:?}, offset={}, size={}",
            self.path,
            addr,
            offset,
            data.len()
        );

        self.file
            .pwrite_all(data, offset)
            .map_err(StorageError::Io)?;
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        self.trim_block(addr).await
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "file trim: path={:?}, addr={:?}, offset={}",
            self.path, addr, offset
        );

        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = self.file.as_raw_fd();
            let rc = unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as i64,
                    self.block_size as i64,
                )
            };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ENOSYS)
                    && err.raw_os_error() != Some(libc::EOPNOTSUPP)
                {
                    debug!("file trim: fallocate not supported, fall back to zero-fill");
                    let zeros = vec![0u8; self.block_size as usize];
                    self.file
                        .pwrite_all(&zeros, offset)
                        .map_err(StorageError::Io)?;
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            let zeros = vec![0u8; self.block_size as usize];
            self.file
                .pwrite_all(&zeros, offset)
                .map_err(StorageError::Io)?;
        }

        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        debug!("file flush: path={:?}", self.path);
        self.file.sync_all().map_err(StorageError::Io)?;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        match self.file.metadata() {
            Ok(_) => Ok(HealthStatus::Healthy),
            Err(e) => {
                debug!("file health check failed: path={:?}, err={}", self.path, e);
                Ok(HealthStatus::Unreachable {
                    reason: format!("{:?}: {e}", self.path),
                })
            }
        }
    }

    async fn used_space(&self) -> Result<u64> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::linux::fs::MetadataExt;
            let meta = self.file.metadata().map_err(StorageError::Io)?;
            let blocks = meta.st_blocks();
            if blocks > 0 {
                return Ok(blocks * 512);
            }
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            let meta = self.file.metadata().map_err(StorageError::Io)?;
            let blocks = meta.st_blocks();
            if blocks > 0 {
                return Ok(blocks * 512);
            }
        }

        let meta = self.file.metadata().map_err(StorageError::Io)?;
        Ok(meta.len())
    }
}

/// Extension trait to guarantee full writes via `write_at`.
///
/// `FileExt::write_at` may perform short writes; this retry-loop helper
/// ensures all bytes are written.
trait WriteAllAtExt {
    fn pwrite_all(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
}

impl WriteAllAtExt for std::fs::File {
    fn pwrite_all(&self, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            let n = self.write_at(buf, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "write_at returned 0",
                ));
            }
            buf = &buf[n..];
            offset += n as u64;
        }
        Ok(())
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

    async fn create_test_backend() -> (FileBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.bin");
        let backend = FileBlockDevice::create(&path, CAPACITY_BLOCKS, BLOCK_SIZE)
            .await
            .unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_file_backend_create_open() {
        let (backend, _dir) = create_test_backend().await;
        assert_eq!(backend.capacity_blocks(), CAPACITY_BLOCKS);
        assert_eq!(backend.block_size(), BLOCK_SIZE);

        let path = backend.path().to_path_buf();
        drop(backend);

        let reopened = FileBlockDevice::open(&path, BLOCK_SIZE).await.unwrap();
        assert_eq!(reopened.capacity_blocks(), CAPACITY_BLOCKS);
        assert_eq!(reopened.block_size(), BLOCK_SIZE);
    }

    #[tokio::test]
    async fn test_file_backend_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(42);
        let data = b"hello volmount file backend";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_file_backend_read_unwritten_returns_zeros() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(999);
        let mut buf = vec![0xFFu8; 16];

        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; 16]);
    }

    #[tokio::test]
    async fn test_file_backend_trim() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(2);
        let data = b"trim me";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);

        backend.trim_block(addr).await.unwrap();
        buf.fill(0xFF);
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; data.len()]);
    }

    #[tokio::test]
    async fn test_file_backend_delete() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(1);
        let data = b"delete me";

        backend.write_block(addr, data).await.unwrap();
        backend.delete_block(addr).await.unwrap();

        let mut buf = vec![0xFFu8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(buf, vec![0u8; data.len()]);
    }

    #[tokio::test]
    async fn test_file_backend_flush() {
        let (backend, _dir) = create_test_backend().await;
        backend.flush().await.unwrap();
    }

    #[tokio::test]
    async fn test_file_backend_health() {
        let (backend, _dir) = create_test_backend().await;
        let health = backend.health_check().await.unwrap();
        assert_eq!(health, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_file_backend_used_space() {
        let (_backend, _dir) = create_test_backend().await;
        // Just verify that used_space returns a reasonable value.
        // The exact value depends on filesystem sparse file support.
        let used = _backend.used_space().await.unwrap();
        assert!(used <= CAPACITY_BLOCKS * BLOCK_SIZE, "used > capacity");
    }

    #[tokio::test]
    async fn test_file_backend_close_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("reopen.bin");

        let data = b"persistent data across close/reopen";
        let addr = test_addr(77);

        {
            let backend = FileBlockDevice::create(&path, CAPACITY_BLOCKS, BLOCK_SIZE)
                .await
                .unwrap();
            backend.write_block(addr, data).await.unwrap();
            backend.flush().await.unwrap();
        }

        {
            let reopened = FileBlockDevice::open(&path, BLOCK_SIZE).await.unwrap();
            assert_eq!(reopened.capacity_blocks(), CAPACITY_BLOCKS);
            let mut buf = vec![0u8; data.len()];
            reopened.read_block(addr, &mut buf).await.unwrap();
            assert_eq!(&buf, data);
        }
    }

    #[tokio::test]
    async fn test_file_backend_multiple_blocks() {
        let (backend, _dir) = create_test_backend().await;

        let addrs: Vec<_> = (0..5).map(|i| test_addr(i * 2)).collect();
        for (i, addr) in addrs.iter().enumerate() {
            backend
                .write_block(*addr, format!("block-{i}").as_bytes())
                .await
                .unwrap();
        }

        for (i, addr) in addrs.iter().enumerate() {
            let expected = format!("block-{i}");
            let mut buf = vec![0u8; expected.len()];
            backend.read_block(*addr, &mut buf).await.unwrap();
            assert_eq!(&buf, expected.as_bytes());
        }
    }

    #[tokio::test]
    async fn test_file_backend_overwrite() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(100);

        backend.write_block(addr, b"first write").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"first write");

        backend.write_block(addr, b"second").await.unwrap();
        let mut buf = vec![0u8; 11];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf[..6], b"second");
    }

    #[tokio::test]
    async fn test_file_backend_health_unreachable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.bin");

        let result = FileBlockDevice::open(&path, BLOCK_SIZE).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_file_backend_trim_then_write() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(50);

        backend.write_block(addr, b"original").await.unwrap();
        backend.trim_block(addr).await.unwrap();

        backend.write_block(addr, b"rewritten").await.unwrap();
        let mut buf = vec![0u8; 9];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, b"rewritten");
    }
}
