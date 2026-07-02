//! 共享配置定义 — VolmountdConfig
//!
//! 供 CLI (volmount) 和 daemon (volmountd) 共同使用，
//! 消除在两个 crate 间重复定义的问题。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::CsumType;

/// volmountd 默认配置
pub const DEFAULT_HOME_DIR: &str = "~/.volmount";
pub const DEFAULT_NBD_SOCKET: &str = "run/volmountd.sock";

// ─── StorageConfig（存储引擎选项，持久化在 Superblock）───

/// 存储引擎选项 — bcachefs 对齐: `struct bch_opts` (opts.h)
///
/// 只取核心子集。所有字段有合法值域验证 + `#[serde(default)]` 向后兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_block_size")]
    pub block_size: u16,
    #[serde(default = "default_btree_node_size")]
    pub btree_node_size: u32,
    #[serde(default = "default_metadata_checksum")]
    pub metadata_checksum: CsumType,
    #[serde(default = "default_data_checksum")]
    pub data_checksum: CsumType,
    #[serde(default = "default_journal_flush_delay_ms")]
    pub journal_flush_delay_ms: u32,
    #[serde(default = "default_gc_reserve_percent")]
    pub gc_reserve_percent: u8,
    #[serde(default = "default_discard")]
    pub discard: bool,
    #[serde(default = "default_read_only")]
    pub read_only: bool,
}

// 默认值函数
fn default_block_size() -> u16 {
    4096
}
fn default_btree_node_size() -> u32 {
    256 * 1024
}
fn default_metadata_checksum() -> CsumType {
    CsumType::Crc32c
}
fn default_data_checksum() -> CsumType {
    CsumType::Crc32c
}
fn default_journal_flush_delay_ms() -> u32 {
    1000
}
fn default_gc_reserve_percent() -> u8 {
    8
}
fn default_discard() -> bool {
    true
}
fn default_read_only() -> bool {
    false
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            block_size: default_block_size(),
            btree_node_size: default_btree_node_size(),
            metadata_checksum: default_metadata_checksum(),
            data_checksum: default_data_checksum(),
            journal_flush_delay_ms: default_journal_flush_delay_ms(),
            gc_reserve_percent: default_gc_reserve_percent(),
            discard: default_discard(),
            read_only: default_read_only(),
        }
    }
}

impl StorageConfig {
    /// 验证所有字段的合法性
    pub fn validate(&self) -> Result<(), ConfigError> {
        // block_size: 512..=65535, 2 的幂, 512 对齐
        if !(512..=65535).contains(&self.block_size) {
            return Err(ConfigError::OutOfRange(
                "block_size",
                512,
                65535,
                self.block_size as u64,
            ));
        }
        if !self.block_size.is_power_of_two() {
            return Err(ConfigError::NotPowerOf2(
                "block_size",
                self.block_size as u64,
            ));
        }
        if self.block_size % 512 != 0 {
            return Err(ConfigError::NotAligned(
                "block_size",
                512,
                self.block_size as u64,
            ));
        }

        // btree_node_size: 512..1M, 2 的幂
        if !(512..=1_048_576).contains(&self.btree_node_size) {
            return Err(ConfigError::OutOfRange(
                "btree_node_size",
                512,
                1_048_576,
                self.btree_node_size as u64,
            ));
        }
        if !self.btree_node_size.is_power_of_two() {
            return Err(ConfigError::NotPowerOf2(
                "btree_node_size",
                self.btree_node_size as u64,
            ));
        }

        // journal_flush_delay_ms: 1..u32::MAX
        if self.journal_flush_delay_ms == 0 {
            return Err(ConfigError::OutOfRange(
                "journal_flush_delay_ms",
                1,
                u32::MAX as u64,
                self.journal_flush_delay_ms as u64,
            ));
        }

        // gc_reserve_percent: 5..21
        if !(5..=21).contains(&self.gc_reserve_percent) {
            return Err(ConfigError::OutOfRange(
                "gc_reserve_percent",
                5,
                21,
                self.gc_reserve_percent as u64,
            ));
        }

        Ok(())
    }
}

// ─── VolmountdConfig（daemon 编排配置）───

/// volmountd 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolmountdConfig {
    /// 数据根目录（默认 ~/.volmount）
    #[serde(default = "default_home_dir")]
    pub home_dir: PathBuf,
    /// NBD Unix socket 路径（相对于 home_dir）
    #[serde(default = "default_nbd_socket")]
    pub nbd_socket_path: PathBuf,
    /// 自动导出的卷列表（启动时自动加载）
    #[serde(default)]
    pub auto_exports: Vec<String>,
    /// 每个卷对应的模拟块设备文件名（默认 `device`）
    #[serde(default = "default_backend_file_name")]
    pub backend_file_name: String,
    /// HTTP API 监听端口（默认 9876）
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// 存储引擎默认配置（可选 — 覆盖 StorageConfig 默认值）
    #[serde(default)]
    pub storage: Option<StorageConfig>,
}

pub fn default_http_port() -> u16 {
    9876
}

fn default_home_dir() -> PathBuf {
    PathBuf::from(DEFAULT_HOME_DIR)
}

fn default_nbd_socket() -> PathBuf {
    PathBuf::from(DEFAULT_NBD_SOCKET)
}

fn default_backend_file_name() -> String {
    "device".to_string()
}

impl Default for VolmountdConfig {
    fn default() -> Self {
        Self {
            home_dir: default_home_dir(),
            nbd_socket_path: default_nbd_socket(),
            auto_exports: Vec::new(),
            backend_file_name: default_backend_file_name(),
            http_port: default_http_port(),
            storage: None,
        }
    }
}

impl VolmountdConfig {
    /// 从 JSON 文件加载配置
    ///
    /// 缺失的字段自动使用默认值（通过 serde default）。
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let data = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        let config: VolmountdConfig = serde_json::from_str(&data).map_err(ConfigError::Parse)?;
        Ok(config)
    }

    /// 保存配置到 JSON 文件
    pub fn save(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        let data = serde_json::to_string_pretty(self).map_err(ConfigError::Parse)?;
        std::fs::write(path, &data).map_err(ConfigError::Io)?;
        Ok(())
    }

    /// 获取展开后的 home 目录（处理 ~ 前缀）
    pub fn resolved_home_dir(&self) -> PathBuf {
        let s = self.home_dir.to_string_lossy().to_string();
        if s.starts_with('~') {
            if let Some(home) = dirs_next::home_dir() {
                let stripped = s.strip_prefix("~/").unwrap_or(".");
                return home.join(stripped);
            }
        }
        self.home_dir.clone()
    }

    /// 获取 NBD socket 绝对路径
    pub fn resolved_nbd_socket(&self) -> PathBuf {
        self.resolved_home_dir().join(&self.nbd_socket_path)
    }

    /// 获取块设备目录
    pub fn blocks_dir(&self) -> PathBuf {
        self.resolved_home_dir().join("blocks")
    }

    /// 获取块设备对应的模拟块设备文件路径
    pub fn block_backend_path(&self, block_name: &str) -> PathBuf {
        self.blocks_dir()
            .join(block_name)
            .join(&self.backend_file_name)
    }

    /// 默认配置文件路径
    pub fn default_config_path() -> PathBuf {
        let home = dirs_next::home_dir().unwrap_or_default();
        home.join(".volmount").join("config.json")
    }
}

/// 配置错误
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),

    // ─── StorageConfig 验证 ───
    #[error("{0} must be a power of 2, got {1}")]
    NotPowerOf2(&'static str, u64),

    #[error("{0} must be aligned to {1}, got {2}")]
    NotAligned(&'static str, u64, u64),

    #[error("{0} out of range [{1}, {2}], got {3}")]
    OutOfRange(&'static str, u64, u64, u64),
}
