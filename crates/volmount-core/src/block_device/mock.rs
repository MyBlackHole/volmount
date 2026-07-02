use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use super::{BlockDevice, Result};
use crate::types::{BlockAddr, HealthStatus, StorageError};

/// 内存 Mock 后端 — 用于单元测试
#[derive(Debug, Clone)]
pub struct MockBlockDevice {
    blocks: Arc<RwLock<HashMap<BlockAddr, Vec<u8>>>>,
}

impl MockBlockDevice {
    pub fn new() -> Self {
        Self {
            blocks: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MockBlockDevice {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BlockDevice for MockBlockDevice {
    async fn read_block(&self, addr: BlockAddr, buf: &mut [u8]) -> Result<()> {
        let map = self.blocks.read();
        if let Some(data) = map.get(&addr) {
            let len = data.len().min(buf.len());
            buf[..len].copy_from_slice(&data[..len]);
        } else {
            // 未写入的块返回零填充，与 FileBlockDevice 行为一致
            buf.fill(0);
        }
        Ok(())
    }

    async fn write_block(&self, addr: BlockAddr, data: &[u8]) -> Result<()> {
        let mut map = self.blocks.write();
        map.insert(addr, data.to_vec());
        Ok(())
    }

    async fn delete_block(&self, addr: BlockAddr) -> Result<()> {
        let mut map = self.blocks.write();
        map.remove(&addr);
        Ok(())
    }

    async fn trim_block(&self, addr: BlockAddr) -> Result<()> {
        self.delete_block(addr).await
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> Result<HealthStatus> {
        Ok(HealthStatus::Healthy)
    }

    async fn used_space(&self) -> Result<u64> {
        let map = self.blocks.read();
        Ok(map.values().map(|v| v.len() as u64).sum())
    }
}
