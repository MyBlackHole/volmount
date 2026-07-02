//! Volume metadata — 存储在 superblock 中的卷元数据

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::{BackendType, VolumeId};

/// Volume 元数据，存储在 superblock 中
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeMeta {
    // === 不可变（创建时设定）===
    pub magic: [u8; 8], // b"VOLMv1\0"
    pub vol_name: String,
    pub vol_id: VolumeId,
    pub pool_name: String,
    pub block_size: u32, // 默认 4096
    pub capacity: u64,   // 字节
    pub backend_type: BackendType,

    // === 可变（运行时更新）===
    pub created_at: String, // unix epoch seconds; 格式: "1719500000"
    pub last_mount_at: Option<String>,
}

impl VolumeMeta {
    /// 创建新 volume 元数据
    pub fn new(
        vol_name: String,
        vol_id: VolumeId,
        pool_name: String,
        block_size: u32,
        capacity: u64,
        backend_type: BackendType,
    ) -> Self {
        let epoch_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        Self {
            magic: *b"VOLMv1\0\0",
            vol_name,
            vol_id,
            pool_name,
            block_size,
            capacity,
            backend_type,
            created_at: epoch_secs,
            last_mount_at: None,
        }
    }
}
