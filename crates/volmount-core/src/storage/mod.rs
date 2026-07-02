//! Storage — 块设备存储层
//!
//! 管理块设备级别的布局：超块区（BlockAddr 0 的元数据块）、保留区、
//! 以及块 I/O 工具函数（分配/读取/写入序列化数据到后端块）。

pub mod block_io;
pub mod service;
pub mod superblock;

pub use block_io::{read_data_from_blocks, write_data_to_blocks};
pub use service::StorageService;
pub use superblock::{BackupSbLayout, BchSb};
