//! BchSb — 块设备超块区（对齐 bcachefs on-disk superblock）
//!
//! 存储在 BlockAddr 0，固定 4096 字节。包含 Volume 元数据及所有子系统状态指针：
//!
//! ```text
//! BlockAddr 0:  BchSb (4KB) — VolumeMeta + 指针
//! BlockAddr 1-7: 保留（未来元数据扩展）
//! BlockAddr 8+:  数据块（由 BchAllocator 管理）
//! ```
//!
//! # bcachefs 对齐
//!
//! bcachefs 的 superblock 位于设备起始处的固定偏移，
//! 包含 magic/version/backpointers/journal_buckets 等。
//! 本实现使用 BlockAddr 0 作为超块区，不使用文件系统层级。

use serde::{Deserialize, Serialize};

use crate::block_device::BlockDevice;
use crate::btree::gc::GcPos;
use crate::btree::types::BtreePtrV2;
use crate::journal::Crc32CHasher;
use crate::meta::VolumeMeta;
use crate::types::{BlockAddr, StorageError};

/// 超块魔数
pub const SUPERBLOCK_MAGIC: [u8; 8] = *b"VOLMOUNT";
/// 当前超块格式版本
pub const SUPERBLOCK_VERSION: u32 = 1;
/// 超块所在 BlockAddr
pub const SUPERBLOCK_ADDR: u64 = 0;
/// 超块大小（固定 4KB，占一个完整 block）
pub const SUPERBLOCK_SIZE: usize = 4096;
/// 保留块数量（BlockAddr 0..RESERVED_BLOCKS 不纳入数据分配器）
pub const RESERVED_BLOCKS: u64 = 8;

/// 超块 feature bits（bcachefs 对齐，对应 `BCH_FEATURE_*`）
///
/// 存储在 `BchSb::features[0]` 的 0-63 位。
///
/// # bcachefs 对齐
///
/// 位号 0-21 与 bcachefs `BCH_SB_FEATURES()` 完全一致：
/// - BIT(0)  = BCH_FEATURE_lz4
/// - BIT(1)  = BCH_FEATURE_gzip
/// - ...
/// - BIT(21) = BCH_FEATURE_no_alloc_info
///
/// 位号 22+ 为 volmount 自定义（位于 bcachefs BCH_FEATURE_NR 之上）。
pub mod feature_bits {
    /// BIT(21): alloc 信息不可用（bcachefs `BCH_FEATURE_no_alloc_info`）
    ///
    /// 语义与 bcachefs 一致——否定式：
    /// - bit = 1 → alloc 信息不存在（旧格式）
    /// - bit = 0 → alloc 信息存在（新格式化）
    pub const NO_ALLOC_INFO: u32 = 21;
    /// BIT(22): journal 日志可用（volmount 自定义，非 bcachefs feature bit）
    pub const JOURNAL: u32 = 22;
    /// BIT(23): 快照功能可用（volmount 自定义，非 bcachefs feature bit）
    pub const SNAPSHOTS: u32 = 23;
}

/// 超块 — 整个 Volume 的元数据入口
///
/// 以 bincode 序列化后写入 BlockAddr 0，固定 4096 字节。
///
/// # CRC 校验
///
/// `crc != 0` 时启用 CRC32 校验（使用 crc32fast）。
/// 校验方式：将 `crc` 字段清零后序列化，对序列化结果计算 CRC32，
/// 与存储值比对。`crc == 0` 时跳检验证（向后兼容旧版本）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BchSb {
    /// 文件格式魔数
    pub magic: [u8; 8],
    /// 格式版本
    pub version: u32,
    /// CRC32 校验和（0 = 未设置，向后兼容旧版）
    #[serde(default)]
    pub crc: u32,
    /// Volume 元数据
    pub vol_meta: VolumeMeta,

    // ─── WAL journal 状态 ───
    /// 当前 WAL seq
    pub journal_seq: u64,

    // ─── Flags ───
    /// 是否正常关闭
    pub clean_shutdown: bool,

    // ─── Journal 位置（Wave 1 新增，#[serde(default)] 向后兼容） ───
    /// 预分配的 journal bucket addrs（动态长度，不再固定 32）
    #[serde(default)]
    pub journal_buckets: Vec<u64>,
    /// 最近的 journal seq
    #[serde(default)]
    pub journal_last_seq: u64,
    /// 当前 journal bucket 索引
    #[serde(default)]
    pub journal_last_bucket: u32,

    // ─── Btree roots（Wave 3 使用，Wave 1 预占位） ───
    /// 每个 btree type 的 root node block addr（pre-Vec 以兼容 serde）
    #[serde(default)]
    pub root_addrs: Vec<u64>,
    /// 每个 btree type 的 root node level
    #[serde(default)]
    pub root_levels: Vec<u8>,
    /// 每个 btree type 的完整 root pointer（addr/sector/level/generation）
    #[serde(default)]
    pub root_ptrs: Vec<BtreePtrV2>,

    // ─── Phase 3: Journal 索引持久化（完整 JournalBchSbState 覆盖） ───
    #[serde(default)]
    pub journal_discard_idx: u32,
    #[serde(default)]
    pub journal_dirty_idx: u32,
    #[serde(default)]
    pub journal_dirty_idx_ondisk: u32,
    #[serde(default)]
    pub journal_bucket_seq: Vec<u64>,
    #[serde(default)]
    pub replayed_seqs: Vec<u64>,

    // ─── Phase 3: Recovery passes 持久化 ───
    #[serde(default)]
    pub pass_done: u64,

    // ─── GC 位置持久化 ───
    /// GC 完成时的位置标记（用于增量 GC 恢复）
    #[serde(default)]
    pub gc_pos: GcPos,
    /// gc_pos 是否有效（旧版本无此字段时 false）
    #[serde(default)]
    pub gc_pos_valid: bool,

    // ─── P2: UUID ───
    /// 卷 UUID（唯一标识，类似于 bcachefs sb.uuid）
    #[serde(default)]
    pub uuid: [u8; 16],
    /// 用户指定 UUID（类似于 bcachefs sb.user_uuid）
    #[serde(default)]
    pub user_uuid: [u8; 16],

    // ─── P2: Feature flags ───
    /// 功能标志位（类似于 bcachefs sb.features，bit 0-63）
    #[serde(default)]
    pub features: [u64; 2],
    /// 兼容标志位（类似于 bcachefs sb.compat）
    #[serde(default)]
    pub compat: [u64; 2],

    // ─── P2: Backup superblock layout ───
    /// 超块副本布局（None = 仅 BlockAddr 0，无副本）
    #[serde(default)]
    pub layout: Option<BackupSbLayout>,

    // ─── StorageConfig（Batch D 新增） ───
    /// 存储引擎配置（None = 使用默认值）
    #[serde(default)]
    pub storage_config: Option<crate::config::StorageConfig>,
}

/// 超块副本布局 — 定义主超块和备份副本的位置
///
/// bcachefs 在设备上保留多个 superblock 副本以提高可靠性。
/// 本实现支持 1 个 primary + 任意数量的 replica。
///
/// # 默认值
///
/// primary = BlockAddr 0, replicas = [BlockAddr 4, BlockAddr 8]。
/// 这些地址位于保留区域（BlockAddr 0..8），不会被数据分配器使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSbLayout {
    /// 主超块 BlockAddr.raw（通常为 0）
    pub primary: u64,
    /// 备份副本 BlockAddr.raw 列表
    pub replicas: Vec<u64>,
}

impl Default for BackupSbLayout {
    fn default() -> Self {
        Self {
            primary: SUPERBLOCK_ADDR, // 0
            replicas: vec![4, 8],
        }
    }
}

impl BchSb {
    /// 创建新的超块（用于 Volume::create）
    pub fn new(meta: VolumeMeta) -> Self {
        Self {
            magic: SUPERBLOCK_MAGIC,
            version: SUPERBLOCK_VERSION,
            crc: 0,
            vol_meta: meta,
            journal_seq: 0,
            clean_shutdown: false,
            journal_buckets: Vec::new(),
            journal_last_seq: 0,
            journal_last_bucket: 0,
            root_addrs: Vec::new(),
            root_levels: Vec::new(),
            root_ptrs: Vec::new(),
            journal_discard_idx: 0,
            journal_dirty_idx: 0,
            journal_dirty_idx_ondisk: 0,
            journal_bucket_seq: Vec::new(),
            replayed_seqs: Vec::new(),
            pass_done: 0,
            gc_pos: GcPos {
                phase: crate::btree::gc::GcPhase::NotRunning,
                btree: 0,
                level: 0,
                pos: 0,
                journal_seq: 0,
            },
            gc_pos_valid: false,
            // ─── P2: UUID ───
            uuid: [0u8; 16],
            user_uuid: [0u8; 16],
            // ─── P2: Feature flags ───
            features: [0u64; 2],
            compat: [0u64; 2],
            // ─── P2: Backup superblock layout ───
            layout: None,
            // ─── StorageConfig ───
            storage_config: None,
        }
    }

    /// 序列化超块到字节（填充到 SUPERBLOCK_SIZE 以便直接写入 block）
    ///
    /// 序列化时计算 CRC32（将 `crc` 字段清零后对整个数据计算 CRC）。
    pub fn serialize(&self) -> Result<Vec<u8>, StorageError> {
        // 第一遍：crc=0 序列化，计算 CRC
        let mut crc_zero = self.clone();
        crc_zero.crc = 0;
        let zeroed_bytes = bincode::serialize(&crc_zero)?;
        if zeroed_bytes.len() > SUPERBLOCK_SIZE {
            return Err(StorageError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "superblock too large: {} > {}",
                    zeroed_bytes.len(),
                    SUPERBLOCK_SIZE
                ),
            )));
        }
        let crc = Crc32CHasher::hash(&zeroed_bytes);

        // 第二遍：填入 CRC 后序列化
        let mut sb_with_crc = self.clone();
        sb_with_crc.crc = crc;
        let mut data = bincode::serialize(&sb_with_crc)?;
        debug_assert_eq!(
            data.len(),
            zeroed_bytes.len(),
            "CRC field must not change serialized size"
        );
        debug_assert!(crc != 0, "CRC32 of non-empty superblock should never be 0");

        // 填充到固定大小
        data.resize(SUPERBLOCK_SIZE, 0);
        Ok(data)
    }

    /// 从字节反序列化超块
    ///
    /// 如果 `crc != 0` 则验证 CRC32 校验和（crc==0 向后兼容旧版本）。
    pub fn deserialize(data: &[u8]) -> Result<Self, StorageError> {
        if data.len() < 16 {
            return Err(StorageError::NotFound("superblock data too short".into()));
        }
        let sb: BchSb = bincode::deserialize(data)?;
        if sb.magic != SUPERBLOCK_MAGIC {
            return Err(StorageError::NotFound(format!(
                "invalid superblock magic: {:?}",
                &sb.magic
            )));
        }
        if sb.version != SUPERBLOCK_VERSION && sb.version != 1 {
            return Err(StorageError::NotFound(format!(
                "unsupported superblock version {}, expected {} or 1",
                sb.version, SUPERBLOCK_VERSION
            )));
        }
        // CRC 校验：crc != 0 时验证，=0 时跳过（旧版兼容）
        if sb.crc != 0 {
            let mut check = sb.clone();
            check.crc = 0;
            let zeroed_bytes = bincode::serialize(&check)?;
            let computed = Crc32CHasher::hash(&zeroed_bytes);
            if computed != sb.crc {
                return Err(StorageError::ChecksumMismatch {
                    expected: sb.crc,
                    actual: computed,
                });
            }
        }
        Ok(sb)
    }

    /// 生成随机 UUID（填充 uuid 和 user_uuid 字段）
    ///
    /// 使用 `rand::thread_rng` 生成 16 字节随机值。
    pub fn generate_uuids(&mut self) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        rng.fill(&mut self.uuid);
        rng.fill(&mut self.user_uuid);
    }

    // ─── Feature flag helpers ───

    /// 检查指定 feature bit 是否已设置
    ///
    /// bit 范围：0..127（features[0] 覆盖 0-63, features[1] 覆盖 64-127）。
    /// 超出范围的 bit 返回 false。
    pub fn feature_test(&self, bit: u32) -> bool {
        if bit < 64 {
            (self.features[0] & (1u64 << bit)) != 0
        } else if bit < 128 {
            (self.features[1] & (1u64 << (bit - 64))) != 0
        } else {
            false
        }
    }

    /// 设置指定 feature bit
    pub fn feature_set(&mut self, bit: u32) {
        if bit < 64 {
            self.features[0] |= 1u64 << bit;
        } else if bit < 128 {
            self.features[1] |= 1u64 << (bit - 64);
        }
    }

    /// 检查指定 compat bit 是否已设置
    pub fn compat_test(&self, bit: u32) -> bool {
        if bit < 64 {
            (self.compat[0] & (1u64 << bit)) != 0
        } else if bit < 128 {
            (self.compat[1] & (1u64 << (bit - 64))) != 0
        } else {
            false
        }
    }

    /// 设置指定 compat bit
    pub fn compat_set(&mut self, bit: u32) {
        if bit < 64 {
            self.compat[0] |= 1u64 << bit;
        } else if bit < 128 {
            self.compat[1] |= 1u64 << (bit - 64);
        }
    }

    /// 返回此超块需要写入的所有 BlockAddr 列表
    ///
    /// 根据 `layout` 字段决定：
    /// - `None`：仅 primary（BlockAddr 0，向后兼容）
    /// - `Some(layout)`：primary + 所有 replica 地址
    fn target_addrs(&self) -> Vec<u64> {
        match &self.layout {
            Some(layout) => {
                let mut addrs = vec![layout.primary];
                addrs.extend_from_slice(&layout.replicas);
                addrs
            }
            None => vec![SUPERBLOCK_ADDR],
        }
    }

    /// 将超块写入后端（所有副本）
    ///
    /// 写入 primary 和所有备份副本。如果任一副本写入失败，
    /// 返回第一个错误。primary 写入先执行以保证至少有一个有效副本。
    pub async fn write_to_backend(&self, backend: &dyn BlockDevice) -> Result<(), StorageError> {
        let data = self.serialize()?;
        let addrs = self.target_addrs();
        for addr in &addrs {
            backend.write_block(BlockAddr::new(*addr), &data).await?;
        }
        Ok(())
    }

    /// 从后端读取超块（primary first，失败时回退副本）
    ///
    /// 1. 尝试从 BlockAddr 0（primary）读取
    /// 2. 如果 primary 读取失败或 CRC 不匹配，尝试每个副本
    /// 3. 返回第一个成功解析的超块
    /// 4. 如果所有位置都失败，返回最后一个错误
    pub async fn read_from_backend(backend: &dyn BlockDevice) -> Result<Self, StorageError> {
        let mut last_err = StorageError::NotFound("no superblock found".into());

        // Step 1: try primary at BlockAddr 0 (backward compatible default)
        match Self::read_from_addr(backend, SUPERBLOCK_ADDR).await {
            Ok(sb) => return Ok(sb),
            Err(e) => last_err = e,
        }

        // Step 2: try each replica from the primary's layout (if readable)
        // We need to know the layout to find replicas. Try reading primary first
        // to get the layout, but primary already failed. Fallback to default
        // replica positions: BlockAddr 4, 8.
        for addr in &[4u64, 8u64] {
            match Self::read_from_addr(backend, *addr).await {
                Ok(sb) => return Ok(sb),
                Err(e) => last_err = e,
            }
        }

        Err(last_err)
    }

    /// 从后端读取超块（显式回退模式 — 跳过 primary，直接尝试副本）
    ///
    /// 当 primary 已知损坏且希望强制使用备份副本时使用。
    /// 按顺序尝试每个副本，返回第一个有效的超块。
    pub async fn read_from_backend_with_fallback(
        backend: &dyn BlockDevice,
    ) -> Result<Self, StorageError> {
        let mut last_err = StorageError::NotFound("no superblock found".into());

        // 尝试默认副本位置：BlockAddr 4, 8
        for addr in &[4u64, 8u64] {
            match Self::read_from_addr(backend, *addr).await {
                Ok(sb) => return Ok(sb),
                Err(e) => last_err = e,
            }
        }

        Err(last_err)
    }

    /// 从指定 BlockAddr 读取并反序列化超块
    async fn read_from_addr(backend: &dyn BlockDevice, addr: u64) -> Result<Self, StorageError> {
        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        backend.read_block(BlockAddr::new(addr), &mut buf).await?;
        Self::deserialize(&buf)
    }

    /// 检查设备是否缺少 alloc 信息
    ///
    /// 对应 bcachefs `c->sb.features & BIT_ULL(BCH_FEATURE_no_alloc_info)`。
    /// 当 `has_no_alloc_info()` 返回 true 时，PASS_ALLOC 标志的 pass 会被跳过。
    ///
    /// 语义与 bcachefs 一致——否定式：
    /// - bit=1 (NO_ALLOC_INFO set) → alloc 信息不存在
    /// - bit=0 (NO_ALLOC_INFO clear) → alloc 信息存在
    pub fn has_no_alloc_info(&self) -> bool {
        self.feature_test(feature_bits::NO_ALLOC_INFO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::VolumeMeta;
    use crate::types::BackendType;

    fn test_meta() -> VolumeMeta {
        VolumeMeta::new(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            1024 * 1024 * 1024,
            BackendType::Sparse,
        )
    }

    #[test]
    fn test_superblock_roundtrip() {
        let meta = test_meta();
        let sb = BchSb::new(meta.clone());
        let data = sb.serialize().unwrap();
        assert_eq!(data.len(), SUPERBLOCK_SIZE);

        let restored = BchSb::deserialize(&data).unwrap();
        assert_eq!(restored.vol_meta.vol_name, meta.vol_name);
        assert_eq!(restored.version, SUPERBLOCK_VERSION);
        assert_eq!(restored.magic, SUPERBLOCK_MAGIC);
        assert!(!restored.clean_shutdown);
    }

    #[test]
    fn test_superblock_preserves_all_fields() {
        let meta = test_meta();
        let mut sb = BchSb::new(meta);
        sb.journal_seq = 42;
        sb.clean_shutdown = true;

        let data = sb.serialize().unwrap();
        let restored = BchSb::deserialize(&data).unwrap();
        assert_eq!(restored.journal_seq, 42);
        assert!(restored.clean_shutdown);
    }

    #[test]
    fn test_superblock_invalid_magic() {
        let mut data = vec![0u8; SUPERBLOCK_SIZE];
        data[..8].copy_from_slice(b"BADMAGIC");
        let result = BchSb::deserialize(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_superblock_too_short() {
        let result = BchSb::deserialize(&[0u8; 8]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_superblock_write_read_backend() {
        let backend = crate::block_device::MockBlockDevice::new();
        let meta = test_meta();
        let sb = BchSb::new(meta.clone());

        sb.write_to_backend(&backend).await.unwrap();

        let restored = BchSb::read_from_backend(&backend).await.unwrap();
        assert_eq!(restored.vol_meta.vol_name, meta.vol_name);
    }
}
