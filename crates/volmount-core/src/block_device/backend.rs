use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::{debug, error};

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 块设备后端配置
#[derive(Debug, Clone)]
pub struct SparseBackendConfig {
    pub base_path: PathBuf,
    pub vol_name: String,
    pub file_name: String,
    pub block_size: u64,
    pub capacity_bytes: Option<u64>,
}

/// 块设备后端 — 底层由稀疏文件实现
///
/// 文件结构：`{base_path}/{vol_name}/{file_name}`
/// 语义：文件本身就是虚拟块设备；块号 `1..8` 对应文件内第 `1..8` 个块
/// 偏移计算：`offset = addr.raw * block_size`
/// TRIM：通过 fallocate FALLOC_FL_PUNCH_HOLE 打洞
#[derive(Debug)]
pub struct SparseBackendBlockDevice {
    device_path: PathBuf,
    block_size: u64,
    file: Arc<Mutex<File>>,
}

impl SparseBackendBlockDevice {
    /// 创建新的块设备后端
    pub async fn new(config: SparseBackendConfig) -> Result<Self> {
        let device_path = config
            .base_path
            .join(&config.vol_name)
            .join(&config.file_name);

        // 确保目录存在
        let dir = device_path.parent().unwrap();
        fs::create_dir_all(dir).await.map_err(|e| {
            error!(
                "block device backend: failed to create directory {:?}: {e}",
                dir
            );
            StorageError::Io(e)
        })?;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&device_path)
            .await
            .map_err(StorageError::Io)?;

        if let Some(capacity_bytes) = config.capacity_bytes {
            if file.metadata().await.map_err(StorageError::Io)?.len() == 0 {
                file.set_len(capacity_bytes)
                    .await
                    .map_err(StorageError::Io)?;
            }
        }

        Ok(Self {
            device_path,
            block_size: config.block_size,
            file: Arc::new(Mutex::new(file)),
        })
    }

    /// 计算文件偏移
    fn offset(&self, addr: BlockAddr) -> u64 {
        addr.raw * self.block_size
    }
}

#[async_trait]
impl BlockDevice for SparseBackendBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "block device backend read: path={:?}, offset={}",
            self.device_path, offset
        );

        let mut file = self.file.lock().await;

        // 检查文件大小是否足够
        let metadata = file.metadata().await.map_err(StorageError::Io)?;
        if offset >= metadata.len() {
            // 稀疏文件空洞 → 返回零
            buf.fill(0);
            return Ok(());
        }

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let n = file.read(buf).await?;
        if n < buf.len() {
            // 部分空洞 → 剩余部分填充零
            buf[n..].fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "block device backend write: path={:?}, offset={}, size={}",
            self.device_path,
            offset,
            data.len()
        );

        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        file.flush().await?;

        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        // delete = trim（打洞释放空间）
        self.trim_block(addr).await
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        let offset = self.offset(addr);
        debug!(
            "block device backend trim: path={:?}, offset={}",
            self.device_path, offset
        );

        #[cfg(target_os = "linux")]
        {
            let file = self.file.lock().await;
            let fd = file.as_raw_fd();

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
                    error!("block device backend trim: fallocate failed: {err}");
                    return Err(StorageError::Io(err));
                }
                debug!("block device backend trim: fallocate not supported, ignoring");
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            // 非 Linux 平台不支持 PUNCH_HOLE，写入零代替
            let data = vec![0u8; self.block_size as usize];
            let mut file = self.file.lock().await;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.write_all(&data).await?;
            file.flush().await?;
        }

        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        let file = self.file.lock().await;
        file.sync_all().await?;
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        let file = self.file.lock().await;
        match file.metadata().await {
            Ok(_) => Ok(HealthStatus::Healthy),
            Err(e) => {
                error!("block device backend health check failed: {e}");
                Ok(HealthStatus::Unreachable {
                    reason: format!("{:?}: {e}", self.device_path),
                })
            }
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

        // fallback to virtual size
        let meta = std::fs::metadata(&self.device_path).map_err(StorageError::Io)?;
        Ok(meta.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const BLOCK_SIZE: u64 = 4096;

    fn test_addr(raw: u64) -> BlockAddr {
        BlockAddr::new(raw)
    }

    async fn create_test_backend() -> (SparseBackendBlockDevice, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = SparseBackendConfig {
            base_path: dir.path().to_path_buf(),
            vol_name: "test".into(),
            file_name: "device".into(),
            block_size: BLOCK_SIZE,
            capacity_bytes: Some(1024 * 1024),
        };
        let backend = SparseBackendBlockDevice::new(config).await.unwrap();
        (backend, dir)
    }

    #[tokio::test]
    async fn test_sparse_backend_create_open() {
        let (backend, dir) = create_test_backend().await;
        let expected_path = dir.path().join("test").join("device");
        assert!(
            expected_path.exists(),
            "backend file should exist after creation"
        );
        // Drop backend should still leave file on disk
        drop(backend);
        assert!(
            expected_path.exists(),
            "backend file should persist after drop"
        );
    }

    #[tokio::test]
    async fn test_sparse_backend_write_read() {
        let (backend, _dir) = create_test_backend().await;
        let addr = test_addr(42);
        let data = b"hello volmount block device backend";

        backend.write_block(addr, data).await.unwrap();
        let mut buf = vec![0u8; data.len()];
        backend.read_block(addr, &mut buf).await.unwrap();
        assert_eq!(&buf, data);
    }

    #[tokio::test]
    async fn test_sparse_backend_trim() {
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
    async fn test_sparse_backend_used_space() {
        let (backend, _dir) = create_test_backend().await;
        let used = backend.used_space().await.unwrap();
        // Just created file should have some minimal space (inode, metadata)
        // The exact value depends on filesystem, but it should not be absurd
        assert!(used <= 1024 * 1024, "used_space should be reasonable");
    }
}
