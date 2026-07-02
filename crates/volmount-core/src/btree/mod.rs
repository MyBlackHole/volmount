pub mod btree;
pub mod bucket_io;
pub mod cache;
pub mod gc;
pub mod interior;
pub mod io;
pub mod iter;
pub mod key;
pub mod key_cache;
pub mod node;
pub mod node_scan;
pub mod op;
pub mod search;
pub mod transaction;
pub mod trigger;
pub mod types;
pub(crate) mod update;
pub mod write_buffer;
pub mod writeback;

use std::collections::HashMap;
use std::sync::Arc;

pub use btree::Btree;
pub use cache::{
    bch2_btree_node_mem_free, bch2_btree_node_transition_state,
    bch2_btree_node_transition_state_locked, bch2_btree_node_write_done_clean, BtreeCache,
    BTREE_FOREGROUND_MERGE_HIGHER, BTREE_FOREGROUND_MERGE_HYSTERESIS,
    BTREE_FOREGROUND_MERGE_THRESHOLD, BTREE_SPLIT_THRESHOLD, BTREE_WRITE_IO_LIMIT, MAX_CLEAN,
    MAX_DIRTY,
};
pub use gc::{
    bch2_check_allocations, bch2_check_topology, bch2_fs_btree_gc_init_early, bch2_gc_alloc_done,
    bch2_gc_alloc_start, bch2_gc_btrees, bch2_gc_gens, bch2_gc_gens_async, bch2_gc_mark_key,
    bch2_gc_pos_from_sb, bch2_gc_pos_to_sb, bch2_gc_pos_to_text, bch2_presplit_shard_boundaries,
    gc_phase, gc_pos_btree, gc_pos_cmp, gc_visited, BtreeGc, GcPhase, GcPos,
};
pub use io::{
    __bch2_btree_node_write, bch2_btree_cancel_all_writes, bch2_btree_flush_all_reads,
    bch2_btree_flush_all_writes, bch2_btree_init_next, bch2_btree_node_io_lock,
    bch2_btree_node_io_unlock, bch2_btree_node_read, bch2_btree_node_read_done,
    bch2_btree_node_wait_on_read, bch2_btree_node_wait_on_write, bch2_btree_node_write,
    bch2_btree_node_write_trans, bch2_btree_post_write_cleanup, bch2_btree_root_read,
    bch2_validate_bset, btree_node_write_if_need,
};
pub use iter::BtreeIter;
pub use key::KEY_TYPE_BTREE_PTR_V3;
pub use key::{Addr48, BchVal, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
pub use key_cache::KeyCache;
pub use node::BtreeNode;
pub use node::{
    bch2_btree_node_iter_advance, bch2_btree_node_iter_init, bch2_btree_node_iter_init_from_start,
    bch2_btree_node_iter_next_all, bch2_btree_node_iter_peek, bch2_btree_node_iter_peek_all,
    bch2_btree_node_iter_set_drop, bch2_btree_node_iter_sort,
};
pub use node::{bset, bset_u64s, btree_bset_first, btree_bset_last, for_each_bset};
pub use node::{BsetAuxTreeType, BtreeNodeIter, BtreeNodeIterSet, BSET_CACHELINE, MAX_BSETS};
pub use node::{NODE_ACCESSED, NODE_NEED_REWRITE};
pub use node_scan::{
    bch2_btree_has_scanned_nodes, bch2_btree_node_is_stale, bch2_find_btree_nodes_exit,
    bch2_find_btree_nodes_init, bch2_found_btree_node_to_text, bch2_get_scanned_nodes,
    bch2_scan_for_btree_nodes, FindBtreeNodes, FoundBtreeNode,
};
pub use transaction::BtreeTrans;
pub use trigger::{TriggerFn, TriggerPhase, TriggerRegistry};
pub use types::{
    BtreeNodeLockedType, BtreePathLevel, BtreePtrV2, BtreeRoot, NodeCache, BTREE_MAX_DEPTH,
};
pub use write_buffer::{
    bch2_btree_write_buffer_flush_going_ro, bch2_btree_write_buffer_flush_sync,
    bch2_btree_write_buffer_maybe_flush, bch2_btree_write_buffer_must_wait,
    bch2_btree_write_buffer_resize, bch2_btree_write_buffer_start, bch2_btree_write_buffer_stop,
    bch2_btree_write_buffer_tryflush, bch2_fs_btree_write_buffer_exit,
    bch2_fs_btree_write_buffer_init, bch2_fs_btree_write_buffer_init_early, bch2_journal_key_to_wb,
    bch2_journal_keys_to_write_buffer_end, bch2_journal_keys_to_write_buffer_start,
    bch_wb_btree_idx, btree_write_buffer_new, wb_key_cmp, BchWbBtree, BtreeWriteBuffer,
    BtreeWriteBufferKeys, BtreeWriteBufferSet, BtreeWriteBufferedKey, JournalKeysToWb,
    WbFlushCaller,
};
pub use writeback::WritebackHandle;

use crate::block_device::BlockDevice;
use crate::recovery::JournalKeys;
use crate::StorageError;

// ---------------------------------------------------------------------------
// BtreeId — bcachefs 对齐的多 btree 架构
// ---------------------------------------------------------------------------

/// 每个 btree 实例处理一种元数据类型（受 bcachefs `enum btree_id` 启发）。
///
/// 所有 type 共享相同的 `Btree` 实现，但各自拥有独立的根节点和 key 空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BtreeId {
    /// 数据 extent 映射：bpos(vol_id, lba, snapshot) -> BchVal
    Extents,
    /// 子卷记录：bpos(subvol_id, 0, snapshot) -> Subvolume
    Subvolumes,
    /// 快照树节点：bpos(snapshot_id, 0, 0) -> SnapshotNode
    Snapshots,
    /// 快照树元信息：bpos(tree_id, 0, 0) -> SnapshotTree
    SnapshotTrees,
    /// 空间分配状态：bpos(bucket_index, 0, 0) -> BchAllocEntry
    Alloc,
    /// 空闲 bucket 索引：bpos(0, bucket_index, gen) -> empty value
    ///
    /// 对应 bcachefs BTREE_ID_freespace。由 Alloc btree trigger 自动维护：
    /// - bucket 变为 Free → insert
    /// - bucket 变为 Allocated → delete
    Freespace,
    /// bucket generation 索引：bpos(device, chunk, 0) -> BchBucketGens
    BucketGens,
}

// ---------------------------------------------------------------------------
// BtreeNodeType — bcachefs 对齐的节点类型描述符
// ---------------------------------------------------------------------------

/// btree 节点类型 — 对应 bcachefs `enum btree_node_type`
///
/// 用于在 commit.c / update.c 路径中区分 internal node（包含 btree 指针）
/// 和 leaf node（包含对应 btree 的数据 key）。
///
/// bcachefs 布局（types.h:1238）：
/// ```c
/// enum btree_node_type {
///     BKEY_TYPE_btree,           // = 0 (internal nodes)
///     BKEY_TYPE_extents = 1,     // leaf: extents
///     BKEY_TYPE_alloc = 2,       // leaf: alloc
///     ...
/// };
/// ```
/// volmount 中 internal node 统一使用 `BTREE_NODE_TYPE_BTREE_PTR = 0`，
/// leaf node 映射为 `(BtreeId as u8) + 1`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BtreeNodeType {
    /// internal 节点：包含 KEY_TYPE_BTREE_PTR_V3 类型条目
    BtreePtr = 0,
    /// leaf 节点：对应 btree 的数据 key
    BtreeData(u8),
}

/// 计算指定 btree 节点应包含的 key type。
///
/// 对应 bcachefs `__btree_node_type(level, btree_id)`（bkey_methods.h:1247）：
/// - level > 0（internal nodes）→ `BKEY_TYPE_btree`（btree pointer entries）
/// - level == 0（leaf nodes）→ `(enum btree_id) + 1`
pub fn btree_node_type(level: u8, btree_id: BtreeId) -> BtreeNodeType {
    if level > 0 {
        BtreeNodeType::BtreePtr
    } else {
        BtreeNodeType::BtreeData(btree_id as u8 + 1)
    }
}

impl BtreeId {
    /// 返回 type 对应的描述性名称
    pub const fn name(self) -> &'static str {
        match self {
            BtreeId::Extents => "extents",
            BtreeId::Subvolumes => "subvolumes",
            BtreeId::Snapshots => "snapshots",
            BtreeId::SnapshotTrees => "snapshot_trees",
            BtreeId::Alloc => "alloc",
            BtreeId::Freespace => "freespace",
            BtreeId::BucketGens => "bucket_gens",
        }
    }

    /// btree type 总数
    pub const fn count() -> usize {
        7
    }

    /// 将 type 映射到 `[Btree; 7]` 数组索引
    pub(crate) const fn index(self) -> usize {
        match self {
            BtreeId::Extents => 0,
            BtreeId::Subvolumes => 1,
            BtreeId::Snapshots => 2,
            BtreeId::SnapshotTrees => 3,
            BtreeId::Alloc => 4,
            BtreeId::Freespace => 5,
            BtreeId::BucketGens => 6,
        }
    }

    /// 从 u8 表示反解 BtreeId（用于 WAL replay 等反序列化场景）
    ///
    /// 与 `index()` 的映射一致：0→Extents, 1→Subvolumes, 2→Snapshots,
    /// 3→SnapshotTrees, 4→Alloc, 5→Freespace。越界返回 None。
    pub fn from_u8(v: u8) -> Option<BtreeId> {
        match v {
            0 => Some(BtreeId::Extents),
            1 => Some(BtreeId::Subvolumes),
            2 => Some(BtreeId::Snapshots),
            3 => Some(BtreeId::SnapshotTrees),
            4 => Some(BtreeId::Alloc),
            5 => Some(BtreeId::Freespace),
            6 => Some(BtreeId::BucketGens),
            _ => None,
        }
    }
}

/// 所有 btree type 的完整列表（bcachefs 对齐的 BTREE_ID_NR）
pub const BTREE_ID_NR: [BtreeId; 7] = [
    BtreeId::Extents,
    BtreeId::Subvolumes,
    BtreeId::Snapshots,
    BtreeId::SnapshotTrees,
    BtreeId::Alloc,
    BtreeId::Freespace,
    BtreeId::BucketGens,
];

/// 批量操作类型
pub enum BatchEntry {
    Insert { pos: Bpos, data: Vec<u8> },
    Delete { pos: Bpos },
}

// ---------------------------------------------------------------------------
// BtreeEngine — 多 btree 实例的统一持有者
// ---------------------------------------------------------------------------

/// 持有所有 [`BtreeId`] 对应的 `Btree` 实例，提供按 type 路由的访问接口。
///
/// 每个 type 的 btree 拥有独立的根节点和 key 空间，适合 bcachefs 风格的多元数据
/// 组织方式。`BtreeEngine` 负责将外部请求按 `BtreeId` 路由到正确的实例。

#[derive(Debug)]
pub struct BtreeEngine {
    trees: [Btree; 7],
    /// Journal overlay（Phase 4: journal_keys 对齐）
    /// set_may_go_rw → journal_replay 期间捕获外部写入
    pub journal_overlay: Option<JournalKeys>,
    /// 子卷 ID 分配计数器（从 1 递增，0 保留）
    /// 每个引擎独立计数，保证 test 并行安全
    pub(crate) subvol_id_counter: u32,
    /// inode → 子卷 ID 列表映射（SubvolInoMap 兼容）
    ///
    /// 用于在删除子卷时清理关联的 inode 映射记录。
    /// 对齐 bcachefs `subvol_ino_map` 的简化实现。
    pub subvol_ino_map: HashMap<u64, Vec<u32>>,
}

impl Default for BtreeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl BtreeEngine {
    /// 创建新的 `BtreeEngine`，为每个 `BtreeId` 初始化一个独立的 `Btree` 实例。
    pub fn new() -> Self {
        Self {
            trees: [
                Btree::new(),
                Btree::new(),
                Btree::new(),
                Btree::new(),
                Btree::new(),
                Btree::new(),
                Btree::new(),
            ],
            journal_overlay: None,
            subvol_id_counter: 1,
            subvol_ino_map: HashMap::new(),
        }
    }

    /// 根据 `BtreeId` 获取对应 btree 实例的不可变引用。
    pub fn get(&self, ty: BtreeId) -> &Btree {
        &self.trees[ty.index()]
    }

    /// 根据 `BtreeId` 获取对应 btree 实例的可变引用。
    pub fn get_mut(&mut self, ty: BtreeId) -> &mut Btree {
        &mut self.trees[ty.index()]
    }

    /// 为所有 btree cache 注入 backend。
    pub fn set_backend(&self, backend: Arc<dyn BlockDevice>) -> bool {
        let mut ok = true;
        for tree in &self.trees {
            ok &= tree.cache().set_backend(backend.clone());
        }
        ok
    }

    /// 为所有 btree cache 注入 writeback coordinator。
    pub fn set_writeback_handle(&self, writeback: Arc<WritebackHandle>) -> bool {
        let mut ok = true;
        for tree in &self.trees {
            ok &= tree.cache().set_writeback_handle(writeback.clone());
        }
        ok
    }

    /// 从 backend 加载指定 BtreeId 的根节点
    pub async fn load_root(
        &mut self,
        ty: BtreeId,
        backend: &dyn BlockDevice,
        root_addr: u64,
    ) -> Result<(), StorageError> {
        self.trees[ty.index()].load_root(backend, root_addr).await
    }

    /// 从完整持久 root pointer 加载指定 BtreeId 的根节点。
    pub async fn load_root_from_ptr(
        &mut self,
        ty: BtreeId,
        backend: &dyn BlockDevice,
        root_ptr: crate::btree::types::BtreePtrV2,
    ) -> Result<(), StorageError> {
        self.trees[ty.index()]
            .load_root_from_ptr(backend, root_ptr)
            .await
    }

    /// 设置指定 btree 的持久根指针。
    pub fn set_root_ptr(&mut self, ty: BtreeId, ptr: BtreePtrV2) {
        self.trees[ty.index()].set_root_ptr_internal(ptr);
    }

    /// 遍历所有 btree 实例并对其调用 `f`。
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(BtreeId, &Btree),
    {
        for ty in BTREE_ID_NR {
            f(ty, self.get(ty));
        }
    }

    /// 在指定 type 的 btree 上执行一次查询。
    ///
    /// bcachefs 对齐：若 journal overlay 存在且未 draining，
    /// 先检查 overlay 中是否存在匹配的 key（读穿透）。
    pub fn get_entry(&self, ty: BtreeId, key: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        // Check overlay first (journal_keys read-through)
        if let Some(ref overlay) = self.journal_overlay {
            if !overlay.draining {
                if let Some(oe) = overlay.lookup_entry(ty, Bpos::from_key(key)) {
                    return Some(oe.entry.to_legacy());
                }
            }
        }
        self.get(ty).get(key)
    }

    /// 在指定 type 的 btree 上执行搜索。
    ///
    /// bcachefs 对齐：若 journal overlay 存在且未 draining，
    /// 先检查 overlay 中是否存在匹配的 key；否则回落到节点本地搜索。
    pub fn search(&self, ty: BtreeId, key: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        if let Some(ref overlay) = self.journal_overlay {
            if !overlay.draining {
                if let Some(oe) = overlay.lookup_entry(ty, Bpos::from_key(key)) {
                    return Some(oe.entry.to_legacy());
                }
            }
        }
        self.get(ty).search(key)
    }

    /// 在指定 type 的 btree 上通过 Bpos 查询（支持 KeyValue::Raw）。
    ///
    /// bcachefs 对齐：若 journal overlay 存在且未 draining，
    /// 先检查 overlay 中是否存在匹配的 entry（读穿透）。
    pub fn get_entry_raw(&self, ty: BtreeId, pos: Bpos) -> Option<BtreeEntry> {
        // Check overlay first (journal_keys read-through)
        if let Some(ref overlay) = self.journal_overlay {
            if !overlay.draining {
                if let Some(oe) = overlay.lookup_entry(ty, pos) {
                    return Some(oe.entry.clone());
                }
            }
        }
        self.get(ty).get_entry(pos)
    }

    /// 在指定 type 的 btree 上插入一条记录。
    pub fn insert_entry(
        &mut self,
        ty: BtreeId,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
    ) -> bool {
        self.get_mut(ty).insert(key, value, journal_seq)
    }

    /// 在指定 type 的 btree 上插入一条记录但不使 key cache 失效（TC5: cached 写入路径）。
    ///
    /// 写入 btree 后不调用 `key_cache.invalidate()`，供脏缓存写回路径使用。
    pub fn insert_entry_skip_cache(
        &mut self,
        ty: BtreeId,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
    ) -> bool {
        let pos = Bpos::from_key(&key);
        let btree_entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::Extent(value));
        self.get_mut(ty)
            .insert_entry_skip_cache(btree_entry, journal_seq)
    }

    /// 在指定 type 的 btree 上插入记录并创建脏缓存条目（TC5: cached 写入路径）。
    ///
    /// 1. 使用 `insert_entry_skip_cache` 写入 btree（不使 cache 失效）
    /// 2. 调用 `bch2_btree_insert_key_cached` 创建/更新脏缓存条目
    ///
    /// 对应 bcachefs 中 `entry.cached == true` 时的写回路径。
    /// journal pin callback 在 journal reclaim 时驱动 flush。
    pub fn insert_entry_cached(
        &mut self,
        ty: BtreeId,
        key: BtreeKey,
        value: BchVal,
        journal_seq: u64,
    ) -> bool {
        let pos = Bpos::from_key(&key);
        let btree_entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::Extent(value));
        let succeeded = self
            .get_mut(ty)
            .insert_entry_skip_cache(btree_entry.clone(), journal_seq);
        if succeeded {
            self.get_mut(ty)
                .key_cache
                .bch2_btree_insert_key_cached(pos, btree_entry, journal_seq);
        }
        succeeded
    }

    /// 在指定 type 的 btree 上插入 BtreeEntry（支持 KeyValue::Raw）。
    pub fn insert_entry_raw(&mut self, ty: BtreeId, entry: BtreeEntry, journal_seq: u64) -> bool {
        self.get_mut(ty).insert_entry(entry, journal_seq)
    }

    /// bcachefs 对齐的 guarded insert
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek()` 路径中对 `journal_keys` 的检查。
    /// 如果 overlay active 且不在 draining，推入 overlay buffer；
    /// 否则直写 btree。
    ///
    /// 外部 API（daemon volume write）→ insert_guarded()
    /// 内部操作（replay、compaction、split/merge）→ insert_entry_raw()
    pub fn insert_guarded(&mut self, ty: BtreeId, entry: BtreeEntry, journal_seq: u64) -> bool {
        // flush dirty key cache entries to btree before journal write
        self.flush_cache_dirty_keys(journal_seq);

        if let Some(ref mut overlay) = self.journal_overlay {
            if overlay.active && !overlay.draining {
                overlay.push(journal_seq, ty, entry);
                return true;
            }
        }
        self.insert_entry_raw(ty, entry, journal_seq)
    }

    /// 启用 journal overlay（由 set_may_go_rw pass 调用）
    ///
    /// 创建 JournalKeys 并立即激活（active = true）。
    /// 此后 insert_guarded() 写入走 overlay buffer，
    /// journal_replay pass 完成后通过 drain_overlay() 刷回 btree。
    pub fn enable_overlay(&mut self) {
        let mut overlay = JournalKeys::new();
        overlay.active = true;
        self.journal_overlay = Some(overlay);
    }

    /// 禁用并 drain journal overlay（由 journal_replay pass 调用）
    pub fn drain_overlay(&mut self) {
        if let Some(mut overlay) = self.journal_overlay.take() {
            overlay.drain_all(self);
        }
    }

    /// 在指定 type 的 btree 上删除一条记录。
    pub fn delete_entry(&mut self, ty: BtreeId, key: &BtreeKey, journal_seq: u64) -> bool {
        self.get_mut(ty).delete(key, journal_seq)
    }

    /// 在同一个 btree 上执行批量写入操作，合并为一个 btree 访问序列。
    /// 所有写入在同一锁范围内顺序执行，减少崩溃不一致窗口。
    ///
    /// entries 参数：`Vec<(BatchEntry, u64)>` 其中 BatchOp 为 Insert/Delete 枚举。
    /// 注意：这不是真正的 btree 事务（不支持跨 btree 原子性），仅确保单 btree 内顺序写入。
    pub fn batch_write(&mut self, ty: BtreeId, entries: &[(BatchEntry, u64)]) -> bool {
        // flush dirty keys before batch write to reduce journal pin pressure
        self.flush_cache_dirty_keys(0);

        for (entry, journal_seq) in entries {
            match entry {
                BatchEntry::Insert { pos, data } => {
                    let btree_entry = BtreeEntry::raw(*pos, KeyType::Normal, data.clone());
                    if !self.insert_entry_raw(ty, btree_entry, *journal_seq) {
                        return false;
                    }
                }
                BatchEntry::Delete { pos } => {
                    let key = BtreeKey::from_bpos(*pos, KeyType::Normal);
                    if !self.delete_entry(ty, &key, *journal_seq) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// 遍历所有 btree type，收集每个 btree 的脏节点序列化数据。
    ///
    /// 对每个有脏节点的 btree 调用 `flush_dirty()` 将脏节点从 cache 中 drain 出来
    /// 并返回按 level 升序排列的节点列表。返回一个 vec，每个元素是
    /// `(BtreeId, Vec<(node_id, Arc<BtreeNode>)>)`，供上层遍历写入后端。
    /// 每个 btree type 内部的节点已按拓扑顺序排列（叶子先于内层节点）。
    pub fn flush_dirty_nodes(&self) -> Vec<(BtreeId, Vec<(u64, Arc<BtreeNode>)>)> {
        let mut result = Vec::new();
        for ty in BTREE_ID_NR {
            let serialized = self.get(ty).flush_dirty();
            if !serialized.is_empty() {
                result.push((ty, serialized));
            }
        }
        result
    }

    /// 遍历所有 btree 的 key cache，写回脏条目。
    ///
    /// 在每个同步点（batch_write、insert_guarded、trans_commit 前）调用。
    /// 使用 `insert_entry_skip_cache` 避免 flush 时 invalidation。
    ///
    /// 两阶段实现（避免借用冲突）：
    /// 1. 遍历各 tree 的 key_cache，收集脏条目（`&self`）
    /// 2. 遍历收集的条目，写回 btree（`&mut self`）
    /// `journal_seq`：flush 时关联的 journal 序列号，写入 btree 节点时记录。
    /// 在 bcachefs 中对应 `ck->journal.seq`，用于 recovery 时追踪每个节点
    /// 关联的 journal 条目。如果 flush 时不知道当前 seq 可传 0
    /// （后续写入会覆盖 node.journal_seq）。
    pub fn flush_cache_dirty_keys(&mut self, journal_seq: u64) -> usize {
        // Phase 1: 收集所有脏条目（不可变借用 self）
        type DirtyEntry = (BtreeId, Bpos, BtreeEntry);
        let all_dirty: Vec<DirtyEntry> = BTREE_ID_NR
            .iter()
            .map(|&ty| {
                let idx = ty as usize;
                (ty, self.trees[idx].key_cache.collect_dirty())
            })
            .flat_map(|(ty, entries)| {
                entries
                    .into_iter()
                    .map(move |(pos, entry)| (ty, pos, entry))
            })
            .collect();

        let total = all_dirty.len();
        if total == 0 {
            return 0;
        }

        // Phase 2: 写回 btree（可变借用 self）
        for (ty, pos, entry) in &all_dirty {
            if self
                .get_mut(*ty)
                .insert_entry_skip_cache(entry.clone(), journal_seq)
            {
                self.trees[*ty as usize].key_cache.mark_clean(pos);
            }
        }

        total
    }

    // ─── SubvolInoMap 操作 ───

    /// 注册 inode → subvol_id 映射
    ///
    /// 对齐 bcachefs `subvol_ino_map_register`。
    /// 应在创建子卷时调用，将 inode 与子卷 ID 关联。
    pub fn register_ino_map(&mut self, inode: u64, subvol_id: u32) {
        self.subvol_ino_map
            .entry(inode)
            .or_default()
            .push(subvol_id);
    }

    /// 清除指定子卷的 inode 映射记录
    ///
    /// 对齐 bcachefs `subvol_ino_map_cleanup`。
    /// 在删除子卷时调用，移除关联的 inode→subvol_id 条目。
    /// 如果某个 inode 不再关联任何子卷，也清理该 inode 的映射。
    pub fn cleanup_ino_map(&mut self, inode: u64, subvol_id: u32) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.subvol_ino_map.entry(inode)
        {
            entry.get_mut().retain(|&id| id != subvol_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_btree_type_all_coverage() {
        // 验证 ALL 列表覆盖所有变体且无遗漏
        assert_eq!(BTREE_ID_NR.len(), BtreeId::count());
        let mut set = std::collections::HashSet::new();
        for ty in BTREE_ID_NR {
            assert!(set.insert(ty), "duplicate BtreeId variant in ALL");
        }
        assert_eq!(set.len(), BtreeId::count());
    }

    #[test]
    fn test_btree_type_index_roundtrip() {
        // 每个 type 到索引再回来应该唯一
        use std::collections::HashSet;
        let mut indices = HashSet::new();
        for ty in BTREE_ID_NR {
            let idx = ty.index();
            assert!(
                idx < BtreeId::count(),
                "index {} >= count {}",
                idx,
                BtreeId::count()
            );
            assert!(indices.insert(idx), "duplicate index {}", idx);
        }
    }

    #[test]
    fn test_btree_type_name_not_empty() {
        for ty in BTREE_ID_NR {
            let n = ty.name();
            assert!(!n.is_empty(), "empty name for {:?}", ty);
        }
    }

    #[test]
    fn test_btree_engine_new_all_initialized() {
        let engine = BtreeEngine::new();
        // 确认每个 type 都有独立的 btree 实例（通过 root 指针区分）
        let roots: std::collections::HashSet<*const _> = BTREE_ID_NR
            .iter()
            .map(|ty| engine.get(*ty).root() as *const _)
            .collect();
        assert_eq!(
            roots.len(),
            BtreeId::count(),
            "each BtreeId must have a distinct Btree instance"
        );
    }

    #[test]
    fn test_btree_engine_insert_and_get() {
        let mut engine = BtreeEngine::new();
        let key = BtreeKey::from_bpos(Bpos::new(1, 100, 42), KeyType::Normal);
        let val = BchVal::new(0x1234, 1);

        assert!(engine.insert_entry(BtreeId::Extents, key, val, 0));
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert_eq!(got, Some((key, val)));
    }

    #[test]
    fn test_btree_engine_types_independent() {
        // 验证不同 type 的 btree 互相隔离
        let mut engine = BtreeEngine::new();

        let ext_key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let ext_val = BchVal::new(0x100, 1);
        engine.insert_entry(BtreeId::Extents, ext_key, ext_val, 0);

        let snap_key = BtreeKey::from_bpos(Bpos::new(2, 20, 1), KeyType::Normal);
        let snap_val = BchVal::new(0x200, 2);
        engine.insert_entry(BtreeId::Snapshots, snap_key, snap_val, 0);

        // 隔离验证：Extents 中查不到 Snapshots 的 key
        assert_eq!(
            engine.get_entry(BtreeId::Extents, &snap_key),
            None,
            "btree types must be isolated"
        );
        assert_eq!(
            engine.get_entry(BtreeId::Snapshots, &ext_key),
            None,
            "btree types must be isolated"
        );
    }

    #[test]
    fn test_btree_engine_delete() {
        let mut engine = BtreeEngine::new();
        let key = BtreeKey::from_bpos(Bpos::new(1, 50, 0), KeyType::Normal);
        let val = BchVal::new(0xABC, 1);

        assert!(engine.insert_entry(BtreeId::Extents, key, val, 0));
        assert_eq!(engine.get_entry(BtreeId::Extents, &key), Some((key, val)));

        assert!(engine.delete_entry(BtreeId::Extents, &key, 0));
        assert_eq!(engine.get_entry(BtreeId::Extents, &key), None);

        // 删除不存在的 key 应返回 false
        assert!(!engine.delete_entry(BtreeId::Extents, &key, 0));
    }

    #[test]
    fn test_btree_engine_for_each() {
        let engine = BtreeEngine::new();
        let mut count = 0;
        engine.for_each(|ty, bt| {
            count += 1;
            assert_eq!(bt.key_count(), 0, "btree {:?} not empty", ty);
        });
        assert_eq!(count, BtreeId::count());
    }

    #[test]
    fn test_btree_engine_default() {
        let engine: BtreeEngine = Default::default();
        assert!(engine.get(BtreeId::Alloc).key_count() == 0);
    }

    use crate::alloc::BchAllocator;
    use crate::block_device::MockBlockDevice;
    use crate::btree::key::KeyValue;
    use crate::journal::Journal;
    use crate::meta::VolumeMeta;
    use crate::recovery::{self, RecoveryState};
    use crate::storage::superblock::BchSb;
    use crate::types::{BackendType, BlockAddr};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// 创建最小 BchSb 用于测试
    fn test_superblock() -> BchSb {
        let meta = VolumeMeta::new(
            "test-vol".into(),
            1,
            "pool".into(),
            4096,
            1024 * 1024,
            BackendType::Sparse,
        );
        BchSb::new(meta)
    }

    #[tokio::test]
    async fn test_recovery_pass_journal_read_and_replay() {
        let backend = Arc::new(MockBlockDevice::new());
        let engine = BtreeEngine::new();
        let mut journal = Journal::new(vec![100]);

        journal
            .append(
                BtreeId::Extents,
                &[
                    BtreeEntry::new(
                        Bpos::new(1, 10, 0),
                        KeyType::Normal,
                        KeyValue::extent(0x100, 1),
                    ),
                    BtreeEntry::new(
                        Bpos::new(1, 20, 0),
                        KeyType::Normal,
                        KeyValue::extent(0x200, 1),
                    ),
                ],
                false,
                &*backend,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush(&*backend).await.unwrap();

        let sb = test_superblock();
        let mut state = RecoveryState::new(
            engine,
            journal,
            backend.clone(),
            sb,
            BchAllocator::new(0, 1, 0),
        );
        recovery::bch2_fs_recovery(&mut state).await.unwrap();

        let k1 = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        assert!(
            state.engine.get_entry(BtreeId::Extents, &k1).is_some(),
            "key 10 should exist"
        );

        let k2 = BtreeKey::from_bpos(Bpos::new(1, 20, 0), KeyType::Normal);
        assert!(
            state.engine.get_entry(BtreeId::Extents, &k2).is_some(),
            "key 20 should exist"
        );

        // Verify passes tracked
        assert!(state.passes_complete > 0, "passes should have completed");
        assert!(!state.jsets.is_empty(), "jsets should be populated");
    }

    #[tokio::test]
    async fn test_recovery_pass_btree_roots() {
        let backend = Arc::new(MockBlockDevice::new());
        let engine = BtreeEngine::new();

        let mut root_node = BtreeNode::new_leaf();
        assert!(root_node.insert(
            BtreeKey::new(42, 1, KeyType::Normal),
            BchVal::new(0xDEAD, 1)
        ));
        let node_bytes = root_node.serialize_to_bucket(0xABCD).unwrap();
        backend
            .write_block(BlockAddr::new(0xABCD), &node_bytes)
            .await
            .unwrap();

        let mut journal = Journal::new(vec![100]);
        journal
            .append_btree_root(BtreeId::Extents, 0xABCD, false, &*backend)
            .await
            .unwrap();
        journal.bch2_journal_flush(&*backend).await.unwrap();
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 10, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x100, 1),
                )],
                false,
                &*backend,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush(&*backend).await.unwrap();

        let sb = test_superblock();
        let mut state = RecoveryState::new(
            engine,
            journal,
            backend.clone(),
            sb,
            BchAllocator::new(0, 1, 0),
        );
        recovery::bch2_fs_recovery(&mut state).await.unwrap();
        let root_key = BtreeKey::new(42, 1, KeyType::Normal);
        assert!(
            state
                .engine
                .get_entry(BtreeId::Extents, &root_key)
                .is_some(),
            "root-loaded key 42 should exist"
        );

        let jkey = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        assert!(
            state.engine.get_entry(BtreeId::Extents, &jkey).is_some(),
            "journal key 10 should exist"
        );
    }

    #[tokio::test]
    async fn test_recovery_empty_journal() {
        let backend = Arc::new(MockBlockDevice::new());
        let engine = BtreeEngine::new();
        let journal = Journal::new(vec![]);
        let sb = test_superblock();

        let mut state =
            RecoveryState::new(engine, journal, backend, sb, BchAllocator::new(0, 1, 0));
        let result = recovery::bch2_fs_recovery(&mut state).await;
        assert!(result.is_ok(), "empty journal should not error");
    }

    // ── journal overlay read-through tests ─────────────────────────────

    #[test]
    fn test_overlay_read_through_get_entry() {
        let mut engine = BtreeEngine::new();

        // Insert directly to btree
        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let btree_val = BchVal::new(0x100, 1);
        engine.insert_entry_raw(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x100, 1),
            ),
            0,
        );

        // Enable overlay and write a different value (over the same key)
        engine.enable_overlay();
        engine.insert_guarded(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x200, 1),
            ),
            1,
        );

        // Read-through: should see overlay value (0x200), not btree value (0x100)
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(got.is_some(), "should find entry via overlay");
        assert_eq!(
            got.unwrap().1.paddr.get(),
            0x200,
            "overlay value should win"
        );
    }

    #[test]
    fn test_overlay_read_through_search() {
        let mut engine = BtreeEngine::new();

        engine.insert_entry_raw(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x100, 1),
            ),
            0,
        );

        engine.enable_overlay();
        engine.insert_guarded(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x200, 1),
            ),
            1,
        );

        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let got = engine.search(BtreeId::Extents, &key);
        assert!(got.is_some(), "search should find entry via overlay");
        assert_eq!(
            got.unwrap().1.paddr.get(),
            0x200,
            "overlay value should win"
        );
    }

    #[test]
    fn test_overlay_read_through_get_entry_raw() {
        let mut engine = BtreeEngine::new();

        // Insert directly to btree
        engine.insert_entry_raw(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x100, 1),
            ),
            0,
        );

        // Enable overlay and overwrite same key
        engine.enable_overlay();
        engine.insert_guarded(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x200, 1),
            ),
            1,
        );

        // get_entry_raw should also see overlay value
        let got = engine.get_entry_raw(BtreeId::Extents, Bpos::new(1, 10, 0));
        assert!(got.is_some(), "should find entry via overlay raw");
        assert_eq!(
            got.unwrap().value.as_extent().unwrap().paddr.get(),
            0x200,
            "overlay value should win"
        );
    }

    #[test]
    fn test_overlay_read_through_draining_uses_btree() {
        let mut engine = BtreeEngine::new();

        // Enable overlay and write a key via guarded (goes to overlay)
        engine.enable_overlay();
        engine.insert_guarded(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x200, 1),
            ),
            1,
        );

        // Before drain: overlay is active, reads see overlay entry
        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(got.is_some(), "should see overlay entry before drain");
        assert_eq!(
            got.unwrap().1.paddr.get(),
            0x200,
            "overlay value before drain"
        );

        // After drain: overlay gone, btree has the entry
        engine.drain_overlay();
        assert!(
            engine.journal_overlay.is_none(),
            "overlay removed after drain"
        );

        // BtreeEngine::get_entry (via BtreeKey) finds the entry in btree
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(
            got.is_some(),
            "engine get_entry should find entry in btree after drain"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x200, "drained value in btree");
    }

    #[test]
    fn test_overlay_read_through_no_overlay() {
        let mut engine = BtreeEngine::new();

        // No overlay active
        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        engine.insert_entry_raw(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x100, 1),
            ),
            0,
        );

        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(got.is_some());
        assert_eq!(got.unwrap().1.paddr.get(), 0x100);
    }

    #[test]
    fn test_overlay_read_through_key_not_in_overlay() {
        let mut engine = BtreeEngine::new();

        // Insert entry1 to btree
        engine.insert_entry_raw(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 10, 0),
                KeyType::Normal,
                KeyValue::extent(0x100, 1),
            ),
            0,
        );

        // Enable overlay and write a DIFFERENT key
        engine.enable_overlay();
        engine.insert_guarded(
            BtreeId::Extents,
            BtreeEntry::new(
                Bpos::new(1, 20, 0),
                KeyType::Normal,
                KeyValue::extent(0x200, 1),
            ),
            1,
        );

        // Read the key that is NOT in overlay → should fall through to btree
        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(got.is_some(), "should find entry in btree");
        assert_eq!(got.unwrap().1.paddr.get(), 0x100);
    }

    #[test]
    fn test_insert_guarded_flushes_dirty_key_cache() {
        let mut engine = BtreeEngine::new();
        let dirty_pos = Bpos::new(1, 50, 0);
        let dirty_entry = BtreeEntry::new(dirty_pos, KeyType::Normal, KeyValue::extent(0x111, 1));

        engine.trees[BtreeId::Extents.index()]
            .key_cache
            .bch2_btree_insert_key_cached(dirty_pos, dirty_entry, 0);
        assert_eq!(
            engine.trees[BtreeId::Extents.index()]
                .key_cache
                .nr_dirty_keys(),
            1
        );

        let write_pos = Bpos::new(1, 60, 0);
        let write_entry = BtreeEntry::new(write_pos, KeyType::Normal, KeyValue::extent(0x222, 1));

        assert!(engine.insert_guarded(BtreeId::Extents, write_entry, 0));
        assert_eq!(
            engine.trees[BtreeId::Extents.index()]
                .key_cache
                .nr_dirty_keys(),
            0
        );

        let cached_key = BtreeKey::from_bpos(dirty_pos, KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &cached_key);
        assert!(
            got.is_some(),
            "dirty key should still be readable after flush"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x111);
    }

    #[test]
    fn test_batch_write_flushes_dirty_key_cache() {
        let mut engine = BtreeEngine::new();
        let dirty_pos = Bpos::new(1, 70, 0);
        let dirty_entry = BtreeEntry::new(dirty_pos, KeyType::Normal, KeyValue::extent(0x333, 1));

        engine.trees[BtreeId::Extents.index()]
            .key_cache
            .bch2_btree_insert_key_cached(dirty_pos, dirty_entry, 0);
        assert_eq!(
            engine.trees[BtreeId::Extents.index()]
                .key_cache
                .nr_dirty_keys(),
            1
        );

        let batch_pos = Bpos::new(1, 80, 0);
        let batch_entry = BatchEntry::Insert {
            pos: batch_pos,
            data: vec![0x10, 0x20, 0x30, 0x40],
        };

        assert!(engine.batch_write(BtreeId::Extents, &[(batch_entry, 0)]));
        assert_eq!(
            engine.trees[BtreeId::Extents.index()]
                .key_cache
                .nr_dirty_keys(),
            0
        );

        let cached_key = BtreeKey::from_bpos(dirty_pos, KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &cached_key);
        assert!(
            got.is_some(),
            "dirty key should remain readable after batch flush"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x333);
    }

    #[tokio::test]
    async fn test_recovery_superblock_roots() {
        let backend = Arc::new(MockBlockDevice::new());
        let engine = BtreeEngine::new();

        let mut alloc_node = BtreeNode::new_leaf();
        assert!(alloc_node.insert(
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0xCAFE, 1)
        ));
        let node_bytes = alloc_node.serialize_to_bucket(0xBBBB).unwrap();
        backend
            .write_block(BlockAddr::new(0xBBBB), &node_bytes)
            .await
            .unwrap();

        let mut journal = Journal::new(vec![200]);
        journal
            .append(
                BtreeId::Extents,
                &[BtreeEntry::new(
                    Bpos::new(1, 99, 0),
                    KeyType::Normal,
                    KeyValue::extent(0x999, 1),
                )],
                false,
                &*backend,
            )
            .await
            .unwrap();
        journal.bch2_journal_flush(&*backend).await.unwrap();

        let mut sb = test_superblock();
        // Set root_ptrs so btree_roots pass loads the Alloc root with full pointer
        while sb.root_addrs.len() < 5 {
            sb.root_addrs.push(0);
        }
        while sb.root_levels.len() < 5 {
            sb.root_levels.push(0);
        }
        while sb.root_ptrs.len() < 5 {
            sb.root_ptrs.push(crate::btree::types::BtreePtrV2::INVALID);
        }
        sb.root_addrs[4] = 0xBBBB; // Alloc type index
        sb.root_levels[4] = 0;
        sb.root_ptrs[4] = crate::btree::types::BtreePtrV2 {
            block_addr: 0xBBBB,
            sectors_written: (node_bytes.len() / crate::btree::node::SECTOR_SIZE) as u16,
            level: 0,
            generation: 1,
        };

        let mut state = RecoveryState::new(
            engine,
            journal,
            backend.clone(),
            sb,
            BchAllocator::new(0, 1, 0),
        );
        recovery::bch2_fs_recovery(&mut state).await.unwrap();

        let alloc_key = BtreeKey::new(100, 1, KeyType::Normal);
        assert!(
            state.engine.get_entry(BtreeId::Alloc, &alloc_key).is_some(),
            "superblock-loaded Alloc key 100 should exist"
        );

        let ext_key = BtreeKey::from_bpos(Bpos::new(1, 99, 0), KeyType::Normal);
        assert!(
            state.engine.get_entry(BtreeId::Extents, &ext_key).is_some(),
            "journal key 99 should exist"
        );
    }
}
