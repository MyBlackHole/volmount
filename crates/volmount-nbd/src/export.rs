use std::sync::Arc;

use volmount_core::block_device::BlockDevice;

/// NBD 导出定义
///
/// 一个导出对应一个卷，具备名称、大小和存储后端。
pub struct NbdExport {
    /// 导出名称（卷名）
    pub name: String,
    /// 卷大小（字节）
    pub size: u64,
    /// 存储后端
    pub backend: Arc<dyn BlockDevice>,
    /// NBD 传输标记
    pub flags: u16,
}

impl NbdExport {
    pub fn new(name: impl Into<String>, size: u64, backend: Arc<dyn BlockDevice>) -> Self {
        let flags = crate::protocol::NBD_FLAG_HAS_FLAGS
            | crate::protocol::NBD_FLAG_SEND_FLUSH
            | crate::protocol::NBD_FLAG_SEND_TRIM;
        Self {
            name: name.into(),
            size,
            backend,
            flags,
        }
    }
}
