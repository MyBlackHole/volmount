//! Volume — bcachefs 式卷容器
//!
//! 聚合 BtreeEngine（5 种 btree 类型）、BchAllocator、meta 为一个命名实体。
//! Volume 不是 I/O 层——块 I/O 通过 NBD → BlockDevice 直接完成。
//!
//! # 架构
//!
//! ```text
//! Volume = BtreeEngine + BchAllocator + meta（仅聚合，非 I/O 层）
//! ```
//!
//! Volume 是纯聚合容器，不参与任何 I/O（Superblock、root pointers、journal 恢复
//! 均由 daemon 层的 VolumeManager 负责）。详见 `.trellis/spec/architecture.md`

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::alloc::{AllocRequest, BchAllocator, DedicatedWp, WritePointSpecifier};
use crate::block_device::BlockDevice;

use crate::btree::transaction::BtreeTrans;
use crate::btree::trigger::TriggerRegistry;
use crate::btree::{
    BchVal, Bpos, BtreeEngine, BtreeEntry, BtreeId, BtreeKey, BtreeNode, WritebackHandle,
    BTREE_ID_NR,
};
use crate::journal::Journal;
use crate::meta::VolumeMeta;
use crate::snap::snapshot::{
    bch2_snapshot_node_create, bch2_snapshot_node_set_deleted, create_root_snapshot_btree,
    list_snapshots_from_btree, read_snapshot_value,
};
use crate::subvol::{
    bch2_subvolume_create, bch2_subvolume_delete, bch2_subvolume_get, bch2_subvolumes_reparent,
};
use crate::types::{BlockAddr, StorageError, Watermark};

/// 默认块大小 (4KB)
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// 默认卷容量 (1GB)
pub const DEFAULT_CAPACITY: u64 = 1024 * 1024 * 1024;

/// Volume 配置
#[derive(Debug, Clone)]
pub struct VolumeConfig {
    /// 卷名
    pub vol_name: String,
    /// 池名
    pub pool_name: String,
    /// 逻辑块大小（默认 4096）
    pub block_size: u32,
    /// 卷容量（字节，默认 1GB）
    pub capacity: u64,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self {
            vol_name: "default".into(),
            pool_name: "default".into(),
            block_size: DEFAULT_BLOCK_SIZE,
            capacity: DEFAULT_CAPACITY,
        }
    }
}

/// 卷生命周期状态（对应 bcachefs BCH_FS_* 标志位系统）
///
/// 命名对齐 bcachefs 风格：
///   - `New` ↔ BCH_FS_new_fs        — 新创建
///   - `Rw`  ↔ BCH_FS_rw             — 可读写（bcachefs 用 rw 而非 "running"）
///   - `Error` ↔ BCH_FS_error
///   - `Stopping` ↔ BCH_FS_stopping
///   - `Stopped` ↔ BCH_FS_clean_shutdown
///
/// 状态转换顺序：New → Starting → Rw → Stopping → Stopped
/// 从任何非终止状态可转入 Error。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VolumeState {
    /// 卷已创建但未启动（BCH_FS_new_fs）
    New = 0,
    /// 卷正在启动（恢复/初始化中）
    Starting = 1,
    /// 卷正常运行中，可读写（BCH_FS_rw）
    Rw = 2,
    /// 卷可读写但后台恢复尚未完成（Rw 子状态）
    ///
    /// 对应 bcachefs BCH_FS_rw 下仍有 recovery passes 在执行的场景。
    /// 恢复完成后应转回 Rw；恢复失败则转入 Error。
    RwWithPendingRecovery = 6,
    /// 卷处于错误状态（非终止，可尝试恢复；BCH_FS_error）
    Error = 3,
    /// 卷正在关闭（BCH_FS_stopping）
    Stopping = 4,
    /// 卷已停止，终止状态（BCH_FS_clean_shutdown）
    Stopped = 5,
}

/// Volume 统计信息
#[derive(Debug, Clone)]
pub struct VolumeStats {
    pub block_size: u32,
    pub capacity: u64,
    pub total_blocks: u64,
    pub allocated_blocks: u64,
    pub mapping_entries: usize,
    pub btree_keys: u32,
    pub snapshot_count: usize,
    pub snapshot_tree_depth: usize,
}

/// volmount 统一卷
///
/// 聚合 BtreeEngine（5 种 btree 类型）、BchAllocator 为一个命名实体。
/// Volume 是纯聚合容器，不参与任何 I/O。
/// Superblock / root pointers / journal 状态的读写由 daemon 层的 VolumeManager 负责。
/// 崩溃恢复由 Journal 模块独立处理，Volume 不参与恢复路径。
pub struct Volume {
    /// 卷元数据
    meta: VolumeMeta,
    /// 块存储后端
    backend: Arc<dyn BlockDevice>,
    /// 卷级唯一 Journal（与 core volume 同生命周期）
    journal: Arc<Journal>,
    /// 多实例元数据 btree（按 BtreeId 路由）
    engine: BtreeEngine,
    /// 根快照 ID（初始化时由 create_root_snapshot_btree 分配）
    root_snapshot_id: u32,
    /// 块分配器
    allocator: BchAllocator,
    /// 触发器注册表（Phase C2: Alloc extent trigger）
    trigger_registry: Arc<TriggerRegistry>,
    /// 卷级 writeback 协调器
    writeback: Mutex<Option<Arc<WritebackHandle>>>,
    /// 配置
    #[allow(dead_code)]
    config: VolumeConfig,
    /// 生命周期状态（通过 AtomicU8 实现无锁并发访问）
    state: AtomicU8,

    // ──── 恢复跟踪（Phase 3: recovery passes 状态） ────
    /// 已完成的最高恢复 pass 索引
    recovery_pass_done: AtomicU8,
    /// 恢复 pass 完成位掩码（bit i = 1 表示 pass i 已完成）
    recovery_passes_complete: AtomicU64,
    /// 失败 pass 位掩码（bit i = 1 表示 pass i 失败）
    passes_failing: AtomicU64,

    // ──── 错误计数（Phase 3: 统计） ────
    /// 总错误计数（累积所有 recoverable 错误）
    error_count: AtomicU64,
    /// fsck 错误聚合计数器
    fsck_error: AtomicU64,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("vol_name", &self.meta.vol_name)
            .field("block_size", &self.meta.block_size)
            .field("capacity", &self.meta.capacity)
            .field(
                "btree_keys",
                &BTREE_ID_NR
                    .iter()
                    .map(|ty| self.engine.get(*ty).key_count())
                    .sum::<u32>(),
            )
            .field("snapshots", &list_snapshots_from_btree(&self.engine).len())
            .finish()
    }
}

impl Volume {
    // ──── 构造器 ────

    /// 创建一个新的 Volume（纯构造器，无 I/O）
    ///
    /// 所有组件由调用方预先构造好再注入，Volume 只做聚合。
    /// daemon 层的 VolumeManager 负责所有 I/O 操作（Superblock 读写、
    /// root pointers 回写、journal 恢复等）。
    pub fn new(
        meta: VolumeMeta,
        backend: Arc<dyn BlockDevice>,
        journal: Arc<Journal>,
        engine: BtreeEngine,
        root_snapshot_id: u32,
        allocator: BchAllocator,
        trigger_registry: Arc<TriggerRegistry>,
        config: VolumeConfig,
    ) -> Self {
        let _ = engine.set_backend(backend.clone());
        Self {
            meta,
            backend,
            journal,
            engine,
            root_snapshot_id,
            allocator,
            trigger_registry,
            writeback: Mutex::new(None),
            config,
            state: AtomicU8::new(VolumeState::New as u8),
            recovery_pass_done: AtomicU8::new(0),
            recovery_passes_complete: AtomicU64::new(0),
            passes_failing: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            fsck_error: AtomicU64::new(0),
        }
    }

    // ──── 生命周期状态机 ────

    /// 返回当前状态
    pub fn state(&self) -> VolumeState {
        match self.state.load(Ordering::Acquire) {
            0 => VolumeState::New,
            1 => VolumeState::Starting,
            2 => VolumeState::Rw,
            3 => VolumeState::Error,
            4 => VolumeState::Stopping,
            5 => VolumeState::Stopped,
            6 => VolumeState::RwWithPendingRecovery,
            _ => VolumeState::Error,
        }
    }

    /// 启动卷：New → Starting, 完成后设为 Rw
    ///
    /// 对应 bcachefs bch2_fs_start()。
    /// 如果卷不在 New 状态则返回 AlreadyExists 错误。
    /// 调用方在成功转换到 Starting 后应执行恢复/初始化流程，
    /// 完成后调用 go_rw() 完成状态转换。
    pub fn start(&self) -> Result<(), StorageError> {
        self.state
            .compare_exchange(
                VolumeState::New as u8,
                VolumeState::Starting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| StorageError::AlreadyExists("volume.start"))
    }

    /// 将状态从 Starting 推进到 Rw（启动流程完成后调用）
    ///
    /// 对应 bcachefs bch2_fs_go_rw() / BCH_FS_rw 标志位置位。
    pub fn go_rw(&self) -> Result<(), StorageError> {
        self.state
            .compare_exchange(
                VolumeState::Starting as u8,
                VolumeState::Rw as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| StorageError::AlreadyExists("volume.go_rw"))
    }

    /// 停止卷：Rw → Stopping → Stopped
    ///
    /// 对应 bcachefs bch2_fs_read_only() / bch2_fs_stop()。
    /// 调用方在成功转换到 Stopping 后应执行关闭/刷出流程，
    /// 完成后调用 set_stopped() 完成终止。
    pub fn stop(&self) -> Result<(), StorageError> {
        self.state
            .compare_exchange(
                VolumeState::Rw as u8,
                VolumeState::Stopping as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| StorageError::AlreadyExists("volume.stop"))
    }

    /// 将状态从 Stopping 推进到 Stopped（关闭流程完成后调用）
    pub fn set_stopped(&self) -> Result<(), StorageError> {
        self.state
            .compare_exchange(
                VolumeState::Stopping as u8,
                VolumeState::Stopped as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| StorageError::AlreadyExists("volume.set_stopped"))
    }

    /// 卷是否处于读写状态（Rw 或 RwWithPendingRecovery, BCH_FS_rw）
    pub fn is_rw(&self) -> bool {
        let s = self.state.load(Ordering::Acquire);
        s == VolumeState::Rw as u8 || s == VolumeState::RwWithPendingRecovery as u8
    }

    /// 将卷置为错误状态
    ///
    /// 从任何非终止状态（非 Stopped）均可转入 Error。
    /// 终止状态（Stopped）不能转入 Error。
    pub fn set_error(&self) {
        loop {
            let current = self.state.load(Ordering::Acquire);
            if current == VolumeState::Stopped as u8 {
                return;
            }
            if current == VolumeState::Error as u8 {
                return;
            }
            if self
                .state
                .compare_exchange(
                    current,
                    VolumeState::Error as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    // ──── 恢复跟踪 ────

    /// 返回恢复进度：(pass_done, passes_complete_bitmask, passes_failing_bitmask)
    pub fn recovery_progress(&self) -> (u8, u64, u64) {
        (
            self.recovery_pass_done.load(Ordering::Acquire),
            self.recovery_passes_complete.load(Ordering::Acquire),
            self.passes_failing.load(Ordering::Acquire),
        )
    }

    /// 设置恢复进度
    pub fn set_recovery_progress(&self, pass_done: u8, passes_complete: u64, passes_failing: u64) {
        self.recovery_pass_done.store(pass_done, Ordering::Release);
        self.recovery_passes_complete
            .store(passes_complete, Ordering::Release);
        self.passes_failing.store(passes_failing, Ordering::Release);
    }

    // ──── 错误计数 ────

    /// 记录一个可恢复错误，递增总错误计数
    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Release);
    }

    /// 返回总错误计数
    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Acquire)
    }

    /// 记录一个 fsck 错误
    pub fn record_fsck_error(&self) {
        self.fsck_error.fetch_add(1, Ordering::Release);
    }

    /// 返回 fsck 错误总数
    pub fn fsck_error_count(&self) -> u64 {
        self.fsck_error.load(Ordering::Acquire)
    }

    // ──── 组件访问器（供 daemon 层进行 I/O 操作时使用） ────

    /// 获取 BtreeEngine 引用
    pub fn engine(&self) -> &BtreeEngine {
        &self.engine
    }

    /// 获取可变 BtreeEngine 引用
    pub fn engine_mut(&mut self) -> &mut BtreeEngine {
        &mut self.engine
    }

    /// 获取 BchAllocator 引用
    pub fn allocator(&self) -> &BchAllocator {
        &self.allocator
    }

    /// 获取可变 BchAllocator 引用
    pub fn allocator_mut(&mut self) -> &mut BchAllocator {
        &mut self.allocator
    }

    /// 同时获取 `&mut BtreeEngine` 和 `&BchAllocator`
    ///
    /// 用于 daemon 层 close_volume 中调用 `write_data_to_blocks`。
    /// 相比分别调用 `engine_mut()` + `allocator()`，此方法避免了 Rust
    /// 借用检查器对同一 `&mut Self` 上两个方法的组合限制。
    pub fn engine_mut_and_allocator(&mut self) -> (&mut BtreeEngine, &BchAllocator) {
        (&mut self.engine, &self.allocator)
    }

    /// 获取根快照 ID
    pub fn root_snapshot_id(&self) -> u32 {
        self.root_snapshot_id
    }

    // ──── Btree 查询 ────

    /// 从 Extents btree 获取给定 vaddr+snapshot 对应的 extent
    ///
    /// Δ1 MVP: 使用 `Bpos(vol_id, vaddr, snapshot_id)` 精确查询。
    /// Δ2 会加入 `BtreeIter::init_with_snapshot` + `peek_visible` 的快照可见性过滤。
    pub fn get_extent_for_snapshot(
        &self,
        vaddr: u64,
        snapshot_id: u32,
    ) -> Option<crate::btree::BtreeEntry> {
        let bpos = Bpos::new(0, vaddr, snapshot_id);
        self.engine.get_entry_raw(BtreeId::Extents, bpos)
    }

    // ──── 快照操作 ────

    /// 创建快照
    ///
    /// 在 Snapshots btree 中创建根快照的子快照节点。
    /// Δ2: 使用 btree 原生快照操作替代 SnapshotTreeManager + SnapshotManager。
    pub fn create_snapshot(&mut self, _description: &str) -> Result<u32, StorageError> {
        bch2_snapshot_node_create(&mut self.engine, self.root_snapshot_id, 0, None)
    }

    /// 获取快照列表
    ///
    /// 从 Snapshots btree 查询所有活跃快照。
    /// Δ2: 使用 btree 原生快照列表替代 SnapshotManager。
    pub fn list_snapshots(&self) -> Vec<crate::snap::SnapshotMeta> {
        list_snapshots_from_btree(&self.engine)
            .into_iter()
            .map(|(id, val)| crate::snap::SnapshotMeta::from_value(id, &val))
            .collect()
    }

    /// 回滚到快照
    ///
    /// Δ2: 验证快照在 Snapshots btree 中存在且未被删除。真正的数据回滚
    /// 通过 BtreeIter::init_with_snapshot + peek_visible 的可见性过滤实现。
    pub fn rollback(&mut self, snap_id: u32) -> Result<(), StorageError> {
        let snap = read_snapshot_value(&self.engine, snap_id)
            .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", snap_id)))?;
        if snap.deleted {
            return Err(StorageError::NotFound(format!(
                "snapshot {} has been deleted",
                snap_id
            )));
        }
        Ok(())
    }

    /// 删除快照
    ///
    /// 在 Snapshots btree 中标记快照为已删除。
    /// Δ2: 使用 btree 原生删除操作替代 SnapshotManager。
    pub fn delete_snapshot(&mut self, snap_id: u32) -> Result<(), StorageError> {
        bch2_snapshot_node_set_deleted(&mut self.engine, snap_id)
    }

    // ──── 子卷操作 ────

    /// 创建新子卷
    ///
    /// 1. 在 Snapshots btree 中创建根快照节点
    /// 2. 在 Subvolumes btree 中创建子卷条目
    /// Δ2: 使用 btree 原生快照操作替代 SnapshotTreeManager
    pub fn create_subvol(&mut self, _name: &str, size: u64) -> Result<u32, StorageError> {
        // 委托给 bch2_subvolume_create()，其内部自动创建根快照节点 + 更新 subvol 指针
        bch2_subvolume_create(&mut self.engine, 0, size, 0)
    }

    /// 删除子卷
    ///
    /// 1. reparent_children：子卷的子卷重挂到 0（无父）
    /// 2. 标记子卷为 UNLINKED
    /// 3. 标记 snapshot node 为 WILL_DELETE
    /// Δ2: 使用 btree 原生快照操作替代 SnapshotTreeManager
    pub fn delete_subvol(&mut self, subvol_id: u32) -> Result<(), StorageError> {
        // 1. 重挂子卷到根
        bch2_subvolumes_reparent(&mut self.engine, subvol_id, 0)?;

        // 2. 标记子卷 UNLINKED
        bch2_subvolume_delete(&mut self.engine, subvol_id)?;

        // 3. 标记 snapshot node 为 WILL_DELETE
        if let Some(sv) = bch2_subvolume_get(&self.engine, subvol_id) {
            let snap_id = sv.snapshot;
            if let Some(mut snap_val) = read_snapshot_value(&self.engine, snap_id) {
                snap_val
                    .flags
                    .insert(crate::snap::BchSnapshotFlags::WILL_DELETE);
                let bytes = bincode::serialize(&snap_val).map_err(StorageError::Serialization)?;
                let entry = crate::btree::key::BtreeEntry::raw(
                    crate::btree::Bpos::new(0, 0, snap_id),
                    crate::btree::key::KeyType::Normal,
                    bytes,
                );
                self.engine
                    .insert_entry_raw(crate::btree::BtreeId::Snapshots, entry, 0);
            }
        }

        Ok(())
    }

    /// 列出所有活跃子卷
    pub fn list_subvols(&self) -> Vec<(u32, crate::subvol::BchSubvolume)> {
        crate::subvol::bch2_subvolume_list(&self.engine)
    }

    // ──── 元数据操作 ────

    /// 在 Extents btree 中插入元数据 key-value
    ///
    /// 使用 insert_guarded（Phase 4 bcachefs 对齐），
    /// 正常运行时 overlay 已 drain，等价于直写。
    pub fn btree_insert(&mut self, key: BtreeKey, value: BchVal) -> bool {
        let pos = Bpos::from_key(&key);
        let entry = BtreeEntry::new(pos, key.key_type, value.into());
        self.engine.insert_guarded(BtreeId::Extents, entry, 0)
    }

    /// 通过 journal WAL 写入元数据 key-value（crash-safe）
    ///
    /// 使用 BtreeTrans + trans_commit 走完整 journal 流程（reserve → modify → fill → release），
    /// 确保写入操作被 WAL 保护。返回 journal_seq 可用于追踪或 pin 管理。
    ///
    /// 与 `btree_insert()` 的区别：后者是轻量同步版本（不走 journal，直接 `insert_guarded`），
    /// 适用于不需要 crash-safe 的场景。
    pub async fn btree_insert_with_journal(
        &mut self,
        key: BtreeKey,
        value: BchVal,
    ) -> Result<u64, StorageError> {
        let mut trans = BtreeTrans::default();
        trans.set_trigger_registry(self.trigger_registry.clone());
        trans.begin();
        trans.journal_insert(BtreeId::Extents, 0, false, key, value, 0);
        let seq = trans
            .trans_commit(&self.journal, &mut self.engine, &*self.backend)
            .await
            .map_err(|e| StorageError::JournalError(e.to_string()))?;
        Ok(seq)
    }

    /// 从 Extents btree 查询元数据
    pub fn btree_get(&self, key: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        self.engine.get_entry(BtreeId::Extents, key)
    }

    // ──── Phase C2: Extent 写入/删除（触发 Alloc btree 同步） ────

    /// 写入 extent（分配 bucket + btree insert + 触发 Alloc 同步）
    pub async fn write_extent(&mut self, key: BtreeKey, buf: &[u8]) -> Result<(), StorageError> {
        // 1. 分配 bucket
        let vaddr = key.vaddr; // copy field to avoid unaligned reference on packed struct
        let request = AllocRequest::new(Watermark::Normal, crate::alloc::BchDataType::User);
        let paddr = self.allocator.bch2_bucket_alloc_new_fs(
            &mut self.engine,
            &request,
            Some(WritePointSpecifier::Hashed(vaddr)),
        )?;

        // 2. 写数据到后端（带 CRC32C 校验和）
        let block_addr = BlockAddr::new(paddr);
        let csum = self.backend.write_block_with_csum(block_addr, buf).await?;
        tracing::debug!(
            "write_extent: vaddr={:#x} paddr={:#x} len={} csum={:#010x}",
            vaddr,
            paddr,
            buf.len(),
            csum,
        );

        // 3. BtreeTrans 插入 Extents（触发 alloc_extent_trigger → Alloc btree）
        let value = BchVal::new(paddr, 0);
        let mut trans = BtreeTrans::default();
        trans.set_trigger_registry(self.trigger_registry.clone());
        trans.begin();
        trans.journal_insert(BtreeId::Extents, 0, false, key, value, 0);
        let _journal_seq = trans
            .trans_commit(&self.journal, &mut self.engine, &*self.backend)
            .await
            .map_err(|e| StorageError::JournalError(e.to_string()))?;

        // Phase 2 btree 修改已由 trans_commit 内部完成（TC1+TC2），
        // 不再需要手动 drain_journal() + engine.insert_entry() 循环。

        Ok(())
    }

    /// 删除 extent（btree delete + 触发 Alloc 同步 + 释放 bucket）
    pub async fn delete_extent(&mut self, key: &BtreeKey) -> Result<(), StorageError> {
        // 1. 获取旧 extent 中的 paddr
        let old_entry = self
            .engine
            .get_entry(BtreeId::Extents, key)
            .ok_or_else(|| StorageError::NotFound("extent not found".into()))?;
        let paddr = old_entry.1.paddr.get();

        // 2. BtreeTrans 从 Extents 删除（触发 alloc_extent_trigger → Alloc btree）
        let mut trans = BtreeTrans::default();
        trans.set_trigger_registry(self.trigger_registry.clone());
        trans.begin();
        trans.journal_delete(BtreeId::Extents, 0, false, *key, 0);
        let _journal_seq = trans
            .trans_commit(&self.journal, &mut self.engine, &*self.backend)
            .await
            .map_err(|e| StorageError::JournalError(e.to_string()))?;

        // Phase 2 btree 修改已由 trans_commit 内部完成（TC1+TC2），
        // 不再需要手动 drain_journal() + engine.insert_entry() 循环。

        // 4. 释放 bucket 回分配器（P1.1: 同步 Alloc btree）
        self.allocator.bch2_bucket_free(paddr, &mut self.engine)?;

        Ok(())
    }

    // ──── 统计信息 ────

    /// 获取 Volume 统计信息
    pub fn stats(&self) -> VolumeStats {
        VolumeStats {
            block_size: self.meta.block_size,
            capacity: self.meta.capacity,
            total_blocks: self.allocator.total_blocks(),
            allocated_blocks: self.allocator.allocated_blocks(),
            mapping_entries: self.engine.get(BtreeId::Extents).key_count() as usize,
            btree_keys: BTREE_ID_NR
                .iter()
                .map(|ty| self.engine.get(*ty).key_count())
                .sum::<u32>(),
            snapshot_count: list_snapshots_from_btree(&self.engine).len(),
            snapshot_tree_depth: 0, // Δ2: depth 需要遍历 btree 计算，延迟优化
        }
    }

    /// 获取 VolumeMeta 引用
    pub fn meta(&self) -> &VolumeMeta {
        &self.meta
    }

    /// 绑定卷级 writeback coordinator。
    pub fn set_writeback_handle(&self, writeback: Arc<WritebackHandle>) -> bool {
        let ok = self.engine.set_writeback_handle(writeback.clone());
        *self.writeback.lock().unwrap() = Some(writeback);
        ok
    }

    /// 收集所有 btree type 中的脏节点，序列化并写入后端
    ///
    /// 流程：
    /// 1. `flush_dirty_nodes()` 从所有 btree 的 cache 中 drain 脏节点
    /// 2. 对每个脏节点：allocate_blocks → serialize_to_bucket → write_block
    ///
    /// 返回 `Vec<(node_id, block_addr)>` 映射，供调用方更新块引用。
    pub async fn flush_dirty_nodes(&mut self) -> Result<Vec<(u64, u64)>, StorageError> {
        let per_type = self.engine.flush_dirty_nodes();
        let mut mappings = Vec::new();

        // 每个 btree type 的脏节点已在 cache 层按 level 升序排列（叶子先于内层节点）。
        // 这里再做一次跨 type 的全局排序作为防御性措施（排序已排序数据的开销可忽略）。
        let mut nodes: Vec<(BtreeId, u64, Arc<BtreeNode>)> = per_type
            .into_iter()
            .flat_map(|(ty, list)| list.into_iter().map(move |(id, node)| (ty, id, node)))
            .collect();
        nodes.sort_by_key(|(_, _, node)| node.level);

        for (_ty, node_id, mut node) in nodes {
            let req = AllocRequest::new(Watermark::Btree, crate::alloc::BchDataType::Btree);
            let block_addr = self.allocator.bch2_alloc_sectors_start_trans(
                1,
                &mut self.engine,
                &req,
                Some(WritePointSpecifier::Direct(DedicatedWp::BTree)),
            )?;
            let bytes = node.serialize_to_bucket(block_addr)?;
            self.backend
                .write_block(BlockAddr::new(block_addr), &bytes)
                .await?;
            // bcachefs 对齐：节点已落盘 → 清除 will_make_reachable
            node.clear_will_make_reachable();
            // 写入后清理：just_written 清除 + 多 bset 合并 + aux 树重建
            // 注意：节点已从 cache 中移除，refcount 应为 1，Arc::get_mut 安全
            if let Some(n) = Arc::get_mut(&mut node) {
                crate::btree::io::bch2_btree_post_write_cleanup(n);
            }
            mappings.push((node_id, block_addr));
        }

        // 节点级 pin 在 cache eviction 时自动释放，此处无需额外 pin 管理
        Ok(mappings)
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        // Volume 生命周期由 daemon 层管理
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::KeyType;
    use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
    use crate::types::BackendType;

    fn test_backend() -> Arc<dyn BlockDevice> {
        Arc::new(MockBlockDevice::new())
    }

    /// 构造一个测试用的 Volume（纯内存，无 I/O）
    fn make_volume(backend: Arc<dyn BlockDevice>, name: &str, capacity: u64) -> Volume {
        let meta = VolumeMeta::new(
            name.into(),
            1,
            "default".into(),
            DEFAULT_BLOCK_SIZE,
            capacity,
            BackendType::Sparse,
        );
        let total_blocks = capacity / DEFAULT_BLOCK_SIZE as u64;
        let group_size = 1024u64.min(total_blocks / 4).max(256);
        let allocator = BchAllocator::new(total_blocks, group_size, 8);
        let mut engine = BtreeEngine::new();
        let root_snapshot_id = create_root_snapshot_btree(&mut engine, 0).unwrap();
        let journal = Arc::new(
            Journal::create(
                &allocator,
                &mut engine,
                crate::journal::DEFAULT_JOURNAL_BUCKETS,
            )
            .unwrap(),
        );
        let mut registry = TriggerRegistry::new();
        registry.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            crate::alloc::bch2_trigger_extent,
        );
        registry.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            crate::alloc::bch2_trigger_gc,
        );
        let config = VolumeConfig {
            vol_name: name.into(),
            pool_name: "default".into(),
            block_size: DEFAULT_BLOCK_SIZE,
            capacity,
        };
        Volume::new(
            meta,
            backend,
            journal,
            engine,
            root_snapshot_id,
            allocator,
            Arc::new(registry),
            config,
        )
    }

    #[tokio::test]
    async fn test_volume_new() {
        let backend = test_backend();
        let vol = make_volume(backend, "test-vol", DEFAULT_CAPACITY);
        assert_eq!(vol.meta.vol_name, "test-vol");
        assert_eq!(vol.meta.block_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(vol.engine.get(BtreeId::Extents).key_count(), 0);
    }

    #[tokio::test]
    async fn test_volume_create_snapshot() {
        let backend = test_backend();
        let mut vol = make_volume(backend, "test", DEFAULT_CAPACITY);

        let snap_id = vol.create_snapshot("first-snap").unwrap();
        assert!(snap_id > 0);

        let snapshots = vol.list_snapshots();
        assert_eq!(snapshots.len(), 2, "root snapshot + created child");
    }

    #[tokio::test]
    async fn test_volume_rollback() {
        let backend = test_backend();
        let mut vol = make_volume(backend, "test", DEFAULT_CAPACITY);

        let snap_id = vol.create_snapshot("before-change").unwrap();

        // 回滚到有效快照
        vol.rollback(snap_id).unwrap();
    }

    #[tokio::test]
    async fn test_volume_stats() {
        let backend = test_backend();
        let mut vol = make_volume(backend, "test", DEFAULT_CAPACITY);

        vol.create_snapshot("test").unwrap();

        let stats = vol.stats();
        assert_eq!(stats.block_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(stats.snapshot_count, 2, "root snapshot + created child");
    }

    #[tokio::test]
    async fn test_snapshot_delete_then_list() {
        let backend = test_backend();
        let mut vol = make_volume(backend, "test", DEFAULT_CAPACITY);

        let s1 = vol.create_snapshot("first").unwrap();
        let s2 = vol.create_snapshot("second").unwrap();
        let _s3 = vol.create_snapshot("third").unwrap();

        vol.delete_snapshot(s2).unwrap();

        let snaps = vol.list_snapshots();
        // 根快照 + s1 + s3 = 3, 不含 s2
        assert_eq!(snaps.len(), 3);
        let ids: Vec<u32> = snaps.iter().map(|s| s.id).collect();
        assert!(ids.contains(&s1));
        assert!(!ids.contains(&s2));
    }

    #[tokio::test]
    async fn test_rollback_to_deleted_snapshot() {
        let backend = test_backend();
        let mut vol = make_volume(backend, "test", DEFAULT_CAPACITY);

        let snap_id = vol.create_snapshot("to-delete").unwrap();
        vol.delete_snapshot(snap_id).unwrap();
        let result = vol.rollback(snap_id);
        assert!(result.is_err());
    }
}
