use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use volmount_core::alloc::BchAllocator;
use volmount_core::block_device::BlockDevice;
use volmount_core::block_device::S3Config;
use volmount_core::btree::key::KeyType;
use volmount_core::btree::trigger::{TriggerPhase, TriggerRegistry};
use volmount_core::btree::{bucket_io::write_initial_node, BtreeEngine, BtreeId, WritebackHandle};
use volmount_core::btree::{BtreePtrV2, BTREE_ID_NR};
use volmount_core::journal::{
    Journal, JournalError, JournalSuperblockState, DEFAULT_JOURNAL_BUCKETS,
};
use volmount_core::meta::VolumeMeta;
use volmount_core::recovery;
use volmount_core::snap::snapshot::{create_root_snapshot_btree, list_snapshots_from_btree};
use volmount_core::storage::superblock::RESERVED_BLOCKS;
use volmount_core::storage::BchSb;
use volmount_core::types::{BackendType, BlockSize, StorageError};
use volmount_core::volume::{Volume as CoreVolume, VolumeConfig};

use crate::config::VolmountdConfig;

/// 卷状态（daemon 层 Volume 包装）
pub struct Volume {
    pub meta: VolumeMeta,
    /// 核心 Volume（纯聚合容器，无 I/O）
    pub inner: RwLock<CoreVolume>,
    /// 块存储后端（用于 NBD 直连读写 + daemon I/O 操作）
    pub backend: Arc<dyn BlockDevice>,
    /// 卷级 Journal（与 core volume 同生命周期）
    pub journal: Arc<Journal>,
    /// 卷级 writeback worker
    pub writeback: Arc<WritebackHandle>,
}

// ─── 目录初始化 ───

/// 初始化 home/blocks 目录结构（等价 bcachefs 创建挂载点）
pub async fn init_dirs(config: &VolmountdConfig) -> Result<(), DaemonError> {
    let home = config.resolved_home_dir();
    tokio::fs::create_dir_all(&home).await?;
    tokio::fs::create_dir_all(config.blocks_dir()).await?;
    Ok(())
}

// ─── 创建新卷 ───

/// 创建新卷（等价 bcachefs `bch2_fs_open` 创建新文件系统分支）
///
/// 1. 创建存储后端
/// 2. 写 Superblock（BlockAddr 0）
/// 3. 创建空 BtreeEngine + 根快照 + 分配器（纯内存）
/// 4. 构造 CoreVolume
/// 5. superblock 已包含卷元数据，无需额外持久化文件
pub async fn create_volume(
    config: &VolmountdConfig,
    name: &str,
    backend_type: BackendType,
    capacity: u64,
    block_size: BlockSize,
) -> Result<Arc<Volume>, DaemonError> {
    let vol_dir = config.blocks_dir().join(name);

    if vol_dir.exists() {
        return Err(DaemonError::VolumeExists(name.to_string()));
    }

    tokio::fs::create_dir_all(&vol_dir).await?;

    // 1. 创建存储后端
    let backend = create_backend(config, backend_type, &vol_dir, Some(capacity)).await?;

    // 2. 准备并持久化各 btree 的初始根节点
    let meta = VolumeMeta::new(
        name.to_string(),
        1,
        "default".to_string(),
        block_size,
        capacity,
        backend_type,
    );
    // 3. 创建纯内存组件
    let total_blocks = capacity / block_size as u64;
    let group_size = 1024u64.min(total_blocks / 4).max(256);
    let allocator = BchAllocator::new(total_blocks, group_size, RESERVED_BLOCKS);

    let mut engine = BtreeEngine::new();
    let root_snapshot_id = create_root_snapshot_btree(&mut engine, 0)?;

    let mut sb = BchSb::new(meta.clone());
    let journal_bucket_count = journal_bucket_count(total_blocks);
    let journal = Arc::new(create_or_load_journal(
        &mut sb,
        &allocator,
        &mut engine,
        true,
        journal_bucket_count,
    )?);

    persist_initial_root_nodes(&*backend, &mut engine, &mut sb).await?;

    // 新创建的卷始终有 alloc 信息：默认 features=[0,0] → NO_ALLOC_INFO=0（bcachefs 对齐）
    sb.write_to_backend(&*backend).await?;

    let mut registry = TriggerRegistry::new();
    registry.register(
        BtreeId::Extents,
        KeyType::Normal as u8,
        TriggerPhase::Atomic,
        volmount_core::alloc::bch2_trigger_extent,
    );
    let trigger_registry = Arc::new(registry);

    // 4. 构造 CoreVolume
    let writeback = WritebackHandle::new();
    let vol_config = VolumeConfig {
        vol_name: name.to_string(),
        pool_name: "default".into(),
        block_size,
        capacity,
    };
    let core_vol = CoreVolume::new(
        meta.clone(),
        backend.clone(),
        journal.clone(),
        engine,
        root_snapshot_id,
        allocator,
        trigger_registry,
        vol_config,
    );
    let _ = core_vol.set_writeback_handle(writeback.clone());

    let volume = Arc::new(Volume {
        meta,
        inner: RwLock::new(core_vol),
        backend,
        journal,
        writeback,
    });

    Ok(volume)
}

// ─── 加载已有卷 ───

/// 初始化已有卷（等价 bcachefs `bch2_fs_open` 加载已存在文件系统）
///
/// 1. 从 volume 目录和 backend layout 推断后端类型
/// 2. 创建后端
/// 3. 读 Superblock → 恢复 Journal/root pointers
/// 4. 通过 recovery 递归加载 Btree roots
/// 5. 从 Snapshots btree 获取根快照 ID
/// 6. 从 Alloc btree 加载分配器状态
/// 7. 构造 CoreVolume
pub async fn init_volume(config: &VolmountdConfig, name: &str) -> Result<Arc<Volume>, DaemonError> {
    let vol_dir = config.blocks_dir().join(name);

    if !vol_dir.exists() {
        return Err(DaemonError::VolumeNotFound(name.to_string()));
    }

    // 1. 推断后端类型并创建后端
    let backend_type = infer_backend_type(config, &vol_dir).await?;
    let backend = create_backend(config, backend_type, &vol_dir, None).await?;

    // 2. 读 Superblock
    let mut sb = BchSb::read_from_backend(&*backend).await?;
    let meta = sb.vol_meta.clone();

    if meta.backend_type != backend_type {
        return Err(DaemonError::Storage(StorageError::InvalidData(format!(
            "block device backend type mismatch for '{}': sb={:?} inferred={:?}",
            name, meta.backend_type, backend_type
        ))));
    }

    // 3. BtreeEngine 加载（统一走 journal/root recovery 路径）
    let total_blocks = meta.capacity / meta.block_size as u64;
    let group_size = 1024u64.min(total_blocks / 4).max(256);
    let allocator = BchAllocator::new(total_blocks, group_size, RESERVED_BLOCKS);
    let mut engine = BtreeEngine::new();
    let journal = if let Some(state) = journal_state_from_superblock(&sb) {
        Journal::from_superblock(&state)
    } else {
        let journal = Journal::create(&allocator, &mut engine, journal_bucket_count(total_blocks))?;
        sync_superblock_from_journal(&mut sb, &journal);
        sb.write_to_backend(&*backend).await?;
        journal
    };

    if !sb.clean_shutdown {
        tracing::info!(
            "volume '{}': unclean shutdown, recovering from journal",
            name
        );
    }

    let mut state = recovery::RecoveryState::new(
        engine,
        journal,
        backend.clone(),
        sb.clone(),
        allocator, // 分配器移入 RecoveryState，AllocRead pass 内部恢复
    );
    recovery::bch2_fs_recovery(&mut state).await?;

    let mut recovered_sb = state.superblock.clone();
    let jss = state.journal.to_superblock_state();
    recovered_sb.pass_done = state.pass_done as u64;
    recovered_sb.replayed_seqs = jss.replayed_seqs;
    recovered_sb.journal_last_seq = jss.last_seq;
    recovered_sb.journal_seq = jss.last_seq_ondisk;
    recovered_sb.journal_last_bucket = jss.last_bucket;
    recovered_sb.journal_discard_idx = jss.discard_idx;
    recovered_sb.journal_dirty_idx = jss.dirty_idx;
    recovered_sb.journal_dirty_idx_ondisk = jss.dirty_idx_ondisk;
    recovered_sb.journal_bucket_seq = jss.bucket_seq;
    recovered_sb.write_to_backend(&*backend).await?;

    let (mut engine, allocator) = state.take_engine_and_allocator();
    let journal = state.journal;
    let journal = Arc::new(journal);

    // 4. 从 Snapshots btree 获取根快照 ID
    let root_snapshot_id = {
        let snaps = list_snapshots_from_btree(&engine);
        snaps.iter().find(|(_, v)| v.parent == 0).map(|(id, _)| *id)
    }
    .unwrap_or_else(|| create_root_snapshot_btree(&mut engine, 0).unwrap_or(u32::MAX));

    // 5. 创建触发器注册表
    let mut registry = TriggerRegistry::new();
    registry.register(
        BtreeId::Extents,
        KeyType::Normal as u8,
        TriggerPhase::Atomic,
        volmount_core::alloc::bch2_trigger_extent,
    );
    let trigger_registry = Arc::new(registry);

    // 6. 构造 CoreVolume（allocator 已从 journal/btree 恢复）
    let writeback = WritebackHandle::new();
    let vol_config = VolumeConfig {
        vol_name: meta.vol_name.clone(),
        pool_name: String::new(),
        block_size: meta.block_size,
        capacity: meta.capacity,
    };
    let core_vol = CoreVolume::new(
        meta.clone(),
        backend.clone(),
        journal.clone(),
        engine,
        root_snapshot_id,
        allocator,
        trigger_registry,
        vol_config,
    );
    let _ = core_vol.set_writeback_handle(writeback.clone());

    let volume = Arc::new(Volume {
        meta,
        inner: RwLock::new(core_vol),
        backend,
        journal,
        writeback,
    });

    Ok(volume)
}

// ─── Stop / Drain ───

/// 停止卷并标记 clean_shutdown（等价 bcachefs `bch2_fs_stop`）
///
/// 语义：
/// 1. 等待 writeback worker 空闲
/// 2. flush 所有脏 btree 节点
/// 3. 将当前 root pointers 回写到 superblock
/// 4. 刷新 journal 并持久化 superblock 的 journal 状态
/// 5. 标记 clean_shutdown 并写回 superblock
pub async fn stop_volume(volume: &Volume) -> Result<(), DaemonError> {
    volume.writeback.wait_idle()?;

    let mut core = volume.inner.write().await;

    // 0. 读取 superblock 作为 journal / root 状态的持久化基底
    let mut sb = BchSb::read_from_backend(&*volume.backend).await?;

    // 1. 将所有 btree 的脏节点写回后端，确保 root 节点已落盘
    let mappings = core.flush_dirty_nodes().await?;
    apply_root_mappings(core.engine_mut(), &mappings);

    // 2. 将当前 root pointers 回写到 superblock
    sync_superblock_from_engine(&mut sb, core.engine());

    // 3. journal flush + blacklist（对应 bcachefs `bch2_journal_blacklist()`）
    // 空 journal 的新 block device 允许直接关闭，不强制走 blacklist。
    if sb.journal_last_seq > 0 {
        volume.journal.bch2_journal_flush(&*volume.backend).await?;
        volume
            .journal
            .bch2_journal_seq_blacklist_add(1, sb.journal_last_seq, &*volume.backend)
            .await?;

        // 持久化更新后的 journal 状态（含 journal 索引）
        let new_state = volume.journal.to_superblock_state();
        sb.journal_last_seq = new_state.last_seq;
        sb.journal_seq = new_state.last_seq_ondisk;
        sb.journal_last_bucket = new_state.last_bucket;
        sb.journal_discard_idx = new_state.discard_idx;
        sb.journal_dirty_idx = new_state.dirty_idx;
        sb.journal_dirty_idx_ondisk = new_state.dirty_idx_ondisk;
        sb.journal_bucket_seq = new_state.bucket_seq;
        sb.replayed_seqs = new_state.replayed_seqs;
    }

    // 4. 标记 clean_shutdown 并持久化
    sb.clean_shutdown = true;
    sb.write_to_backend(&*volume.backend).await?;

    Ok(())
}

// ─── 删除卷 ───

/// 删除卷（stop/drain + 移除目录）
pub async fn delete_volume(config: &VolmountdConfig, name: &str) -> Result<(), DaemonError> {
    let vol_dir = config.blocks_dir().join(name);
    if !vol_dir.exists() {
        return Err(DaemonError::VolumeNotFound(name.to_string()));
    }

    // 先 init（通过 superblock 获取 backend 信息），再 stop/drain 后删除
    let vol = init_volume(config, name).await?;
    if let Err(e) = stop_volume(&vol).await {
        tracing::warn!(
            "best-effort stop failed before deleting block '{}': {e}",
            name
        );
    }

    tokio::fs::remove_dir_all(&vol_dir).await?;
    tracing::info!("deleted volume '{}'", name);
    Ok(())
}

/// 从现有块设备克隆一个新块设备
pub async fn clone_volume(
    config: &VolmountdConfig,
    source_name: &str,
    clone_name: &str,
) -> Result<Arc<Volume>, DaemonError> {
    let source_dir = config.blocks_dir().join(source_name);
    if !source_dir.exists() {
        return Err(DaemonError::VolumeNotFound(source_name.to_string()));
    }
    let clone_dir = config.blocks_dir().join(clone_name);
    if clone_dir.exists() {
        return Err(DaemonError::VolumeExists(clone_name.to_string()));
    }

    tokio::fs::create_dir_all(&clone_dir).await?;

    let source_backend_path = config.block_backend_path(source_name);
    let clone_backend_path = config.block_backend_path(clone_name);
    clone_sparse_backend_file(&source_backend_path, &clone_backend_path).await?;

    let backend_type = infer_backend_type(config, &clone_dir).await?;
    let backend = create_backend(config, backend_type, &clone_dir, None).await?;
    let mut sb = BchSb::read_from_backend(&*backend).await?;
    sb.vol_meta.vol_name = clone_name.to_string();
    sb.vol_meta.vol_id = sb.vol_meta.vol_id.saturating_add(1);
    sb.vol_meta.created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    sb.clean_shutdown = true;
    sb.write_to_backend(&*backend).await?;

    init_volume(config, clone_name).await
}

impl Drop for Volume {
    fn drop(&mut self) {
        self.writeback.close();
    }
}

fn journal_state_from_superblock(sb: &BchSb) -> Option<JournalSuperblockState> {
    if sb.journal_buckets.is_empty() || sb.journal_last_seq == 0 {
        return None;
    }
    let n = sb.journal_buckets.len();
    Some(JournalSuperblockState {
        bucket_addrs: sb.journal_buckets.clone(),
        last_seq: sb.journal_last_seq,
        last_seq_ondisk: sb.journal_seq,
        last_bucket: sb.journal_last_bucket,
        discard_idx: sb.journal_discard_idx,
        dirty_idx: sb.journal_dirty_idx,
        dirty_idx_ondisk: sb.journal_dirty_idx_ondisk,
        bucket_seq: if sb.journal_bucket_seq.len() == n {
            sb.journal_bucket_seq.clone()
        } else {
            vec![0; n]
        },
        replayed_seqs: sb.replayed_seqs.clone(),
    })
}

fn sync_superblock_from_journal(sb: &mut BchSb, journal: &Journal) {
    let state = journal.to_superblock_state();
    sb.journal_buckets = state.bucket_addrs;
    sb.journal_last_seq = state.last_seq;
    sb.journal_seq = state.last_seq_ondisk;
    sb.journal_last_bucket = state.last_bucket;
    sb.journal_discard_idx = state.discard_idx;
    sb.journal_dirty_idx = state.dirty_idx;
    sb.journal_dirty_idx_ondisk = state.dirty_idx_ondisk;
    sb.journal_bucket_seq = state.bucket_seq;
    sb.replayed_seqs = state.replayed_seqs;
}

fn sync_superblock_from_engine(sb: &mut BchSb, engine: &BtreeEngine) {
    let root_count = BTREE_ID_NR.len();
    if sb.root_addrs.len() < root_count {
        sb.root_addrs.resize(root_count, 0);
    }
    if sb.root_levels.len() < root_count {
        sb.root_levels.resize(root_count, 0);
    }
    if sb.root_ptrs.len() < root_count {
        sb.root_ptrs.resize(root_count, BtreePtrV2::INVALID);
    }

    for (idx, ty) in BTREE_ID_NR.into_iter().enumerate() {
        let ptr = *engine.get(ty).root_ptr();
        sb.root_addrs[idx] = ptr.block_addr;
        sb.root_levels[idx] = ptr.level;
        sb.root_ptrs[idx] = ptr;
    }
}

fn apply_root_mappings(engine: &mut BtreeEngine, mappings: &[(u64, u64)]) {
    if mappings.is_empty() {
        return;
    }

    let mapped: std::collections::HashMap<u64, u64> = mappings.iter().copied().collect();
    for ty in BTREE_ID_NR {
        let current = *engine.get(ty).root_ptr();
        if let Some(&new_addr) = mapped.get(&current.block_addr) {
            let mut updated = current;
            updated.block_addr = new_addr;
            engine.set_root_ptr(ty, updated);
        }
    }
}

fn create_or_load_journal(
    sb: &mut BchSb,
    allocator: &BchAllocator,
    engine: &mut BtreeEngine,
    force_create: bool,
    bucket_count: u32,
) -> Result<Journal, DaemonError> {
    if !force_create {
        if let Some(state) = journal_state_from_superblock(sb) {
            return Ok(Journal::from_superblock(&state));
        }
    }
    let journal = Journal::create(allocator, engine, bucket_count)?;
    sync_superblock_from_journal(sb, &journal);
    Ok(journal)
}

fn journal_bucket_count(total_blocks: u64) -> u32 {
    let total_buckets = (total_blocks / volmount_core::alloc::BLOCKS_PER_BUCKET).max(1);
    total_buckets
        .saturating_div(8)
        .max(1)
        .min(DEFAULT_JOURNAL_BUCKETS as u64) as u32
}

async fn clone_sparse_backend_file(
    source: &std::path::Path,
    dest: &std::path::Path,
) -> Result<(), DaemonError> {
    if try_reflink_clone(source, dest)? {
        return Ok(());
    }

    tokio::fs::copy(source, dest).await?;
    Ok(())
}

fn try_reflink_clone(
    source: &std::path::Path,
    dest: &std::path::Path,
) -> Result<bool, DaemonError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        let source_file = std::fs::OpenOptions::new()
            .read(true)
            .open(source)
            .map_err(DaemonError::Io)?;
        let dest_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(dest)
            .map_err(DaemonError::Io)?;

        let rc = unsafe {
            libc::ioctl(
                dest_file.as_raw_fd(),
                libc::FICLONE,
                source_file.as_raw_fd(),
            )
        };
        if rc == 0 {
            return Ok(true);
        }

        let err = std::io::Error::last_os_error();
        let retryable = matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP)
                | Some(libc::EXDEV)
                | Some(libc::EINVAL)
                | Some(libc::ENOTTY)
                | Some(libc::EPERM)
        );
        if retryable {
            return Ok(false);
        }

        return Err(DaemonError::Io(err));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, dest);
        Ok(false)
    }
}

async fn persist_initial_root_nodes(
    backend: &dyn BlockDevice,
    engine: &mut BtreeEngine,
    sb: &mut BchSb,
) -> Result<(), DaemonError> {
    let root_count = BTREE_ID_NR.len();
    if sb.root_addrs.len() < root_count {
        sb.root_addrs.resize(root_count, 0);
    }
    if sb.root_levels.len() < root_count {
        sb.root_levels.resize(root_count, 0);
    }
    if sb.root_ptrs.len() < root_count {
        sb.root_ptrs.resize(root_count, BtreePtrV2::INVALID);
    }

    let mut root_ptrs = Vec::with_capacity(root_count);
    for (idx, ty) in BTREE_ID_NR.into_iter().enumerate() {
        let block_addr = (idx + 1) as u64;
        root_ptrs.push((ty, block_addr));
    }

    for (idx, (ty, block_addr)) in root_ptrs.into_iter().enumerate() {
        let node = engine.get(ty).root().node.clone();
        let ptr = write_initial_node(node.as_ref(), block_addr, 1, backend)
            .await
            .map_err(DaemonError::Storage)?;

        engine.set_root_ptr(ty, ptr);
        sb.root_addrs[idx] = ptr.block_addr;
        sb.root_levels[idx] = ptr.level;
        sb.root_ptrs[idx] = ptr;
    }

    Ok(())
}

// ─── 列出卷 ───

/// 列出所有块设备（扫描 blocks 目录下的子目录）
pub async fn list_all_volumes(config: &VolmountdConfig) -> Result<Vec<String>, DaemonError> {
    let vol_dir = config.blocks_dir();
    let mut reader = tokio::fs::read_dir(&vol_dir).await?;
    let mut names = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// 从卷目录读取 superblock（用于 daemon API / inspect）
pub async fn read_volume_superblock(
    config: &VolmountdConfig,
    name: &str,
) -> Result<BchSb, DaemonError> {
    let vol_dir = config.blocks_dir().join(name);
    if !vol_dir.exists() {
        return Err(DaemonError::VolumeNotFound(name.to_string()));
    }
    let backend_type = infer_backend_type(config, &vol_dir).await?;
    let backend = create_backend(config, backend_type, &vol_dir, None).await?;
    let sb = BchSb::read_from_backend(&*backend).await?;
    Ok(sb)
}

/// 从卷目录读取当前后端已使用空间
pub async fn read_volume_used_space(
    config: &VolmountdConfig,
    name: &str,
) -> Result<u64, DaemonError> {
    let vol_dir = config.blocks_dir().join(name);
    if !vol_dir.exists() {
        return Err(DaemonError::VolumeNotFound(name.to_string()));
    }
    let backend_type = infer_backend_type(config, &vol_dir).await?;
    let backend = create_backend(config, backend_type, &vol_dir, None).await?;
    Ok(backend.used_space().await?)
}

// ─── 后端创建 ───

/// 根据后端类型创建 BlockDevice
async fn create_backend(
    config: &VolmountdConfig,
    backend_type: BackendType,
    vol_dir: &std::path::Path,
    capacity_bytes: Option<u64>,
) -> Result<Arc<dyn BlockDevice>, DaemonError> {
    let vol_name = vol_dir.file_name().unwrap().to_string_lossy().to_string();
    match backend_type {
        BackendType::Sparse => {
            let backend_path = config.block_backend_path(&vol_name);
            if let Some(parent) = backend_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let sparse_config = volmount_core::block_device::SparseBackendConfig {
                base_path: config.blocks_dir(),
                vol_name,
                file_name: config.backend_file_name.clone(),
                block_size: 4096,
                capacity_bytes,
            };
            let backend = volmount_core::block_device::SparseBackendBlockDevice::new(sparse_config)
                .await
                .map_err(DaemonError::Storage)?;
            Ok(Arc::new(backend))
        }
        BackendType::S3 => {
            let config = S3Config {
                bucket: format!("volmount-{vol_name}"),
                key_prefix: String::new(),
                region: String::from("us-east-1"),
                endpoint_url: None,
                ..Default::default()
            };
            let backend = volmount_core::block_device::S3BlockDevice::new(config)
                .await
                .map_err(DaemonError::Storage)?;
            Ok(Arc::new(backend))
        }
    }
}

async fn infer_backend_type(
    config: &VolmountdConfig,
    vol_dir: &std::path::Path,
) -> Result<BackendType, DaemonError> {
    let backend_path = config.block_backend_path(&vol_dir.file_name().unwrap().to_string_lossy());

    if backend_path.exists() {
        Ok(BackendType::Sparse)
    } else {
        Ok(BackendType::S3)
    }
}

// ─── 错误类型 ───

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("volume '{0}' already exists")]
    VolumeExists(String),

    #[error("volume '{0}' not found")]
    VolumeNotFound(String),

    #[error("NBD error: {0}")]
    Nbd(#[from] volmount_nbd::NbdError),

    #[error("journal error: {0}")]
    Journal(#[from] JournalError),
}

// ─── Tests ───

#[cfg(test)]
mod tests {
    use volmount_core::config::default_http_port;

    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::sync::RwLock;

    fn setup_config() -> (VolmountdConfig, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = VolmountdConfig {
            home_dir: dir.path().to_path_buf(),
            nbd_socket_path: std::path::PathBuf::from("test.sock"),
            auto_exports: vec![],
            http_port: default_http_port(),
            storage: None,
            backend_file_name: "device".to_string(),
        };
        (config, dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_init() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_and_init_sparse_volume() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();

        let vol = create_volume(
            &cfg,
            "test-vol",
            BackendType::Sparse,
            64 * 1024 * 1024,
            4096,
        )
        .await
        .unwrap();
        assert_eq!(vol.meta.vol_name, "test-vol");
        assert_eq!(vol.meta.backend_type, BackendType::Sparse);

        let sb = BchSb::read_from_backend(&*vol.backend).await.unwrap();
        assert_eq!(sb.root_ptrs.len(), BtreeId::count());
        assert!(sb.root_ptrs.iter().all(|ptr| ptr.is_valid()));

        // 直接 re-init：新建卷应能通过 root_ptrs 恢复
        let vol2 = init_volume(&cfg, "test-vol").await.unwrap();
        assert_eq!(vol2.meta.vol_name, "test-vol");

        // stop/drain + re-init（覆盖 clean path）
        stop_volume(&vol).await.unwrap();

        let vol3 = init_volume(&cfg, "test-vol").await.unwrap();
        assert_eq!(vol3.meta.vol_name, "test-vol");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_create_duplicate() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();

        create_volume(&cfg, "dup", BackendType::Sparse, 64 * 1024 * 1024, 4096)
            .await
            .unwrap();
        let result = create_volume(&cfg, "dup", BackendType::Sparse, 64 * 1024 * 1024, 4096).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_volumes() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();

        create_volume(&cfg, "a", BackendType::Sparse, 64 * 1024 * 1024, 4096)
            .await
            .unwrap();
        create_volume(&cfg, "b", BackendType::Sparse, 64 * 1024 * 1024, 4096)
            .await
            .unwrap();

        let all = list_all_volumes(&cfg).await.unwrap();
        assert!(all.contains(&"a".to_string()));
        assert!(all.contains(&"b".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_delete_volume() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();

        create_volume(
            &cfg,
            "to-delete",
            BackendType::Sparse,
            64 * 1024 * 1024,
            4096,
        )
        .await
        .unwrap();
        assert!(list_all_volumes(&cfg)
            .await
            .unwrap()
            .contains(&"to-delete".to_string()));

        delete_volume(&cfg, "to-delete").await.unwrap();
        assert!(!list_all_volumes(&cfg)
            .await
            .unwrap()
            .contains(&"to-delete".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_init_nonexistent() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();
        let result = init_volume(&cfg, "nonexistent").await;
        assert!(result.is_err());
    }

    /// 模拟 daemon 持有 blocks HashMap 的模式（等价 bcachefs 持有 bch_fs 列表）
    #[tokio::test(flavor = "multi_thread")]
    async fn test_bcachefs_lifecycle() {
        let (cfg, _dir) = setup_config();
        init_dirs(&cfg).await.unwrap();

        let blocks: Arc<RwLock<HashMap<String, Arc<Volume>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // bch2_fs_open: 创建或初始化 volume
        let vol = create_volume(&cfg, "mydata", BackendType::Sparse, 64 * 1024 * 1024, 4096)
            .await
            .unwrap();
        blocks.write().await.insert("mydata".to_string(), vol);

        assert!(blocks.read().await.contains_key("mydata"));

        // bch2_fs_stop: stop/drain
        let vol = blocks.read().await.get("mydata").unwrap().clone();
        stop_volume(&vol).await.unwrap();

        // 从 HashMap 移除（等价 bch2_fs_put）
        blocks.write().await.remove("mydata");
        assert!(!blocks.read().await.contains_key("mydata"));
    }
}
