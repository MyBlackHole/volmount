//! StorageService — 块设备存储服务
//!
//! 封装块设备布局知识。Volume 不直接操作 Superblock 类型，
//! 全部通过 StorageService API 访问元数据和块 I/O。

use std::sync::Arc;

use crate::block_device::BlockDevice;
use crate::btree::types::BtreePtrV2;
use crate::journal::JournalSuperblockState;
use crate::meta::VolumeMeta;
use crate::types::StorageError;

use super::superblock::BchSb;

/// 块设备存储服务
///
/// 管理块设备级元数据（Superblock）和保留区。
pub struct StorageService {
    backend: Arc<dyn BlockDevice>,
    sb: BchSb,
}

impl StorageService {
    /// 创建设备（写 Superblock）
    pub async fn create(
        backend: Arc<dyn BlockDevice>,
        meta: VolumeMeta,
    ) -> Result<Self, StorageError> {
        // 创建 BchSb
        let sb = BchSb::new(meta);

        // 写 Superblock 到 BlockAddr 0
        sb.write_to_backend(&*backend).await?;

        Ok(Self { backend, sb })
    }

    /// 打开设备（读 Superblock）
    pub async fn open(backend: Arc<dyn BlockDevice>) -> Result<Self, StorageError> {
        let sb = BchSb::read_from_backend(&*backend).await?;

        Ok(Self { backend, sb })
    }

    /// 关闭设备（写 Superblock + flush）
    pub async fn close(&mut self) -> Result<(), StorageError> {
        self.sb.write_to_backend(&*self.backend).await?;
        self.backend.flush().await?;
        Ok(())
    }

    // ──── Superblock 字段访问器（Volume 不直接操作 Superblock 类型） ────

    pub fn volume_meta(&self) -> &VolumeMeta {
        &self.sb.vol_meta
    }

    pub fn volume_meta_mut(&mut self) -> &mut VolumeMeta {
        &mut self.sb.vol_meta
    }

    pub fn journal_seq(&self) -> u64 {
        self.sb.journal_seq
    }

    pub fn clean_shutdown(&self) -> bool {
        self.sb.clean_shutdown
    }

    pub fn set_journal_seq(&mut self, seq: u64) {
        self.sb.journal_seq = seq;
    }

    pub fn set_clean_shutdown(&mut self, val: bool) {
        self.sb.clean_shutdown = val;
    }

    // ──── Journal 字段访问器（Wave 1 新增） ────

    pub fn journal_buckets(&self) -> &[u64] {
        &self.sb.journal_buckets
    }

    pub fn journal_last_seq(&self) -> u64 {
        self.sb.journal_last_seq
    }

    pub fn journal_last_bucket(&self) -> u32 {
        self.sb.journal_last_bucket
    }

    pub fn root_addrs(&self) -> &[u64] {
        &self.sb.root_addrs
    }

    pub fn root_ptrs(&self) -> &[BtreePtrV2] {
        &self.sb.root_ptrs
    }

    /// 将 superblock 中的 journal 字段转换为 JournalSuperblockState
    pub fn journal_superblock_state(&self) -> JournalSuperblockState {
        let n = self.sb.journal_buckets.len();
        JournalSuperblockState {
            bucket_addrs: self.sb.journal_buckets.clone(),
            last_seq: self.sb.journal_last_seq,
            last_seq_ondisk: self.sb.journal_seq,
            last_bucket: self.sb.journal_last_bucket,
            discard_idx: self.sb.journal_discard_idx,
            dirty_idx: self.sb.journal_dirty_idx,
            dirty_idx_ondisk: self.sb.journal_dirty_idx_ondisk,
            bucket_seq: if self.sb.journal_bucket_seq.len() == n {
                self.sb.journal_bucket_seq.clone()
            } else {
                vec![0; n]
            },
            replayed_seqs: self.sb.replayed_seqs.clone(),
        }
    }

    pub fn set_journal_buckets(&mut self, buckets: Vec<u64>) {
        self.sb.journal_buckets = buckets;
    }

    pub fn set_journal_last_seq(&mut self, seq: u64) {
        self.sb.journal_last_seq = seq;
    }

    pub fn set_journal_last_bucket(&mut self, idx: u32) {
        self.sb.journal_last_bucket = idx;
    }

    pub fn set_root_addr(&mut self, ty_index: usize, addr: u64) {
        // 确保 Vec 长度足够
        if ty_index >= self.sb.root_addrs.len() {
            self.sb.root_addrs.resize(ty_index + 1, 0);
        }
        self.sb.root_addrs[ty_index] = addr;
        if ty_index >= self.sb.root_ptrs.len() {
            self.sb.root_ptrs.resize(ty_index + 1, BtreePtrV2::INVALID);
        }
        self.sb.root_ptrs[ty_index].block_addr = addr;
    }

    pub fn set_root_ptr(&mut self, ty_index: usize, ptr: BtreePtrV2) {
        if ty_index >= self.sb.root_addrs.len() {
            self.sb.root_addrs.resize(ty_index + 1, 0);
        }
        if ty_index >= self.sb.root_levels.len() {
            self.sb.root_levels.resize(ty_index + 1, 0);
        }
        if ty_index >= self.sb.root_ptrs.len() {
            self.sb.root_ptrs.resize(ty_index + 1, BtreePtrV2::INVALID);
        }
        self.sb.root_addrs[ty_index] = ptr.block_addr;
        self.sb.root_levels[ty_index] = ptr.level;
        self.sb.root_ptrs[ty_index] = ptr;
    }

    // ──── 后端刷新 ────

    pub async fn flush(&self) -> Result<(), StorageError> {
        self.backend.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::types::BackendType;

    fn test_meta() -> VolumeMeta {
        VolumeMeta::new(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            64 * 1024 * 1024,
            BackendType::Sparse,
        )
    }

    #[tokio::test]
    async fn test_create_open_roundtrip() {
        let backend = Arc::new(MockBlockDevice::new());
        let meta = test_meta();

        // 创建设备
        let mut svc = StorageService::create(backend.clone(), meta.clone())
            .await
            .unwrap();
        // VolumeMeta 没有 PartialEq，逐字段验证
        {
            let actual = svc.volume_meta();
            assert_eq!(actual.vol_name, meta.vol_name);
            assert_eq!(actual.vol_id, meta.vol_id);
            assert_eq!(actual.block_size, meta.block_size);
            assert_eq!(actual.capacity, meta.capacity);
        }
        assert!(!svc.clean_shutdown());

        svc.set_clean_shutdown(true);
        svc.close().await.unwrap();

        // 重新打开
        let svc2 = StorageService::open(backend.clone()).await.unwrap();
        {
            let actual = svc2.volume_meta();
            assert_eq!(actual.vol_name, meta.vol_name);
            assert_eq!(actual.capacity, meta.capacity);
        }
        assert!(svc2.clean_shutdown());
    }
}
