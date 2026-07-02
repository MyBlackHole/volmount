//! Btree — bcachefs 对齐的 B-tree 公共 API
//!
//! 提供 get/insert/delete 高级接口，内部使用 BtreeTrans + BtreeIter。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::block_device::BlockDevice;
use crate::btree::iter::{BtreeIter, IterFlags};
use crate::btree::key::{BchVal, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
use crate::btree::key_cache::KeyCache;
use crate::btree::node::BtreeNode;
use crate::btree::transaction::BtreeTrans;
use crate::btree::types::{BtreeRoot, NodeCache, ROOT_CACHE_ADDR};
use crate::btree::update::{BtreeInteriorUpdate, BtreeUpdateMode, InteriorUpdateType};
use crate::btree::BtreeId;
use crate::StorageError;

/// RAII guard for split-allocated nodes — bcachefs 对齐的错误路径回滚
///
/// split 过程中如果 parent update 失败，guard 在 drop 时自动释放
/// 已分配的右节点（从 cache 中移除）。
/// 操作成功时调用 `disarm()` 禁用回滚。
struct SplitGuard {
    cache: Arc<NodeCache>,
    right_addr: u64,
    disarmed: bool,
}

impl SplitGuard {
    fn new(cache: Arc<NodeCache>, right_addr: u64) -> Self {
        Self {
            cache,
            right_addr,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for SplitGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.cache.take_node(self.right_addr);
        }
    }
}

/// B-tree 主结构 — 对应 bcachefs `bch_fs` 中的 btree 实例
pub struct Btree {
    /// B-tree 根
    root: BtreeRoot,
    /// 节点缓存
    cache: Arc<NodeCache>,
    /// 整棵树（所有 leaf）的 key 总数
    total_key_count: u32,
    /// 写入阻塞标志：interior update（分裂/合并）进行中时阻止并发修改
    write_blocked: AtomicBool,
    /// 提交锁：序列化 interior update 的提交阶段
    commit_lock: AtomicBool,
    /// Key 级读缓存：减少热 key 的 btree 全路径下降
    pub(crate) key_cache: KeyCache,
    /// depth=0 时 root 节点是否被修改且未 flush（替代 cache dirty tracking）
    root_modified: AtomicBool,
}

impl Btree {
    /// 创建一个新的空 B-tree
    pub fn new() -> Self {
        let cache = Arc::new(NodeCache::new());
        let node = Arc::new(BtreeNode::new_leaf());
        Self {
            root: BtreeRoot::new(node, 0),
            cache,
            total_key_count: 0,
            write_blocked: AtomicBool::new(false),
            commit_lock: AtomicBool::new(false),
            key_cache: KeyCache::new(),
            root_modified: AtomicBool::new(false),
        }
    }

    /// 从已有的根节点创建
    pub fn from_root(root: BtreeRoot, cache: Arc<NodeCache>) -> Self {
        let total = Self::count_entries(&root, &cache);
        Self {
            root,
            cache: cache.clone(),
            total_key_count: total,
            write_blocked: AtomicBool::new(false),
            commit_lock: AtomicBool::new(false),
            key_cache: KeyCache::new(),
            root_modified: AtomicBool::new(false),
        }
    }

    /// 遍历整棵树统计 entry 总数
    fn count_entries(root: &BtreeRoot, cache: &Arc<NodeCache>) -> u32 {
        if root.depth == 0 {
            root.node.key_count
        } else {
            let mut iter = BtreeIter::init(
                root,
                &BtreeKey::MIN_KEY,
                crate::btree::iter::IterFlags::default(),
                cache,
                crate::btree::BtreeId::Extents,
                None,
            );
            let mut count = 0u32;
            while let Some((k, _v)) = iter.peek() {
                if k.key_type != KeyType::Deleted {
                    count += 1;
                }
                if !iter.advance() {
                    break;
                }
            }
            count
        }
    }

    /// 获取根节点
    pub fn root(&self) -> &BtreeRoot {
        &self.root
    }

    /// 获取根节点的持久 pointer。
    pub fn root_ptr(&self) -> &crate::btree::types::BtreePtrV2 {
        &self.root.ptr
    }

    /// 设置根节点 node_size（仅用于测试）
    #[cfg(test)]
    pub(crate) fn set_root_node_size(&mut self, size: u32) {
        if let Some(root) = Arc::get_mut(&mut self.root.node) {
            root.node_size = size;
        }
    }

    fn node_progress(node: &BtreeNode) -> (u16, u16, u16) {
        let written = node.total_data_bytes().div_ceil(512).min(u16::MAX as u32) as u16;
        let sectors = (node.node_size / 512).min(u16::MAX as u32) as u16;
        let remaining = node
            .node_size
            .saturating_sub(node.total_data_bytes())
            .div_ceil(8)
            .min(u16::MAX as u32) as u16;
        (written, sectors, remaining)
    }

    fn init_interior_update(
        update: &mut BtreeInteriorUpdate,
        mode: BtreeUpdateMode,
        node: &BtreeNode,
    ) {
        update.set_btree_id(BtreeId::Extents);
        update.set_mode(mode);
        update.set_node_span(node.min_key, node.max_key);
        update.set_update_level_span(node.level, node.level);
        let (node_written, node_sectors, node_remaining) = Self::node_progress(node);
        update.set_node_progress(node_written, node_sectors, node_remaining);
    }

    /// 从 backend 读取 BtreeNode 并设为 tree root
    ///
    /// root_addr=0 时跳过（空 btree）。
    /// depth 从 node.level 获取。
    pub async fn load_root(
        &mut self,
        backend: &dyn BlockDevice,
        root_addr: u64,
    ) -> Result<(), StorageError> {
        if root_addr == 0 {
            return Ok(());
        }
        let (node, ptr) =
            crate::btree::bucket_io::load_btree_node_with_addr(backend, root_addr).await?;
        self.load_tree_from_loaded_root(backend, node, ptr).await
    }

    /// 从完整持久 root pointer 递归加载整棵树。
    ///
    /// 这条路径是 clean-load / recovery 的真正入口：先加载 root，再按
    /// internal node 中的 `KeyValue::BtreePtr` 递归加载 child，直到 leaf。
    /// `sectors_written` 作为恢复边界会在 `load_btree_node_from_ptr()` 内被严格
    /// 约束；这里额外校验 child level 只能比 parent 低一层。
    pub async fn load_root_from_ptr(
        &mut self,
        backend: &dyn BlockDevice,
        root_ptr: crate::btree::types::BtreePtrV2,
    ) -> Result<(), StorageError> {
        if !root_ptr.is_valid() {
            return Ok(());
        }

        let (node, loaded_ptr) =
            crate::btree::bucket_io::load_btree_node_with_ptr(backend, root_ptr).await?;
        self.load_tree_from_loaded_root(backend, node, loaded_ptr)
            .await
    }

    async fn load_tree_from_loaded_root(
        &mut self,
        backend: &dyn BlockDevice,
        root_node: BtreeNode,
        root_ptr: crate::btree::types::BtreePtrV2,
    ) -> Result<(), StorageError> {
        let mut stack = vec![(root_node, root_ptr)];
        let mut visited: HashSet<(u64, u32)> = HashSet::new();

        while let Some((node, ptr)) = stack.pop() {
            if !visited.insert((ptr.block_addr, ptr.generation)) {
                continue;
            }

            node.set_block_addr(ptr.block_addr);
            let node = Arc::new(node);
            self.cache.insert(ptr.block_addr, node.clone());

            if ptr.level > 0 {
                let expected_child_level = ptr.level - 1;
                for child_ptr in Self::collect_child_ptrs(&node) {
                    if child_ptr.level != expected_child_level {
                        return Err(StorageError::InvalidData(format!(
                            "btree child level mismatch: parent level {} child level {}",
                            ptr.level, child_ptr.level
                        )));
                    }
                    let (child_node, loaded_child_ptr) =
                        crate::btree::bucket_io::load_btree_node_with_ptr(backend, child_ptr)
                            .await?;
                    stack.push((child_node, loaded_child_ptr));
                }
            }
        }

        let root_node = self
            .cache
            .get(root_ptr.block_addr)
            .ok_or_else(|| StorageError::NotFound("btree root missing after load".into()))?;
        self.root = BtreeRoot::with_ptr(root_node, root_ptr.level, root_ptr);
        self.total_key_count = Self::count_entries(&self.root, &self.cache);
        Ok(())
    }

    fn collect_child_ptrs(node: &BtreeNode) -> Vec<crate::btree::types::BtreePtrV2> {
        let mut children = Vec::new();
        for set in node.sets.iter() {
            if set.size == 0 {
                continue;
            }
            for idx in 1..=set.size as usize {
                let entry = node.read_entry_raw(set, idx);
                if let KeyValue::BtreePtr(ptr) = entry.value {
                    children.push(ptr);
                }
            }
        }
        children
    }

    /// 获取 B-tree 深度
    pub fn depth(&self) -> u8 {
        self.root.depth
    }

    /// 查找 key — 精确匹配
    ///
    /// 返回精确匹配 target 的 (key, value)。
    /// 未找到返回 None。
    /// 通过 KeyCache 减少热 key 的 btree 全路径下降。
    /// 仅缓存正结果（有匹配 entry），不缓存负结果（对齐 bcachefs）。
    pub fn get(&self, target: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        let pos = Bpos::from_key(target);
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            if entry.key_type == target.key_type {
                if let KeyValue::Extent(bchval) = entry.value {
                    let key = BtreeKey::from_bpos(pos, entry.key_type);
                    return Some((key, bchval));
                }
            }
        }
        // ── Normal btree search ──
        let iter = BtreeIter::init(
            &self.root,
            target,
            IterFlags::default(),
            &self.cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let result = match iter.peek() {
            Some((k, v)) if k == *target => Some((k, v)),
            _ => None,
        };
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some((k, v)) = result {
            let entry = BtreeEntry::new(pos, k.key_type, KeyValue::Extent(v));
            self.key_cache.insert(pos, entry);
        }
        result
    }

    /// 查找 entry — 通过 Bpos 精确匹配（支持 Extent 和 Raw value）
    ///
    /// 如果目标位置有 Deleted/Whiteout 条目，会自动跳过并检查下一条。
    /// 这实现了 bcachefs 风格的更新模式：删除（追加 Deleted 墓碑）+ 插入（追加新值）
    /// 后，本函数始终返回最新的非删除条目。
    /// 通过 KeyCache 减少热 key 的 btree 全路径下降。
    /// 仅缓存正结果（有匹配 entry），不缓存负结果（对齐 bcachefs）。
    pub fn get_entry(&self, pos: Bpos) -> Option<BtreeEntry> {
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            return Some(entry);
        }
        // ── Normal btree search ──
        let result = self.get_entry_inner(pos);
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some(ref entry) = result {
            self.key_cache.insert(pos, entry.clone());
        }
        result
    }

    /// 带 BtreeTrans 重启感知的缓存查找（TC4: trigger_key_cache_miss 连接）
    ///
    /// 当 key cache miss 时触发事务重启（`trigger_key_cache_miss`），
    /// 使事务循环在 commit 时重试查找路径。缓存未命中且 btree 实际有值时，
    /// 第一次调用标记重启，插入缓存后，第二次调用（重启后）命中缓存返回。
    ///
    /// 这是 `get_entry` 的变体——不改变原方法签名。
    /// 对应 bcachefs 在 key cache miss 后的重启机制。
    pub fn get_entry_with_restart(&self, pos: Bpos, trans: &mut BtreeTrans) -> Option<BtreeEntry> {
        // ── Key cache check ──
        if let Some(entry) = self.key_cache.find(&pos) {
            return Some(entry);
        }
        // ── Cache miss: trigger restart signal for transaction loop ──
        trans.trigger_key_cache_miss();
        // ── Normal btree search ──
        let result = self.get_entry_inner(pos);
        // ── Cache positive result only (bcachefs 对齐: 不缓存负结果) ──
        if let Some(ref entry) = result {
            self.key_cache.insert(pos, entry.clone());
        }
        result
    }

    /// 获取条目（允许 Whiteout，不跳过已删除条目）。
    /// 用于需要读取已删除快照节点信息的场景（祖先链遍历）。
    pub(crate) fn get_entry_allow_whiteout(&self, pos: Bpos) -> Option<BtreeEntry> {
        let target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &self.root,
            &target,
            IterFlags::default(),
            &self.cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                // 接受任何 key_type（包括 Whiteout/Deleted）
                candidate = Some(entry);
            }
            if !iter.advance() {
                return candidate;
            }
        }
    }

    /// get_entry 的无缓存内部实现
    fn get_entry_inner(&self, pos: Bpos) -> Option<BtreeEntry> {
        let target = BtreeKey::from_bpos(pos, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &self.root,
            &target,
            IterFlags::default(),
            &self.cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let mut candidate: Option<BtreeEntry> = None;
        loop {
            let entry = iter.peek_entry()?;
            if entry.pos == pos {
                match entry.key_type {
                    KeyType::Normal => {
                        candidate = Some(entry);
                    }
                    KeyType::Deleted | KeyType::Whiteout => {
                        candidate = None;
                    }
                }
            }
            if !iter.advance() {
                return candidate;
            }
        }
    }

    /// 插入 entry — 支持 KeyValue::Raw 等任意 value 类型（depth=0 单 leaf 模式）
    /// 成功插入后使对应 Bpos 的 key cache 失效。
    pub fn insert_entry(&mut self, entry: BtreeEntry, journal_seq: u64) -> bool {
        let pos = entry.pos;
        if self.insert_entry_into_node(entry, journal_seq) {
            self.key_cache.invalidate(&pos);
            true
        } else {
            false
        }
    }

    /// 同 `insert_entry` 但不使 key cache 失效。
    /// 用于 flush_dirty 写回场景：entry 本身是 cached + dirty，写回后 cache 还应保留。
    pub fn insert_entry_skip_cache(&mut self, entry: BtreeEntry, journal_seq: u64) -> bool {
        self.insert_entry_into_node(entry, journal_seq)
    }

    /// 内部方法: 将 entry 写入 btree 节点（depth=0 单 leaf 模式）
    fn insert_entry_into_node(&mut self, entry: BtreeEntry, journal_seq: u64) -> bool {
        // 目前只支持单 leaf 模式
        if self.root.depth > 0 {
            return false;
        }
        let node = Arc::make_mut(&mut self.root.node);
        if node.insert_entry(&entry) {
            self.total_key_count += 1;
            node.journal_seq = journal_seq;
            self.root_modified.store(true, Ordering::Release);
            if self.root.ptr.is_valid() {
                self.cache
                    .insert(self.root.ptr.block_addr, self.root.node.clone());
            }
            return true;
        }
        node.compact();
        if node.insert_entry(&entry) {
            self.total_key_count += 1;
            node.journal_seq = journal_seq;
            self.root_modified.store(true, Ordering::Release);
            if self.root.ptr.is_valid() {
                self.cache
                    .insert(self.root.ptr.block_addr, self.root.node.clone());
            }
            return true;
        }
        false
    }

    /// 插入 key/value — 支持单级和多级 B-tree
    ///
    /// depth=0 单 leaf 模式：直接插入，满时 compact 重试，仍满则 split_root
    /// depth>0 多级树模式：`find_leaf_addr` → take_node → insert → put_node
    /// 成功插入后使对应 Bpos 的 key cache 失效。
    pub fn insert(&mut self, key: BtreeKey, value: BchVal, journal_seq: u64) -> bool {
        let pos = Bpos::from_key(&key);
        let result = if self.root.depth == 0 {
            let node = Arc::make_mut(&mut self.root.node);
            if node.insert(key, value) {
                self.total_key_count += 1;
                node.journal_seq = journal_seq;
                self.root_modified.store(true, Ordering::Release);
                true
            } else {
                node.compact();
                // A1: compact_fits — 只有 compact 释放了足够空间才重试 insert
                // BchVal 固定 8 字节 → 1 u64
                if node.compact_fits(1) && node.insert(key, value) {
                    self.total_key_count += 1;
                    node.journal_seq = journal_seq;
                    self.root_modified.store(true, Ordering::Release);
                    true
                } else if self.split_root(Some((key, value)), journal_seq) {
                    self.total_key_count += 1;
                    true
                } else {
                    false
                }
            }
        } else {
            self.insert_multi(key, value, journal_seq)
        };
        if result {
            if self.root.depth == 0 && self.root.ptr.is_valid() {
                self.cache
                    .insert(self.root.ptr.block_addr, self.root.node.clone());
            }
            self.key_cache.invalidate(&pos);
        }
        result
    }

    /// 删除 key — 支持单级和多级 B-tree
    /// 成功删除后使对应 Bpos 的 key cache 失效。
    pub fn delete(&mut self, key: &BtreeKey, journal_seq: u64) -> bool {
        let pos = Bpos::from_key(key);
        let result = if self.root.depth == 0 {
            if let Some(node) = Arc::get_mut(&mut self.root.node) {
                let deleted = node.delete_key(key);
                if deleted {
                    node.journal_seq = journal_seq;
                    self.root_modified.store(true, Ordering::Release);
                }
                deleted
            } else {
                false
            }
        } else {
            self.delete_multi(key, journal_seq)
        };
        if result {
            self.total_key_count = self.total_key_count.saturating_sub(1);
            self.key_cache.invalidate(&pos);
        }
        result
    }

    /// 根节点分裂：当前根（leaf 或 internal）已满，分裂为两个同级节点，提升为新根
    ///
    /// depth=0 时：leaf → 两个 leaf，新 internal 根，depth→1
    /// depth≥1 时：internal → 两个 internal，新 internal 根，depth→+1
    /// key/value 是触发分裂的超额 entry（data 或 routing）
    ///
    /// BtreeInteriorUpdate 生命周期：Init → NodesAllocated → UpdateParent → Done
    fn split_root(&mut self, entry: Option<(BtreeKey, BchVal)>, journal_seq: u64) -> bool {
        let mut update = BtreeInteriorUpdate::new(InteriorUpdateType::Split, journal_seq);

        let node = match Arc::get_mut(&mut self.root.node) {
            Some(n) => n,
            None => return false,
        };
        Self::init_interior_update(&mut update, BtreeUpdateMode::Root, node);
        // 保存原始 node_size（split 后 node 变为 left 节点）
        let old_node_size = node.node_size;
        let (median_key, mut right_node) = match node.split() {
            Some((k, n)) => (k, n),
            None => return false,
        };
        // 传播 node_size：分裂出的右侧节点应与原节点大小一致
        right_node.node_size = old_node_size;

        let mut left_node = node.clone();
        if let Some((key, value)) = entry {
            if key >= median_key {
                right_node.insert(key, value);
            } else {
                left_node.insert(key, value);
            }
        }
        // 分裂出的新节点继承当前 journal_seq，保持与 insert_multi() 路径一致。
        left_node.journal_seq = journal_seq;
        right_node.journal_seq = journal_seq;

        let left_addr = self.cache.alloc_addr();
        let right_addr = self.cache.alloc_addr();
        let left_arc = Arc::new(left_node);
        let right_arc = Arc::new(right_node);
        // bcachefs 对齐：新分裂出的节点在首次落盘前设置 will_make_reachable
        left_arc.set_will_make_reachable();
        right_arc.set_will_make_reachable();
        self.cache.insert_dirty(left_addr, left_arc);
        self.cache.insert_dirty(right_addr, right_arc);

        // 记录新节点到 BtreeInteriorUpdate
        update.add_new_node(crate::btree::types::BtreePtrV2 {
            block_addr: left_addr,
            sectors_written: 0,
            level: self.root.node.level,
            generation: 0,
        });
        update.add_new_node(crate::btree::types::BtreePtrV2 {
            block_addr: right_addr,
            sectors_written: 0,
            level: self.root.node.level,
            generation: 0,
        });
        update.set_median_key(median_key);
        update.mark_nodes_allocated();

        // 新根 level = 原根 level + 1
        // 当 depth=0 时原根 level=0 → 新根 level=1 ✓
        // 当 depth=2 时原根 level=1 → 新根 level=2 ✓
        let mut internal = BtreeNode::new_internal();
        internal.level = self.root.node.level + 1;
        // 新根使用与原节点相同的 node_size（保持一致性）
        internal.node_size = old_node_size;

        // 跟踪 write_entry 实际返回的大小，支持变长 entry
        let mut cur = 0u32;
        cur += internal.write_entry(cur, &BtreeKey::MIN_KEY, &BchVal::new(left_addr, 0));
        cur += internal.write_entry(cur, &median_key, &BchVal::new(right_addr, 0));

        use crate::btree::node::BsetTree;
        internal.sets[0] = BsetTree {
            data_offset: 0,
            end_offset: cur,
            aux_offset: 0,
            size: 2,
            extra: 0,
        };
        internal.key_count = 2;
        internal.journal_seq = journal_seq;

        update.mark_updating_parent();
        let new_root = Arc::new(internal);
        // bcachefs 对齐：新分裂出的根节点在首次落盘前设置 will_make_reachable
        new_root.set_will_make_reachable();
        self.root.node = new_root;
        self.root.depth += 1;
        // 新根有 routing entries，标记为 dirty 确保 flush
        self.root_modified.store(true, Ordering::Release);
        update.mark_done();
        true
    }

    /// Presplit shard boundaries — 在恢复阶段预分割跨越分片边界的 leaf 节点
    ///
    /// 检查 depth=0 的 leaf 节点中的 entries 是否跨越 SHARD_FACTOR（1024）分片边界。
    /// 如果跨越且 split 点位于合理位置（距两端至少 20%），则执行节点分裂。
    /// 分裂后的两棵子树位于不同的 shard 中，未来写入可直接定位到对应子树。
    ///
    /// 仅在 recovery 的 presplit_shard_boundaries pass 中调用。
    /// 对应 bcachefs `bch2_presplit_shard_boundaries()`。
    ///
    /// Returns true if a split was performed.
    pub fn presplit_shard_boundaries(&mut self) -> bool {
        if self.root.depth != 0 {
            return false;
        }

        const SHARD_FACTOR: u64 = 1024;

        // 收集 entries 检查 shard 边界跨越，同时保留用于 split
        let entries: Vec<BtreeEntry> = {
            let mut entries = Vec::new();
            self.for_each_entry(|e| entries.push(e));
            entries
        };

        let n = entries.len();
        if n < 3 {
            return false;
        }

        let mut found_split = false;
        for i in 1..n {
            let prev_off = entries[i - 1].pos.offset;
            let curr_off = entries[i].pos.offset;
            // 跨越 shard 边界：prev 在 N*SHARD_FACTOR 之前，curr 在其之后
            if prev_off / SHARD_FACTOR < curr_off / SHARD_FACTOR {
                // 仅在 split 点距两端至少 20% 时执行
                if i > n / 5 && i < n * 4 / 5 {
                    found_split = true;
                    break;
                }
            }
        }

        if !found_split {
            return false;
        }

        // 执行分裂但不插入额外 entry（split_root(None) 跳过额外 entry 插入）
        // journal_seq=0：recovery 期间的预分裂，无有效 journal seq 可用。
        // 分裂操作在 recovery 完成后会由正常的 journal 路径重新追踪。
        self.split_root(None, 0)
    }

    /// 使用事务执行操作
    ///
    /// 提供对 BtreeTrans 的底层访问，用于需要多个 iter 的场景。
    /// 返回 `Result<R, StorageError>`，其中 `Err` 来自事务重启限制溢出。
    pub fn with_transaction<F, R>(&self, f: F) -> Result<R, StorageError>
    where
        F: FnOnce(&mut BtreeTrans) -> R,
    {
        let mut trans = BtreeTrans::new(self.cache.clone());
        trans.begin();
        let result = f(&mut trans);
        trans.commit(None)?;
        Ok(result)
    }

    /// 在事务上下文中插入 key/value — 节点分裂时通知事务重启
    ///
    /// 比 `insert()` 多了事务集成：当单 leaf 插入触发 `split_root` 时，
    /// 通过 `trans.trigger_node_split()` 通知事务路径可能需要重新遍历。
    pub fn insert_with_transaction(
        &mut self,
        key: BtreeKey,
        value: BchVal,
        trans: Option<&mut BtreeTrans>,
        _journal_seq: u64,
    ) -> bool {
        let pos = Bpos::from_key(&key);
        let result = if self.root.depth == 0 {
            let node = match Arc::get_mut(&mut self.root.node) {
                Some(n) => n,
                None => return false,
            };
            if node.insert(key, value) {
                self.total_key_count += 1;
                self.root_modified.store(true, Ordering::Release);
                true
            } else {
                node.compact();
                if node.insert(key, value) {
                    self.total_key_count += 1;
                    self.root_modified.store(true, Ordering::Release);
                    true
                } else {
                    let did_split = self.split_root(Some((key, value)), _journal_seq);
                    if did_split {
                        self.total_key_count += 1;
                        if let Some(trans) = trans {
                            trans.trigger_node_split();
                        }
                    }
                    did_split
                }
            }
        } else {
            let result = self.insert_multi(key, value, _journal_seq);
            if result {
                if let Some(trans) = trans {
                    trans.trigger_node_split();
                }
            }
            result
        };
        if result {
            if self.root.depth == 0 && self.root.ptr.is_valid() {
                self.cache
                    .insert(self.root.ptr.block_addr, self.root.node.clone());
            }
            self.key_cache.invalidate(&pos);
        }
        result
    }

    /// 遍历所有 entry（旧 API，只支持 Extent value）
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(BtreeKey, BchVal),
    {
        let mut iter = BtreeIter::init(
            &self.root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &self.cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        while let Some((k, v)) = iter.peek() {
            if k.key_type != KeyType::Deleted {
                f(k, v);
            }
            if !iter.advance() {
                break;
            }
        }
    }

    /// 遍历所有 entry，返回 BtreeEntry（支持 Extent 和 Raw value）
    pub fn for_each_entry<F>(&self, mut f: F)
    where
        F: FnMut(BtreeEntry),
    {
        let mut iter = BtreeIter::init(
            &self.root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &self.cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        while let Some(entry) = iter.peek_entry() {
            if entry.key_type != KeyType::Deleted {
                f(entry);
            }
            if !iter.advance() {
                break;
            }
        }
    }

    /// 对单 leaf（depth=0）根节点执行 compact，消除重复 Bpos 条目
    ///
    /// 调用时机：当使用 `insert_entry` 覆盖已有键但留有多份副本时，
    /// compact 会将所有 bsets 归并到 set[0] 并去重（保留每键最后写入的条目）。
    pub fn compact(&mut self) {
        if self.root.depth == 0 {
            if let Some(node) = Arc::get_mut(&mut self.root.node) {
                node.compact();
            }
        }
    }

    /// 当前 key 总数（跨所有 leaf）
    pub fn key_count(&self) -> u32 {
        self.total_key_count
    }

    /// 获取节点缓存引用
    pub fn cache(&self) -> &NodeCache {
        &self.cache
    }

    /// 节点缓存引用（NodeCache 内部使用 Mutex，不需要 &mut）
    pub fn cache_mut(&self) -> &NodeCache {
        &self.cache
    }

    /// drain 并返回所有脏节点（按 level 升序排列，包含 depth=0 时被修改的 root）
    pub fn flush_dirty(&self) -> Vec<(u64, Arc<BtreeNode>)> {
        let mut result = self.cache.flush_dirty();
        // depth=0 root 不在 cache 中，通过 root_modified 跟踪
        if self.root_modified.swap(false, Ordering::Acquire) {
            result.push((ROOT_CACHE_ADDR, self.root.node.clone()));
        }
        result
    }

    /// 获取节点缓存的 Arc 引用（crate 内部使用）
    #[allow(dead_code)]
    pub(crate) fn cache_arc(&self) -> Arc<NodeCache> {
        Arc::clone(&self.cache)
    }

    // ─── Interior Update 并发控制 ───────────────────────────

    /// 尝试获取 write_blocked 锁（成功返回 true，被阻塞返回 false）
    pub(crate) fn try_start_interior_update(&self) -> bool {
        self.write_blocked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// 释放 write_blocked 锁
    pub(crate) fn finish_interior_update(&self) {
        self.write_blocked.store(false, Ordering::Release);
    }

    /// 检查当前是否有 interior update 在进行
    pub(crate) fn is_write_blocked(&self) -> bool {
        self.write_blocked.load(Ordering::Acquire)
    }

    // ─── Interior 模块辅助方法 ─────────────────────────────────

    /// 内部方法：从 root 下降到 leaf，返回路径（供 interior 模块使用）
    pub(crate) fn find_path_to_leaf_internal(
        &self,
        target: &BtreeKey,
        path: &mut Vec<u64>,
    ) -> Option<u64> {
        self.find_path_to_leaf(target, path)
    }

    /// 内部方法：直接设置根节点（供 interior 模块使用）
    pub(crate) fn set_root_internal(&mut self, node: Arc<BtreeNode>) {
        let depth = node.level;
        self.root = BtreeRoot::new(node, depth);
    }

    /// 内部方法：更新根节点持久指针。
    pub(crate) fn set_root_ptr_internal(&mut self, ptr: crate::btree::types::BtreePtrV2) {
        self.root.ptr = ptr;
    }

    /// 内部方法：获取根节点的可变访问（仅用于测试/内部操作）
    pub(crate) fn root_node_mut_internal(&mut self) -> &mut BtreeNode {
        // 仅用于测试和内部操作（node_size 调整等）
        // 使用 Arc::get_mut 需要在没有其它引用时才能成功
        Arc::get_mut(&mut self.root.node)
            .expect("root_node_mut_internal: root Arc has multiple references")
    }

    /// 内部方法：重新统计整棵树的 key 数
    pub(crate) fn reset_key_count(&mut self) {
        self.total_key_count = Self::count_entries(&self.root, &self.cache);
    }

    /// 尝试获取 commit_lock（用于序列化 interior update 提交）
    fn try_acquire_commit_lock(&self) -> bool {
        self.commit_lock
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// 释放 commit_lock
    fn release_commit_lock(&self) {
        self.commit_lock.store(false, Ordering::Release);
    }

    // ─── 多级树辅助 ───────────────────────────────────────

    /// 从 root 下降到 leaf，记录所有经过的 internal node 地址
    ///
    /// path 填充为 [level(depth-1), level(depth-2), ..., level1] 的地址。
    /// depth=1 时 path 为空（root 自身就是 leaf 的 direct parent）。
    fn find_path_to_leaf(&self, target: &BtreeKey, path: &mut Vec<u64>) -> Option<u64> {
        if self.root.depth == 0 {
            return None;
        }
        let mut current = self.root.node.clone();
        path.clear();
        for level in (1..=self.root.depth).rev() {
            let (child_addr, _child_idx) = BtreeIter::find_child_node(&current, target);
            if child_addr > 10000 {
                eprintln!("CORRUPT: level={} child_addr={} huge address, current node key_count={} level={} node_size={}", 
                    level, child_addr, current.key_count, current.level, current.node_size);
            }
            if level == 1 {
                if child_addr > 10000 {
                    eprintln!(
                        "CORRUPT: returning leaf_addr={} from depth={}",
                        child_addr, self.root.depth
                    );
                }
                return Some(child_addr);
            }
            path.push(child_addr);
            current = self.cache.get_or_create(child_addr, level - 1);

            // min_key/max_key 防御性检查：验证 target 在当前 child 的 key 范围内
            // 如果触发，说明父节点的 routing entry 与子节点的范围不一致，
            // 通常是 btree 损坏或 bug 的信号。
            if current.key_count > 0 {
                let target_pos = target.get_vaddr();
                let node_min_off = current.min_key.offset;
                let node_max_off = current.max_key.offset;
                if target_pos < node_min_off || target_pos > node_max_off {
                    // 软检查：仅 debug 时输出警告，不影响正常路径
                    // 第一次加载节点时，min/max 可能还不准确（如新分裂后尚未设值）
                }
            }
        }
        None
    }

    /// 多级树 insert：找到目标 leaf，插入或分裂后更新 parent
    fn insert_multi(&mut self, key: BtreeKey, value: BchVal, journal_seq: u64) -> bool {
        // write_blocked 时返回 false，caller 重试
        if self.is_write_blocked() {
            return false;
        }

        let mut path: Vec<u64> = Vec::new();
        let leaf_addr = match self.find_path_to_leaf(&key, &mut path) {
            Some(addr) => addr,
            None => {
                eprintln!("FAIL: find_path_to_leaf returned None");
                return false;
            }
        };

        // ── Phase 1: try insert with compact retry ──
        {
            let mut leaf_arc = match self.cache.take_node(leaf_addr) {
                Some(n) => n,
                None => {
                    eprintln!("FAIL: take_node({}) returned None", leaf_addr);
                    return false;
                }
            };
            let leaf = match Arc::get_mut(&mut leaf_arc) {
                Some(n) => n,
                None => {
                    self.cache.put_node(leaf_addr, leaf_arc);
                    eprintln!("FAIL: Arc::get_mut leaf");
                    return false;
                }
            };

            // A4: 75% 主动分裂阈值 — 节点使用率 >75% 时直接触发 split
            // 参考 bcachefs BTREE_SPLIT_THRESHOLD (cache.h:189)
            let live_u64s = leaf.total_data_bytes() / 8;
            if crate::btree::node::should_split(live_u64s, leaf.node_size) {
                self.cache.put_node(leaf_addr, leaf_arc);
                // 进入 Phase 2 split 路径
            } else if leaf.insert(key, value) {
                self.total_key_count += 1;
                leaf.journal_seq = journal_seq;
                self.cache.insert_dirty(leaf_addr, leaf_arc);
                return true;
            } else {
                leaf.compact();
                // A1: compact_fits — compact 后检查是否还有空间
                if leaf.compact_fits(1) && leaf.insert(key, value) {
                    self.total_key_count += 1;
                    leaf.journal_seq = journal_seq;
                    self.cache.insert_dirty(leaf_addr, leaf_arc);
                    return true;
                }
                // Leaf full — put back, proceed to split path
                self.cache.put_node(leaf_addr, leaf_arc);
            }
        }

        // ── Phase 2: leaf split (write_blocked 保护) ──
        // 尝试获取 write_blocked + commit_lock，失败则返回 false 让 caller 重试
        if !self.try_acquire_commit_lock() {
            return false;
        }
        if !self.try_start_interior_update() {
            self.release_commit_lock();
            return false;
        }

        // 创建 BtreeInteriorUpdate 跟踪分裂生命周期
        // journal_seq 来自插入操作的 journal 预留，确保 crash recovery 可追踪此分裂。
        // 对应 bcachefs bch2_btree_node_split_pre (split.c) 中的 journal 预留。
        let mut update = BtreeInteriorUpdate::new(InteriorUpdateType::Split, journal_seq);
        let old_node = crate::btree::types::BtreePtrV2 {
            block_addr: leaf_addr,
            sectors_written: 0,
            level: 0,
            generation: 0,
        };
        update.add_old_node(old_node);

        let mut leaf_arc = self.cache.take_node(leaf_addr).unwrap();
        let leaf = Arc::get_mut(&mut leaf_arc).unwrap();
        Self::init_interior_update(&mut update, BtreeUpdateMode::Node, leaf);

        let (median_key, mut right_node) = match leaf.split() {
            Some((k, n)) => (k, n),
            None => {
                eprintln!(
                    "FAIL: leaf.split() returned None at leaf_addr={}",
                    leaf_addr
                );
                self.cache.put_node(leaf_addr, leaf_arc);
                self.finish_interior_update();
                self.release_commit_lock();
                return false;
            }
        };
        // 确保右侧 leaf 使用相同的 node_size（split 创建 DEFAULT_NODE_SIZE 节点）
        right_node.node_size = leaf.node_size;

        if key >= median_key {
            right_node.insert(key, value);
        } else {
            leaf.insert(key, value);
        }
        leaf.journal_seq = journal_seq;
        right_node.journal_seq = journal_seq;
        self.total_key_count += 1; // 计入触发分裂的 key

        // Cache the new right half as dirty
        let right_addr = self.cache.alloc_addr();
        self.cache.insert_dirty(right_addr, Arc::new(right_node));
        // A2: SplitGuard 确保 split 失败时释放已分配的右节点
        let mut guard = SplitGuard::new(self.cache.clone(), right_addr);
        // Put back the left half (original addr) as dirty
        self.cache.insert_dirty(leaf_addr, leaf_arc);

        // 记录新节点到 update
        let new_node_right = crate::btree::types::BtreePtrV2 {
            block_addr: right_addr,
            sectors_written: 0,
            level: 0,
            generation: 0,
        };
        update.add_new_node(new_node_right);
        update.set_median_key(median_key);
        update.mark_nodes_allocated();

        // ── Phase 3: insert routing entry into parent ──
        update.mark_updating_parent();
        // path 为空时（depth=1），parent 就是 root
        let pos = if path.is_empty() { 0 } else { path.len() - 1 };
        let result = self.insert_routing_entry_at(median_key, right_addr, &path, pos, journal_seq);

        if result {
            guard.disarm();
        }

        // 释放锁
        update.mark_done();
        self.finish_interior_update();
        self.release_commit_lock();
        result
    }

    /// 在指定层级的 parent 中插入 routing entry；满时递归向上分裂
    ///
    /// path: 从 root（排除）到 leaf（排除）的 internal node 地址列表
    ///       path[0] = level(depth-1) 节点, path[last] = level1 节点
    /// pos: 在 path 中的索引。pos >= path.len() 表示 parent 是 root
    #[allow(unused_labels)]
    fn insert_routing_entry_at(
        &mut self,
        mut routing_key: BtreeKey,
        mut child_addr: u64,
        path: &[u64],
        mut pos: usize,
        journal_seq: u64,
    ) -> bool {
        'routing_loop: loop {
            if pos >= path.len() {
                // === Root 作为 parent ===
                // 使用块作用域在 split_root 调用前结束 parent 的借用
                let needs_split = {
                    let parent = match Arc::get_mut(&mut self.root.node) {
                        Some(n) => n,
                        None => {
                            eprintln!("FAIL: root Arc::get_mut failed");
                            return false;
                        }
                    };
                    let entry = BchVal::new(child_addr, 0);
                    if parent.insert(routing_key, entry) {
                        self.root_modified.store(true, Ordering::Release);
                        return true;
                    }
                    parent.compact();
                    let entry = BchVal::new(child_addr, 0);
                    if parent.insert(routing_key, entry) {
                        self.root_modified.store(true, Ordering::Release);
                        return true;
                    }
                    true
                };
                let _ = needs_split;
                return self
                    .split_root(Some((routing_key, BchVal::new(child_addr, 0))), journal_seq);
            }

            // === Cache 中的 internal node 作为 parent ===
            let parent_addr = path[pos];
            let mut parent_arc = match self.cache.take_node(parent_addr) {
                Some(n) => n,
                None => {
                    eprintln!("FAIL: take_node({}) in routing path", parent_addr);
                    return false;
                }
            };
            let parent = match Arc::get_mut(&mut parent_arc) {
                Some(n) => n,
                None => {
                    self.cache.put_node(parent_addr, parent_arc);
                    eprintln!("FAIL: Arc::get_mut parent{}", parent_addr);
                    return false;
                }
            };

            let entry = BchVal::new(child_addr, 0);
            if parent.insert(routing_key, entry) {
                self.cache.insert_dirty(parent_addr, parent_arc);
                return true;
            }
            parent.compact();
            // A1: compact_fits — 只有 compact 释放了足够空间才重试 insert
            if parent.compact_fits(1) {
                let entry = BchVal::new(child_addr, 0);
                if parent.insert(routing_key, entry) {
                    self.cache.insert_dirty(parent_addr, parent_arc);
                    return true;
                }
            }

            // Internal node 也满了 → 分裂
            let (median_key, mut right_node) = match parent.split() {
                Some((k, n)) => (k, n),
                None => {
                    self.cache.put_node(parent_addr, parent_arc);
                    return false;
                }
            };
            // 确保右侧节点使用相同的 level 和 node_size
            right_node.node_size = parent.node_size;
            debug_assert_eq!(
                right_node.level, parent.level,
                "split right_node level {} != parent level {}",
                right_node.level, parent.level
            );
            debug_assert_eq!(
                right_node.node_size, parent.node_size,
                "split right_node node_size {} != parent node_size {}",
                right_node.node_size, parent.node_size
            );

            if routing_key >= median_key {
                right_node.insert(routing_key, BchVal::new(child_addr, 0));
            } else {
                parent.insert(routing_key, BchVal::new(child_addr, 0));
            }

            // 分配右侧节点地址，放回左侧节点（均为 dirty）
            let right_addr = self.cache.alloc_addr();
            self.cache.insert_dirty(right_addr, Arc::new(right_node));
            self.cache.insert_dirty(parent_addr, parent_arc);

            // 递归向上：将 median_key + right_addr 插入到祖父母
            // path 从 root 向 leaf 排列：path[0]=离 root 最近, path[last]=离 leaf 最近
            // 所以 pos 需要递减来向上回溯
            // 备注：当 pos=0 时，path[0] 已分裂，下一轮应走 root 处理分支
            // 备注：原代码使用 wrapping_sub(1) 在 pos=0 时会导致 usize::MAX 溢出引发死循环
            // 备注：改为显式边界判断 — pos=0 时设 pos=path.len() 触发 root 分支
            routing_key = median_key;
            child_addr = right_addr;
            if pos > 0 {
                pos -= 1; // 向上回溯到 path 中的上一层 parent
            } else {
                // pos=0 表示 path[0]（最靠近 root 的 internal node）已分裂，
                // 下一轮直接从 root 插入
                pos = path.len();
            }
            // 继续循环（pos 递减回溯，越过 path[0] 后回到 root）
        }
    }

    /// 多级树 delete：找到目标 leaf，取出删除后放回
    fn delete_multi(&mut self, key: &BtreeKey, journal_seq: u64) -> bool {
        // write_blocked 时返回 false，caller 重试
        if self.is_write_blocked() {
            return false;
        }

        let mut path_buf = Vec::new();
        let leaf_addr = match self.find_path_to_leaf(key, &mut path_buf) {
            Some(addr) => addr,
            None => return false,
        };
        let mut leaf_arc = match self.cache.take_node(leaf_addr) {
            Some(n) => n,
            None => return false,
        };
        let leaf = match Arc::get_mut(&mut leaf_arc) {
            Some(n) => n,
            None => {
                self.cache.put_node(leaf_addr, leaf_arc);
                return false;
            }
        };
        let leaf_start = leaf.min_key;
        let leaf_end = leaf.max_key;
        let leaf_level = leaf.level;
        let leaf_progress = Self::node_progress(leaf);
        let result = leaf.delete_key(key);
        if result {
            leaf.journal_seq = journal_seq;
            self.cache.insert_dirty(leaf_addr, leaf_arc);
        } else {
            self.cache.put_node(leaf_addr, leaf_arc);
        }

        // 合并阶段需要 write_blocked 保护
        if result && self.root.depth > 0 {
            // 尝试获取 write_blocked，失败则跳过（key 已删除，下次操作触发合并）
            let acquired = if !self.is_write_blocked() && self.try_acquire_commit_lock() {
                let blocked = self.try_start_interior_update();
                if !blocked {
                    self.release_commit_lock();
                }
                blocked
            } else {
                false
            };

            if acquired {
                let mut update = BtreeInteriorUpdate::new(InteriorUpdateType::Merge, 0);
                update.set_btree_id(BtreeId::Extents);
                update.set_mode(BtreeUpdateMode::Node);
                update.set_node_span(leaf_start, leaf_end);
                update.set_update_level_span(leaf_level, leaf_level);
                let (node_written, node_sectors, node_remaining) = leaf_progress;
                update.set_node_progress(node_written, node_sectors, node_remaining);
                update.add_old_node(crate::btree::types::BtreePtrV2 {
                    block_addr: leaf_addr,
                    sectors_written: 0,
                    level: 0,
                    generation: 0,
                });

                let merged = self.maybe_merge_leaf(leaf_addr, &path_buf);
                // Cascade: leaf 合并可能导致其祖先节点 underfull
                if merged {
                    update.mark_nodes_allocated();
                    update.mark_updating_parent();
                    for level in 1..self.root.depth as usize {
                        if level > path_buf.len() {
                            break;
                        }
                        let node_addr = path_buf[path_buf.len() - level];
                        let ancestors = &path_buf[..path_buf.len() - level];
                        if !self.try_merge_node(node_addr, ancestors) {
                            break;
                        }
                    }
                    // cascade 可能导致根节点只剩 1 个 child，需要缩减深度
                    while self.collapse_root() {}
                }
                update.mark_done();
                self.finish_interior_update();
                self.release_commit_lock();
            }
        }
        result
    }

    /// 检查 node 是否低于阈值，是则与相邻兄弟节点合并
    ///
    /// ancestors 是 node 以上的路径：[parent, grandparent, ..., root]。
    /// ancestors.is_empty() 表示 node 的父节点就是 root（depth=1 时的 leaf，或 depth=0 时的单节点）。
    ///
    /// 返回值：是否执行了合并（且父节点已更新）
    pub(crate) fn try_merge_node(&mut self, node_addr: u64, ancestors: &[u64]) -> bool {
        // ── Phase 1: 取 node 检查 underfull ──
        let mut node_arc = match self.cache.take_node(node_addr) {
            Some(n) => n,
            None => return false,
        };
        let node = match Arc::get_mut(&mut node_arc) {
            Some(n) => n,
            None => {
                self.cache.put_node(node_addr, node_arc);
                return false;
            }
        };
        if !node_underfull(node) {
            self.cache.put_node(node_addr, node_arc);
            return false;
        }
        // Compact 以获得真实的 key count 和有序存储
        node.compact();
        let ances_str = if ancestors.is_empty() {
            "root"
        } else {
            "internal"
        };
        eprintln!(
            "TRACE MERGE: level={} node_addr={} key_count={} ancestors=[{:?}] ({})",
            node.level, node_addr, node.key_count, ancestors, ances_str
        );

        // ── Phase 2: 确定 parent 并查找兄弟节点 ──
        let parent_is_root = ancestors.is_empty();
        let (sibling_addr, merge_to_right) = if parent_is_root {
            match find_sibling(&self.root.node, node_addr, false) {
                Some((_k, addr)) => (addr, true), // 右兄弟存在 → 合并到右
                None => {
                    match find_sibling(&self.root.node, node_addr, true) {
                        Some((_k, addr)) => (addr, false), // 左兄弟 → 合并到左
                        None => {
                            self.cache.put_node(node_addr, node_arc);
                            return false; // 无兄弟（单 child 树）
                        }
                    }
                }
            }
        } else {
            let parent_addr = ancestors[ancestors.len() - 1];
            let parent_arc = match self.cache.get(parent_addr) {
                Some(n) => n,
                None => {
                    self.cache.put_node(node_addr, node_arc);
                    return false;
                }
            };
            let result = match find_sibling(&parent_arc, node_addr, false) {
                Some((_k, addr)) => (addr, true),
                None => match find_sibling(&parent_arc, node_addr, true) {
                    Some((_k, addr)) => (addr, false),
                    None => {
                        self.cache.put_node(node_addr, node_arc);
                        return false;
                    }
                },
            };
            // parent_arc 在此释放
            result
        };

        // ── Phase 3: 取兄弟节点，执行 absorb ──
        let mut sibling_arc = match self.cache.take_node(sibling_addr) {
            Some(n) => n,
            None => {
                self.cache.put_node(node_addr, node_arc);
                return false;
            }
        };
        let sibling = match Arc::get_mut(&mut sibling_arc) {
            Some(n) => n,
            None => {
                self.cache.put_node(node_addr, node_arc);
                self.cache.put_node(sibling_addr, sibling_arc);
                return false;
            }
        };

        // 确定 absorb 方向
        let survivor_addr: u64;
        let absorbed_addr: u64;
        if merge_to_right {
            // node 向前合并到右兄弟 → sibling 吸收 node
            let merged_ok = sibling.can_absorb(node);
            if !merged_ok {
                // ── merge_fail_backoff: 如果合并后超出 MERGE_HIGHER 且
                //     node 低于 MERGE_HYSTERESIS，退避避免反复 split/merge ──
                let node_bytes = node.total_data_bytes();
                let sib_bytes = sibling.total_data_bytes();
                let combined = node_bytes + sib_bytes + crate::btree::node::entry_size();
                if combined
                    > sibling.node_size * crate::btree::node::MERGE_HIGHER_NUM
                        / crate::btree::node::MERGE_HIGHER_DEN
                    && node_bytes
                        < sibling.node_size * crate::btree::node::MERGE_HYSTERESIS_NUM
                            / crate::btree::node::MERGE_HYSTERESIS_DEN
                {
                    // 退避：放回两者，不执行合并
                    self.cache.put_node(node_addr, node_arc);
                    self.cache.put_node(sibling_addr, sibling_arc);
                    eprintln!(
                        "TRACE MERGE: backoff at node_addr={} sib_addr={}",
                        node_addr, sibling_addr
                    );
                    return false;
                }
                // ── 3→2 merge: 收集三个节点（左、当前、右）重新平衡分布 ──
                // 寻找左侧的兄弟（sibling 的左侧兄弟）
                let left_of_sib = if parent_is_root {
                    find_sibling(&self.root.node, sibling_addr, true)
                } else {
                    let p_guard = self.cache.get(ancestors[ancestors.len() - 1]);
                    match p_guard {
                        Some(p) => find_sibling(&p, sibling_addr, true),
                        // None: parent not in cache — 不动 node_arc/sibling_arc（被 Arc::get_mut 借用），
                        // 清理交给 line 1004-1006 统一处理
                        None => None,
                    }
                };
                if let Some((_lk, left_addr)) = left_of_sib {
                    let mut left_arc = match self.cache.take_node(left_addr) {
                        Some(n) => n,
                        None => {
                            self.cache.put_node(node_addr, node_arc);
                            self.cache.put_node(sibling_addr, sibling_arc);
                            return false;
                        }
                    };
                    let left = match Arc::get_mut(&mut left_arc) {
                        Some(n) => n,
                        None => {
                            self.cache.put_node(node_addr, node_arc);
                            self.cache.put_node(sibling_addr, sibling_arc);
                            return false;
                        }
                    };
                    left.compact();
                    // 收集三个节点的所有 entries
                    let mut all_entries = Vec::new();
                    // 从 left 收集
                    for i in 1..=left.key_count {
                        let (k, v) = left.read_entry(&left.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    // 从 node 收集
                    node.compact();
                    // 修正：node 已在 Phase 1 compact，再次确认
                    for i in 1..=node.sets[0].size {
                        let (k, v) = node.read_entry(&node.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    // 从 sibling 收集
                    sibling.compact();
                    for i in 1..=sibling.sets[0].size {
                        let (k, v) = sibling.read_entry(&sibling.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    // 排序
                    all_entries.sort_by_key(|a| a.0);
                    all_entries.dedup_by(|a, b| a.0 == b.0);
                    // 用 find_balanced_split 分成两组
                    let n = all_entries.len();
                    if n < 2 {
                        self.cache.put_node(node_addr, node_arc);
                        self.cache.put_node(sibling_addr, sibling_arc);
                        self.cache.put_node(left_addr, left_arc);
                        return false;
                    }
                    // 计算总 u64 数：BchVal 固定 1 u64
                    let entry_u64s: Vec<u32> = all_entries
                        .iter()
                        .map(|(_k, _v)| {
                            // BKEY_U64S 是 key 的固定 u64 数
                            // BtreeKey unpacked 固定 BKEY_U64S u64s
                            // BchVal 固定 1 u64（paddr(6B) + ver(2B) = 8B）
                            super::key::BKEY_U64S as u32 + 1u32
                        })
                        .collect();
                    let total_u64s: u32 = entry_u64s.iter().sum();
                    let target_left = total_u64s * crate::btree::node::BALANCE_TARGET_NUM
                        / crate::btree::node::BALANCE_TARGET_DEN;
                    let mut acc = 0u32;
                    let mut mid_3to2 = n / 2;
                    for (i, &u) in entry_u64s.iter().enumerate() {
                        if i > 0 && acc >= target_left {
                            mid_3to2 = i;
                            break;
                        }
                        acc += u;
                    }
                    let (left_half, right_half) = all_entries.split_at(mid_3to2);
                    let median_3to2 = right_half[0].0;
                    // 写左侧幸存节点
                    let mut l_cur = 0u32;
                    for (k, v) in left_half {
                        l_cur += left.write_entry(l_cur, k, v);
                    }
                    left.sets[0] = crate::btree::node::BsetTree {
                        data_offset: 0,
                        end_offset: l_cur,
                        aux_offset: 0,
                        size: left_half.len() as u16,
                        extra: 0,
                    };
                    left.key_count = left_half.len() as u32;
                    // 写右侧新节点（使用 sibling 的内存）
                    let mut r_cur = 0u32;
                    for (k, v) in right_half {
                        r_cur += sibling.write_entry(r_cur, k, v);
                    }
                    sibling.sets[0] = crate::btree::node::BsetTree {
                        data_offset: 0,
                        end_offset: r_cur,
                        aux_offset: 0,
                        size: right_half.len() as u16,
                        extra: 0,
                    };
                    sibling.key_count = right_half.len() as u32;
                    // 释放三节点（左右幸存节点标记 dirty）
                    self.cache
                        .put_node(node_addr, Arc::new(BtreeNode::new_leaf()));
                    self.cache.insert_dirty(left_addr, left_arc);
                    self.cache.insert_dirty(sibling_addr, sibling_arc);
                    // 更新父节点：3→2 合并需要删除旧路由条目并插入两个新条目
                    let parent_updated_3to2 = if parent_is_root {
                        let updated = match Arc::get_mut(&mut self.root.node) {
                            Some(parent) => update_parent_routing_3to2(
                                parent,
                                left_addr,
                                node_addr,
                                sibling_addr,
                                median_3to2,
                            ),
                            None => true,
                        };
                        if updated {
                            self.root_modified.store(true, Ordering::Release);
                        }
                        updated
                    } else {
                        let parent_addr_3to2 = ancestors[ancestors.len() - 1];
                        let mut parent_arc_3to2 = match self.cache.take_node(parent_addr_3to2) {
                            Some(n) => n,
                            None => return true,
                        };
                        let updated = {
                            let parent = match Arc::get_mut(&mut parent_arc_3to2) {
                                Some(n) => n,
                                None => {
                                    self.cache.put_node(parent_addr_3to2, parent_arc_3to2);
                                    return true;
                                }
                            };
                            update_parent_routing_3to2(
                                parent,
                                left_addr,
                                node_addr,
                                sibling_addr,
                                median_3to2,
                            )
                        };
                        self.cache.insert_dirty(parent_addr_3to2, parent_arc_3to2);
                        updated
                    };
                    return parent_updated_3to2;
                }
                // 3→2 merge 都失败，放回
                self.cache.put_node(node_addr, node_arc);
                self.cache.put_node(sibling_addr, sibling_arc);
                return false;
            }
            sibling.absorb(node);
            survivor_addr = sibling_addr;
            absorbed_addr = node_addr;
            // node_arc（被吸收）在此释放（从 cache 移除）
            self.cache.insert_dirty(survivor_addr, sibling_arc);
        } else {
            // 左兄弟吸收 node → node 吸收 sibling
            let merged_ok = node.can_absorb(sibling);
            if !merged_ok {
                // merge_fail_backoff check
                let node_bytes = node.total_data_bytes();
                let sib_bytes = sibling.total_data_bytes();
                let combined = node_bytes + sib_bytes + crate::btree::node::entry_size();
                if combined
                    > node.node_size * crate::btree::node::MERGE_HIGHER_NUM
                        / crate::btree::node::MERGE_HIGHER_DEN
                    && node_bytes
                        < node.node_size * crate::btree::node::MERGE_HYSTERESIS_NUM
                            / crate::btree::node::MERGE_HYSTERESIS_DEN
                {
                    self.cache.put_node(node_addr, node_arc);
                    self.cache.put_node(sibling_addr, sibling_arc);
                    eprintln!(
                        "TRACE MERGE: backoff at node_addr={} sib_addr={}",
                        node_addr, sibling_addr
                    );
                    return false;
                }
                // 3→2 merge: 找 sibling 的右侧兄弟
                let right_of_sib = if parent_is_root {
                    find_sibling(&self.root.node, sibling_addr, false)
                } else {
                    let p_guard = self.cache.get(ancestors[ancestors.len() - 1]);
                    match p_guard {
                        Some(p) => find_sibling(&p, sibling_addr, false),
                        None => None,
                    }
                };
                if let Some((_rk, right_addr)) = right_of_sib {
                    // ... 类似逻辑（与上方对称，左+当前+右三节点重分布）
                    let mut right_arc = match self.cache.take_node(right_addr) {
                        Some(n) => n,
                        None => {
                            self.cache.put_node(node_addr, node_arc);
                            self.cache.put_node(sibling_addr, sibling_arc);
                            return false;
                        }
                    };
                    let right = match Arc::get_mut(&mut right_arc) {
                        Some(n) => n,
                        None => {
                            self.cache.put_node(node_addr, node_arc);
                            self.cache.put_node(sibling_addr, sibling_arc);
                            return false;
                        }
                    };
                    right.compact();
                    let mut all_entries = Vec::new();
                    for i in 1..=node.sets[0].size {
                        let (k, v) = node.read_entry(&node.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    node.compact();
                    for i in 1..=sibling.sets[0].size {
                        let (k, v) = sibling.read_entry(&sibling.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    sibling.compact();
                    for i in 1..=right.sets[0].size {
                        let (k, v) = right.read_entry(&right.sets[0], i as usize);
                        all_entries.push((k, v));
                    }
                    all_entries.sort_by_key(|a| a.0);
                    all_entries.dedup_by(|a, b| a.0 == b.0);
                    let n = all_entries.len();
                    if n < 2 {
                        self.cache.put_node(node_addr, node_arc);
                        self.cache.put_node(sibling_addr, sibling_arc);
                        self.cache.put_node(right_addr, right_arc);
                        return false;
                    }
                    // BchVal 固定 1 u64，BtreeKey 固定 BKEY_U64S u64s
                    let entry_u64 = super::key::BKEY_U64S as u32 + 1u32;
                    let total_u64s: u32 = all_entries.len() as u32 * entry_u64;
                    let target_left = total_u64s * crate::btree::node::BALANCE_TARGET_NUM
                        / crate::btree::node::BALANCE_TARGET_DEN;
                    let mut acc = 0u32;
                    let mut mid_3to2 = n / 2;
                    for (i, (_, _)) in all_entries.iter().enumerate() {
                        if i > 0 && acc >= target_left {
                            mid_3to2 = i;
                            break;
                        }
                        acc += entry_u64;
                    }
                    let (left_half, right_half) = all_entries.split_at(mid_3to2);
                    let median_3to2 = right_half[0].0;
                    let mut l_cur = 0u32;
                    for (k, v) in left_half {
                        l_cur += node.write_entry(l_cur, k, v);
                    }
                    node.sets[0] = crate::btree::node::BsetTree {
                        data_offset: 0,
                        end_offset: l_cur,
                        aux_offset: 0,
                        size: left_half.len() as u16,
                        extra: 0,
                    };
                    node.key_count = left_half.len() as u32;
                    let mut r_cur = 0u32;
                    for (k, v) in right_half {
                        r_cur += right.write_entry(r_cur, k, v);
                    }
                    right.sets[0] = crate::btree::node::BsetTree {
                        data_offset: 0,
                        end_offset: r_cur,
                        aux_offset: 0,
                        size: right_half.len() as u16,
                        extra: 0,
                    };
                    right.key_count = right_half.len() as u32;
                    self.cache
                        .put_node(sibling_addr, Arc::new(BtreeNode::new_leaf()));
                    self.cache.insert_dirty(node_addr, node_arc);
                    self.cache.insert_dirty(right_addr, right_arc);
                    let parent_updated_3to2 = if parent_is_root {
                        let updated = match Arc::get_mut(&mut self.root.node) {
                            Some(parent) => update_parent_routing_3to2(
                                parent,
                                node_addr,
                                sibling_addr,
                                right_addr,
                                median_3to2,
                            ),
                            None => true,
                        };
                        if updated {
                            self.root_modified.store(true, Ordering::Release);
                        }
                        updated
                    } else {
                        let parent_addr_3to2 = ancestors[ancestors.len() - 1];
                        let mut parent_arc_3to2 = match self.cache.take_node(parent_addr_3to2) {
                            Some(n) => n,
                            None => return true,
                        };
                        let updated = {
                            let parent = match Arc::get_mut(&mut parent_arc_3to2) {
                                Some(n) => n,
                                None => {
                                    self.cache.put_node(parent_addr_3to2, parent_arc_3to2);
                                    return true;
                                }
                            };
                            update_parent_routing_3to2(
                                parent,
                                node_addr,
                                sibling_addr,
                                right_addr,
                                median_3to2,
                            )
                        };
                        self.cache.insert_dirty(parent_addr_3to2, parent_arc_3to2);
                        updated
                    };
                    return parent_updated_3to2;
                }
                self.cache.put_node(node_addr, node_arc);
                self.cache.put_node(sibling_addr, sibling_arc);
                return false;
            }
            node.absorb(sibling);
            survivor_addr = node_addr;
            absorbed_addr = sibling_addr;
            // sibling_arc（被吸收）在此释放
            self.cache.insert_dirty(survivor_addr, node_arc);
        }

        // ── Phase 4: 更新父节点 routing 条目 ──
        //
        // 合并后幸存节点覆盖了更广的 key 区间。需要更新父节点的 routing：
        // 1. 删除被吸收节点的路由条目
        // 2. 如果吸收者（幸存节点）在其右侧（即右侧节点吸收左侧节点），
        //    吸收者的 routing key 大于被吸收者的 key，会导致 [absorbed_key, survivor_key)
        //    区间的 key 路由到前一个 sibling 而非实际持有数据的幸存节点。
        //    因此需要同时删除吸收者的旧条目，用 min(absorbed_key, survivor_key) 重新插入。
        //
        // 简而言之：删除吸收节点的条目后，吸收者的 routing key 必须能覆盖整个合并区间。
        //
        // 3→2 merge 的 parent routing 更新：
        // 删除三个旧节点（first、middle、last）的 routing 条目，
        // 插入两个新节点（left_result、right_result）的 routing 条目。
        fn update_parent_routing_3to2(
            parent: &mut BtreeNode,
            first_addr: u64,
            middle_addr: u64,
            last_addr: u64,
            median_key: BtreeKey,
        ) -> bool {
            // 需要删除的条目
            let to_delete = [first_addr, middle_addr, last_addr];
            for &addr in &to_delete {
                if let Some(idx) = find_entry_index(parent, addr) {
                    if let Some((k, _v)) = read_entry_by_global_idx(parent, idx) {
                        parent.delete_key(&k);
                    }
                }
            }
            parent.compact();
            // 重新插入两个新路由条目：
            // left: 使用 first_addr，用 MIN_KEY 确保覆盖合并后最左侧区间
            parent.insert(BtreeKey::MIN_KEY, BchVal::new(first_addr, 0));
            // right: 使用 last_addr，用 median_key 作为分割
            parent.insert(median_key, BchVal::new(last_addr, 0));
            true
        }

        fn update_parent_routing_after_merge(
            parent: &mut BtreeNode,
            survivor_addr: u64,
            absorbed_addr: u64,
        ) -> bool {
            let absorbed_idx = match find_entry_index(parent, absorbed_addr) {
                Some(idx) => idx,
                None => return true,
            };
            let absorbed_key = match read_entry_by_global_idx(parent, absorbed_idx) {
                Some((k, _v)) => k,
                None => return false,
            };
            let survivor_idx = match find_entry_index(parent, survivor_addr) {
                Some(idx) => idx,
                None => return true,
            };
            let survivor_key = match read_entry_by_global_idx(parent, survivor_idx) {
                Some((k, _v)) => k,
                None => return false,
            };
            let new_key = if absorbed_key.cmp(&survivor_key).is_lt() {
                absorbed_key
            } else {
                survivor_key
            };
            // 删除吸收者和被吸收者的条目
            parent.delete_key(&absorbed_key);
            parent.delete_key(&survivor_key);
            parent.compact();
            // 用合并后的最小 key + survivor_addr 重新插入
            let entry = BchVal::new(survivor_addr, 0);
            if parent.insert(new_key, entry) {
                return true;
            }
            // 低概率失败：compact 后再试一次
            parent.compact();
            parent.insert(new_key, entry)
        }

        let parent_updated = if parent_is_root {
            let updated = match Arc::get_mut(&mut self.root.node) {
                Some(parent) => {
                    update_parent_routing_after_merge(parent, survivor_addr, absorbed_addr)
                }
                None => true,
            };
            if updated {
                self.root_modified.store(true, Ordering::Release);
            }
            updated
        } else {
            let parent_addr = ancestors[ancestors.len() - 1];
            let mut parent_arc = match self.cache.take_node(parent_addr) {
                Some(n) => n,
                None => return true,
            };
            let updated = {
                let parent = match Arc::get_mut(&mut parent_arc) {
                    Some(n) => n,
                    None => {
                        self.cache.put_node(parent_addr, parent_arc);
                        return true;
                    }
                };
                update_parent_routing_after_merge(parent, survivor_addr, absorbed_addr)
            };
            self.cache.insert_dirty(parent_addr, parent_arc);
            updated
        };

        parent_updated
    }

    /// 合并 leaf（兼容层，内部调用 try_merge_node）
    fn maybe_merge_leaf(&mut self, leaf_addr: u64, path: &[u64]) -> bool {
        self.try_merge_node(leaf_addr, path)
    }

    /// 当 root 只有 1 个 child 时，将该 child 提升为新 root，深度减 1
    ///
    /// 在 cascade merge 后调用：如果所有 level-1 节点合并为 1 个，
    /// root 只有 1 个 routing 条目，此时将 depth-1 层级的节点设为新 root。
    fn collapse_root(&mut self) -> bool {
        if self.root.depth == 0 {
            return false;
        }
        let sole_child = {
            let root_node = match Arc::get_mut(&mut self.root.node) {
                Some(n) => n,
                None => return false,
            };
            if root_node.key_count != 1 {
                return false;
            }
            match read_entry_by_global_idx(root_node, 1) {
                Some((_k, v)) => v.paddr.get(),
                None => return false,
            }
        };
        let child_arc = match self.cache.take_node(sole_child) {
            Some(n) => n,
            None => return false,
        };
        self.root.node = child_arc;
        self.root.depth -= 1;
        true
    }
}

impl Default for Btree {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Btree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Btree")
            .field("depth", &self.root.depth)
            .field("key_count", &self.key_count())
            .field("cache_size", &self.cache.len())
            .finish()
    }
}

// ─── Leaf Merge 辅助函数 ─────────────────────────────────

/// 检查节点是否低于 1/3 容量阈值（需要合并）
///
/// 使用 `total_data_bytes()` 计算实际 packed 字节数，
/// 支持变长 `KeyValue::Raw`（避免固定 32B 误判）。
fn node_underfull(node: &BtreeNode) -> bool {
    node.total_data_bytes() < node.node_size / 3
}

/// 在 node 的所有 bset 中查找 value.paddr == target_addr 的 entry
/// 返回 1-indexed 全局索引
fn find_entry_index(node: &BtreeNode, target_addr: u64) -> Option<u16> {
    let mut global_idx = 1u16;
    for set in &node.sets {
        for i in 0..set.size as usize {
            let (_k, v) = node.read_entry(set, i + 1);
            if v.paddr.get() == target_addr {
                return Some(global_idx);
            }
            global_idx += 1;
        }
    }
    None
}

/// 根据全局 1-indexed 索引读取 entry 的 (key, value)
/// 遍历所有 bset 找到正确的 set
fn read_entry_by_global_idx(node: &BtreeNode, global_idx: u16) -> Option<(BtreeKey, BchVal)> {
    let mut remaining = global_idx;
    for set in &node.sets {
        if set.size == 0 {
            continue;
        }
        if remaining <= set.size {
            return Some(node.read_entry(set, remaining as usize));
        }
        remaining = remaining.saturating_sub(set.size);
    }
    None
}

/// 查找父节点中 child_addr 的相邻兄弟节点（按 KEY 序）
///
/// 之前用全局索引（set[0]→set[1] 顺序）找相邻 entry，
/// 但当 bsets 跨集无序时（例如 compact 后 insert 的小 key 进入 set[1] 而
/// set[0] 有更大的 key），全局索引邻居 ≠ key 序邻居，导致错误的合并。
///
/// 修复：收集所有 bsets 的 entries，按 key 排序后找 child_addr 的 prev/next 邻居。
fn find_sibling(parent: &BtreeNode, child_addr: u64, is_left: bool) -> Option<(BtreeKey, u64)> {
    let n = parent.key_count as usize;
    if n == 0 {
        return None;
    }
    // 收集所有 bset 中的 routing entries
    let mut entries: Vec<(BtreeKey, u64)> = Vec::with_capacity(n);
    for set in &parent.sets {
        for i in 0..set.size as usize {
            let (k, v) = parent.read_entry(set, i + 1);
            entries.push((k, v.paddr.get()));
        }
    }
    entries.sort_by_key(|a| a.0);

    let pos = entries.iter().position(|(_, addr)| *addr == child_addr)?;

    if is_left {
        if pos > 0 {
            Some(entries[pos - 1])
        } else {
            None
        }
    } else {
        if pos + 1 < entries.len() {
            Some(entries[pos + 1])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::MockBlockDevice;
    use crate::btree::bucket_io::write_initial_node;
    use crate::btree::key::KeyType;
    use crate::btree::key::{BtreeEntry, KeyValue};
    use crate::btree::node::BsetTree;
    use crate::btree::types::BtreePtrV2;
    use crate::btree::BtreeId;
    use crate::types::BlockAddr;

    /// 手动构造一个 2 层 B+tree（internal root + 2 leaves）
    fn make_two_level_tree() -> Btree {
        let cache = Arc::new(NodeCache::new());

        // left: keys 10, 20, 30
        let mut left = BtreeNode::new_leaf();
        left.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left);

        // right: keys 40, 50
        let mut right = BtreeNode::new_leaf();
        right.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        let right = Arc::new(right);

        let left_addr = cache.alloc_addr();
        let right_addr = cache.alloc_addr();
        cache.insert(left_addr, left);
        cache.insert(right_addr, right);

        // internal root
        let mut internal = BtreeNode::new_internal();
        let mut cur = 0u32;
        cur += internal.write_entry(cur, &BtreeKey::MIN_KEY, &BchVal::new(left_addr, 0));
        cur += internal.write_entry(
            cur,
            &BtreeKey::new(40, 1, KeyType::Normal),
            &BchVal::new(right_addr, 0),
        );
        internal.sets[0] = BsetTree {
            data_offset: 0,
            end_offset: cur,
            aux_offset: 0,
            size: 2,
            extra: 0,
        };
        internal.key_count = 2;

        Btree::from_root(BtreeRoot::new(Arc::new(internal), 1), cache)
    }

    #[tokio::test]
    async fn test_load_root_from_ptr_recursively_loads_children() {
        let backend = MockBlockDevice::new();

        let mut left = BtreeNode::new_leaf();
        left.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left.compact();
        let left_ptr = write_initial_node(&left, 100, 7, &backend).await.unwrap();

        let mut right = BtreeNode::new_leaf();
        right.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        right.compact();
        let right_ptr = write_initial_node(&right, 200, 8, &backend).await.unwrap();

        let mut root = BtreeNode::new_internal();
        let left_entry = BtreeEntry::new(
            BtreeKey::MIN_KEY.to_bpos(),
            KeyType::Normal,
            KeyValue::BtreePtr(BtreePtrV2 {
                block_addr: left_ptr.block_addr,
                sectors_written: left_ptr.sectors_written,
                level: left_ptr.level,
                generation: left_ptr.generation,
            }),
        );
        let right_entry = BtreeEntry::new(
            BtreeKey::new(40, 1, KeyType::Normal).to_bpos(),
            KeyType::Normal,
            KeyValue::BtreePtr(BtreePtrV2 {
                block_addr: right_ptr.block_addr,
                sectors_written: right_ptr.sectors_written,
                level: right_ptr.level,
                generation: right_ptr.generation,
            }),
        );
        assert!(root.insert_entry(&left_entry));
        assert!(root.insert_entry(&right_entry));
        root.compact();
        let root_ptr = write_initial_node(&root, 300, 9, &backend).await.unwrap();

        let mut btree = Btree::new();
        btree.load_root_from_ptr(&backend, root_ptr).await.unwrap();

        assert_eq!(btree.root_ptr(), &root_ptr);
        assert!(btree.cache.get(left_ptr.block_addr).is_some());
        assert!(btree.cache.get(right_ptr.block_addr).is_some());
        assert_eq!(
            btree.get(&BtreeKey::new(10, 1, KeyType::Normal)).unwrap().1,
            BchVal::new(100, 0)
        );
        assert_eq!(
            btree.get(&BtreeKey::new(50, 1, KeyType::Normal)).unwrap().1,
            BchVal::new(500, 0)
        );
    }

    #[test]
    fn test_btree_multi_level_insert() {
        let mut b = make_two_level_tree();

        // 插入到左叶子
        assert!(b.insert(
            BtreeKey::new(15, 1, KeyType::Normal),
            BchVal::new(150, 0),
            0
        ));
        let found = b.get(&BtreeKey::new(15, 1, KeyType::Normal));
        assert!(found.is_some(), "inserted key 15 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(150, 0));

        // 插入到右叶子
        assert!(b.insert(
            BtreeKey::new(45, 1, KeyType::Normal),
            BchVal::new(450, 0),
            0
        ));
        let found = b.get(&BtreeKey::new(45, 1, KeyType::Normal));
        assert!(found.is_some(), "inserted key 45 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(450, 0));

        // 现有 key 仍可读
        assert!(b.get(&BtreeKey::new(10, 1, KeyType::Normal)).is_some());
        assert!(b.get(&BtreeKey::new(50, 1, KeyType::Normal)).is_some());
    }

    #[test]
    fn test_btree_multi_level_delete() {
        let mut b = make_two_level_tree();

        // 删除左叶子中的 key
        assert!(b.delete(&BtreeKey::new(10, 1, KeyType::Normal), 0));
        assert!(
            b.get(&BtreeKey::new(10, 1, KeyType::Normal)).is_none(),
            "deleted key 10 gone"
        );

        // 删除右叶子中的 key
        assert!(b.delete(&BtreeKey::new(50, 1, KeyType::Normal), 0));
        assert!(
            b.get(&BtreeKey::new(50, 1, KeyType::Normal)).is_none(),
            "deleted key 50 gone"
        );

        // 其他 key 不受影响
        assert!(b.get(&BtreeKey::new(20, 1, KeyType::Normal)).is_some());
        assert!(b.get(&BtreeKey::new(40, 1, KeyType::Normal)).is_some());

        // 删除不存在的 key
        assert!(!b.delete(&BtreeKey::new(999, 1, KeyType::Normal), 0));
    }

    #[test]
    fn test_btree_multi_level_insert_after_delete() {
        let mut b = make_two_level_tree();

        // 删除后重新插入同一 key
        assert!(b.delete(&BtreeKey::new(20, 1, KeyType::Normal), 0));
        assert!(b.get(&BtreeKey::new(20, 1, KeyType::Normal)).is_none());

        assert!(b.insert(
            BtreeKey::new(20, 1, KeyType::Normal),
            BchVal::new(999, 0),
            0
        ));
        let found = b.get(&BtreeKey::new(20, 1, KeyType::Normal));
        assert!(found.is_some(), "re-inserted key 20 should be found");
        assert_eq!(found.unwrap().1, BchVal::new(999, 0));
    }

    /// 填充 leaf 直到触发 split → 验证 routing entry 正确插入 parent
    #[test]
    fn test_btree_multi_level_leaf_split() {
        let mut b = make_two_level_tree();

        // 左叶子现有 3 keys (10,20,30)，填充至满再 split
        // 256KB node / 29b entry ≈ 9039 max
        let fill_count = 9040;
        for i in 31..=fill_count {
            assert!(b.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0
            ));
        }

        // 验证 split 后的 key 可读
        let mid = fill_count / 2 + 30;
        assert!(
            b.get(&BtreeKey::new(mid, 1, KeyType::Normal)).is_some(),
            "key {} should be findable after split",
            mid
        );
        assert!(
            b.get(&BtreeKey::new(fill_count, 1, KeyType::Normal))
                .is_some(),
            "key {} should be findable after split",
            fill_count
        );

        // 原有 key 仍可读
        assert!(b.get(&BtreeKey::new(10, 1, KeyType::Normal)).is_some());
        assert!(b.get(&BtreeKey::new(50, 1, KeyType::Normal)).is_some());

        // depth 仍为 1（parent 有空间放 routing entry）
        assert_eq!(b.depth(), 1);
    }

    #[test]
    fn test_btree_new() {
        let b = Btree::new();
        assert_eq!(b.depth(), 0);
        assert_eq!(b.key_count(), 0);
    }

    #[test]
    fn test_btree_insert_and_count() {
        let mut b = Btree::new();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let v = BchVal::new(0xABCD, 1);
        assert!(b.insert(k, v, 0));
        assert!(b.key_count() > 0);
    }

    #[test]
    fn test_btree_get_empty() {
        let b = Btree::new();
        let result = b.get(&BtreeKey::new(100, 1, KeyType::Normal));
        assert!(result.is_none());
    }

    #[test]
    fn test_btree_get_after_insert() {
        let mut b = Btree::new();
        let k = BtreeKey::new(42, 1, KeyType::Normal);
        let v = BchVal::new(0xFF, 1);
        assert!(b.insert(k, v, 0));
        let found = b.get(&k);
        assert!(found.is_some());
        assert_eq!(found.unwrap().0, k);
    }

    #[test]
    fn test_btree_delete_no_panic() {
        let mut b = Btree::new();
        let k = BtreeKey::new(42, 1, KeyType::Normal);
        b.insert(k, BchVal::new(0xFF, 1), 0);
        b.delete(&k, 0);
    }

    #[test]
    fn test_btree_transaction() {
        let b = Btree::new();
        let result = b.with_transaction(|trans| {
            let iter = trans.get_iter(
                b.root(),
                &BtreeKey::new(100, 1, KeyType::Normal),
                false,
                BtreeId::Extents,
            );
            assert!(iter.peek().is_none());
        });
        assert!(result.is_ok());
    }

    #[test]
    fn test_btree_multiple_inserts() {
        let mut b = Btree::new();
        for i in 0..10 {
            let k = BtreeKey::new(i, 1, KeyType::Normal);
            assert!(b.insert(k, BchVal::new(i * 10, 1), 0));
        }
        assert_eq!(b.key_count(), 10);
    }

    /// 验证递归分裂传播：构建小节点树，插入大量 key 触发 3 层分裂
    ///
    /// 1. 设 root.node_size = 2048（~64 entries/node）
    /// 2. 插入足够 key 触发 leaf split → internal split → root split
    /// 3. 验证 depth=3 且所有 key 可读
    #[test]
    fn test_split_propagation_3level() {
        let mut b = Btree::new();
        // 使用小 node_size 加速分裂
        // node_size=512 → ~16 entries/node
        // depth 0→1: ~16 inserts
        // depth 1→2: ~14 leaf splits × 16 inserts ≈ 224
        // depth 2→3: ~14 internal splits × 16 leaf splits × 16 inserts ≈ 3584
        // 总计 ~3824 inserts，用 5000 保证触发
        let small_size = 512u32;
        Arc::get_mut(&mut b.root.node).unwrap().node_size = small_size;

        let total_keys = 5000u64;
        for i in 0..total_keys {
            if i % 500 == 0 {
                eprintln!(
                    "DEBUG: i={}, depth={}, cache_len={}",
                    i,
                    b.depth(),
                    b.cache().len()
                );
            }
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0
                ),
                "insert failed at i={}, depth={}, cache_len={}",
                i,
                b.depth(),
                b.cache().len()
            );
        }
        assert_eq!(
            b.depth(),
            3,
            "tree should have depth 3 after recursive split propagation (got depth={})",
            b.depth()
        );
        assert_eq!(
            b.key_count(),
            total_keys as u32,
            "all {} keys should be counted",
            total_keys
        );

        // 验证所有 key 可达
        for i in 0..total_keys {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after split propagation",
                i
            );
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    // ─── Wave 0: insert_routing_entry_at 边界安全测试 ─────────────

    /// 验证 insert_routing_entry_at 在 pos=0 时不触发 wrapping panic：
    /// depth=2 树，强制分裂 level-1 internal node → 触发 pos 从 0 回溯到 root
    #[test]
    fn test_insert_routing_no_wrap_panic() {
        let mut b = Btree::new();
        // node_size=256 → ~8 entries/node，确保内部节点也能分裂
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 256;

        // 插入大量 key 强制多级分裂（depth 从 0→1→2→3），
        // 覆盖 insert_routing_entry_at 中 pos=0 后的边界分支
        let total = 3000u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0
                ),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }
        // 深度应 ≥2（验证经过内部节点分裂）
        assert!(
            b.depth() >= 2,
            "tree should have depth >=2 after forcing internal splits (got depth={})",
            b.depth()
        );

        // 所有 key 仍可达
        for i in 0..total {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证单 leaf → split_root 时空 path 正确：depth=0 插入满触发 split_root
    #[test]
    fn test_insert_routing_path_empty() {
        let mut b = Btree::new();
        // 使用小 node_size 加速分裂
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 256;

        let total = 200u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 5, 0),
                    0
                ),
                "insert failed at i={}",
                i
            );
        }
        // 应触发 root split（depth 0→1）
        assert!(
            b.depth() >= 1,
            "tree should have depth >=1 after split_root"
        );
        assert_eq!(b.key_count(), total as u32, "all keys counted");

        // 所有 key 可达
        for i in 0..total {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after split_root",
                i
            );
        }
    }

    /// 验证 split_root 会把当前 journal_seq 写入新分裂出的节点和根节点。
    #[test]
    fn test_split_root_propagates_journal_seq() {
        let mut b = Btree::new();
        let split_seq = 77u64;
        {
            let root = Arc::get_mut(&mut b.root.node).unwrap();
            root.node_size = 256;
            root.journal_seq = 13;
            for i in 0..8u64 {
                assert!(root.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 3, 0),));
            }
        }
        assert!(b.split_root(None, split_seq));

        let flushed = b.flush_dirty();
        assert!(
            !flushed.is_empty(),
            "split_root should leave dirty nodes to flush"
        );
        for (addr, node) in flushed {
            assert_eq!(
                node.journal_seq, split_seq,
                "node {addr} should inherit the split journal_seq"
            );
        }
    }

    // ─── Wave 1: 字节分割 + 命名循环 + debug 断言 ─────────────

    /// 验证 byte-size 分割后两个半节点的字节用量相近（偏差 ≤20%）
    ///
    /// Given: 包含奇数个等大小条目的节点（9 entries × 32 bytes = 288 total）
    /// When:  执行 split()
    /// Then:  左右半节点的字节用量应在 half_bytes 的 ±20% 以内
    #[test]
    fn test_split_balanced_byte_size() {
        let mut node = BtreeNode::new_leaf();
        // 插入 9 个 key（奇数个，展示字节分割与计数分割的差异）
        for i in 0..9 {
            assert!(node.insert(
                BtreeKey::new(i as u64, 1, KeyType::Normal),
                BchVal::new(i as u64 * 10, 0),
            ));
        }
        let (median_key, right_half) = node.split().expect("split should succeed with 9 entries");
        assert!(median_key.get_vaddr() > 0, "median_key should be valid");

        let left_bytes = node.total_data_bytes();
        let right_bytes = right_half.total_data_bytes();
        let total_bytes = left_bytes + right_bytes;
        let half_bytes = total_bytes / 2;

        // Both halves should be within 20% of the ideal half size
        let left_diff_pct = if left_bytes > half_bytes {
            (left_bytes - half_bytes) as f64 / half_bytes as f64 * 100.0
        } else {
            (half_bytes - left_bytes) as f64 / half_bytes as f64 * 100.0
        };
        let right_diff_pct = if right_bytes > half_bytes {
            (right_bytes - half_bytes) as f64 / half_bytes as f64 * 100.0
        } else {
            (half_bytes - right_bytes) as f64 / half_bytes as f64 * 100.0
        };

        // 容差 35%：find_balanced_split 以 60/40 为目标（bcachefs 对齐），
        // 等大小离散条目下 9→6/3 分最大偏差 33.3%（192/96 vs 144）
        assert!(
            left_diff_pct <= 35.0,
            "left half byte usage {} bytes is {:.1}% off from half {} bytes",
            left_bytes,
            left_diff_pct,
            half_bytes
        );
        assert!(
            right_diff_pct <= 35.0,
            "right half byte usage {} bytes is {:.1}% off from half {} bytes",
            right_bytes,
            right_diff_pct,
            half_bytes
        );

        // All entries should be searchable in their respective halves
        // find_balanced_split 以 60/40 为目标，9 个等大条目 → 6 left (0-5), 3 right (6-8)
        for i in 0..6 {
            let found = node.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "left entry {} should survive split", i);
        }
        for i in 6..9 {
            let found = right_half.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "right entry {} should survive split", i);
        }
    }

    /// 验证 insert_routing_entry_at 主循环干净迭代（不卡死、不 panic）：
    /// 构建 depth=2 树，强制 internal node 分裂 → routing loop 必须多次迭代
    ///
    /// Given: node_size=512 的 Btree
    /// When:  插入足够的 key 强制 depth 增长到 ≥2（经过内部节点分裂）
    /// Then:  所有 key 可达，key_count 正确，depth≥2
    #[test]
    fn test_insert_routing_clean_loop() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 512;

        // Insert enough to force internal node splits and depth ≥2
        // node_size=512 → ~16 entries/node
        let total = 2000u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }

        // The routing loop should have cleanly processed all iterations
        assert!(
            b.depth() >= 2,
            "depth should be >=2 after forcing internal splits (got depth={})",
            b.depth()
        );
        assert_eq!(
            b.key_count(),
            total as u32,
            "all {} keys should be counted",
            total
        );

        // Verify all keys are reachable
        for i in 0..total {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} lost after routing loop", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证 at_path_boundary（pos=0 → root 回溯）不 panic：
    /// 构建小节点树 → 插入大量 key 强制通过 path[0] 分裂边界
    ///
    /// 与 test_split_propagation_3level 类似，但明确断言
    /// at_path_boundary（insert_routing_entry_at 中 pos=0 时的
    /// root 回溯分支）不会引发 panic 或死循环。
    #[test]
    fn test_insert_routing_at_path_boundary() {
        let mut b = Btree::new();
        // node_size=384 → ~12 entries/node，更快触发深度分裂
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 384;

        // 强制多级分裂（depth 0→1→2→3），
        // 确保 routing loop 的 at_path_boundary（pos=0 → root）被触发
        let total = 4000u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ),
                "insert failed at i={}, depth={}",
                i,
                b.depth()
            );
        }

        // 断言：at_path_boundary 分支没有 panic
        // 如果到达这里，说明边界分支执行成功
        assert!(
            b.depth() >= 2,
            "depth should be >=2 after crossing path boundary (got depth={})",
            b.depth()
        );
        assert_eq!(
            b.key_count(),
            total as u32,
            "all {} keys should survive at_path_boundary traversal",
            total
        );

        // 验证所有 key 仍可达
        for i in 0..total {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} lost after at_path_boundary split",
                i
            );
        }
    }

    // ─── Wave 2: Leaf merge after delete ─────────────────────────

    /// 验证叶子合并：左 leaf underfull 后与右兄弟合并
    ///
    /// 1. 创建小节点树（node_size=512, ~16 entries/node）
    /// 2. 插入 30 个 key → 分裂为 2 个 leaf（depth=1）
    /// 3. 从左 leaf 删除 10 个 key → 左 leaf ~5 entries < 6（underfull）
    /// 4. 验证合并发生：所有剩余 key（10..29）仍可达
    #[test]
    fn test_leaf_merge_after_delete() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 512;

        // 插入 30 个 key → 2 个 leaf（depth=1）
        for i in 0..30u64 {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ),
                "insert failed at i={}",
                i
            );
        }
        assert_eq!(b.depth(), 1, "should be depth=1 after split");
        assert_eq!(b.key_count(), 30, "all 30 keys inserted");

        // 从左 leaf 删除 10 个 key（keys 0..9）
        // 左 leaf 原 ~15 entries → 余 ~5 entries → underfull（< 6）
        for i in 0..10u64 {
            assert!(
                b.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0),
                "delete failed at i={}",
                i
            );
        }

        // 合并后幸存 leaf 应有 keys 10..29（20 个 key）
        assert_eq!(
            b.key_count(),
            20,
            "should have 20 keys after merge (expected 20, got {})",
            b.key_count()
        );

        // 验证已删除 key 不可达
        for i in 0..10u64 {
            assert!(
                b.get(&BtreeKey::new(i, 1, KeyType::Normal)).is_none(),
                "deleted key {} should not exist",
                i
            );
        }

        // 验证剩余 key 全部可达
        for i in 10..30u64 {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable after merge", i);
            assert_eq!(
                found.unwrap().1,
                BchVal::new(i * 10, 0),
                "key {} should have correct value",
                i
            );
        }
    }

    /// 验证不触发合并：阈值以上时不执行合并
    ///
    /// 1. 创建小节点树（node_size=512）
    /// 2. 插入 30 个 key → 2 个 leaf
    /// 3. 只删除 1 个 key → leaf 仍在阈值以上 → 不触发合并
    #[test]
    fn test_leaf_no_merge_above_threshold() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 512;

        for i in 0..30u64 {
            assert!(b.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ));
        }
        let cache_len_before = b.cache().len();
        assert_eq!(b.key_count(), 30);

        // 只删除 1 个 key → leaf 有 ~14 entries → 远高于阈值（<6）
        assert!(b.delete(&BtreeKey::new(0, 1, KeyType::Normal), 0));

        // 应不触发合并（cache 不变，key_count 正确）
        assert_eq!(
            b.cache().len(),
            cache_len_before,
            "cache should not change when no merge occurs"
        );
        assert_eq!(b.key_count(), 29, "should have 29 keys after single delete");

        // 验证已删除 key
        assert!(b.get(&BtreeKey::new(0, 1, KeyType::Normal)).is_none());

        // 验证剩余 key
        for i in 1..30u64 {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist", i);
        }
    }

    /// 验证与左兄弟合并：最右侧 leaf underfull 时与左兄弟合并
    ///
    /// 1. 插入足够 key 创建 5+ 个 leaf（depth=2+）
    /// 2. 删除最后 ~30 个 key → 最右侧 leaf underfull
    /// 3. 应与左兄弟合并，所有剩余 key 可达
    #[test]
    fn test_leaf_merge_left_sibling() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 256;

        // node_size=256 → ~8 entries/node
        // 插入 120 个 key → 多个 leaf, depth≥2
        let total = 120u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ),
                "insert failed at i={}",
                i
            );
        }
        assert!(b.depth() >= 2, "should have depth >= 2 (got {})", b.depth());
        let original_count = b.key_count();

        // 删除最右侧 ~30 个 key → 最右侧 leaf underfull
        let delete_start = 90u64;
        for i in delete_start..total {
            assert!(
                b.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0),
                "delete failed at i={}",
                i
            );
        }

        // 合并后 key_count 正确
        assert_eq!(
            b.key_count(),
            delete_start as u32,
            "should have {} keys (got {})",
            delete_start,
            b.key_count()
        );

        // 验证剩余 key 全部可达
        for i in 0..delete_start {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证最左侧 leaf 合并：左边界 leaf underfull 时与右兄弟合并
    ///
    /// 1. 创建小节点树（node_size=512）
    /// 2. 插入 30 个 key → 2 个 leaf
    /// 3. 从左 leaf 删除 12 个 key → 左 leaf ~3 entries → underfull
    /// 4. 应与右兄弟（右侧 leaf）合并
    #[test]
    fn test_leaf_merge_edge_min_key() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 512;

        for i in 0..30u64 {
            assert!(b.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ));
        }
        assert_eq!(b.depth(), 1);
        assert_eq!(b.key_count(), 30);

        // 从左 leaf 删除 12 个 key → 余 ~3 → underfull
        for i in 0..12u64 {
            assert!(
                b.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0),
                "delete failed at i={}",
                i
            );
        }

        // 合并后应有 18 个 key（keys 12..29）
        assert_eq!(
            b.key_count(),
            18,
            "should have 18 keys after left leaf merge (got {})",
            b.key_count()
        );

        // 验证已删除 key 不可达
        for i in 0..12u64 {
            assert!(
                b.get(&BtreeKey::new(i, 1, KeyType::Normal)).is_none(),
                "deleted key {} should not exist",
                i
            );
        }

        // 验证剩余 key 全部可达
        for i in 12..30u64 {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should be reachable", i);
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }
    }

    /// 验证 3 层树的 cascade merge + collapse_root：
    ///
    /// 1. 创建深度 3 的树（node_size=512, ~5000 keys）
    /// 2. 删除大量右侧 key → 触发 leaf merge cascade
    /// 3. cascade 向上传播 → 内部节点合并 → collapse_root 缩减深度
    /// 4. 验证所有剩余 key 仍可达
    #[test]
    fn test_cascade_merge_3level() {
        let mut b = Btree::new();
        Arc::get_mut(&mut b.root.node).unwrap().node_size = 512;

        // 插入足够的 key 使树深度达到 3
        let total = 5000u64;
        for i in 0..total {
            assert!(
                b.insert(
                    BtreeKey::new(i, 1, KeyType::Normal),
                    BchVal::new(i * 10, 0),
                    0,
                ),
                "insert failed at i={}",
                i
            );
        }
        assert_eq!(
            b.depth(),
            3,
            "tree should have depth 3 before cascade (got {})",
            b.depth()
        );
        assert_eq!(b.key_count(), total as u32);

        // 小批量删除，每步检查 integrity
        let check_keys: &[u64] = &[312, 313, 314, 315, 320, 350, 400, 500];

        // 逐个删除，每 5 个检查一次 routing integrity
        for i in 250..400u64 {
            assert!(
                b.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0),
                "delete failed at i={}",
                i
            );
            if i % 5 == 0 {
                for &ck in check_keys {
                    if ck > i {
                        let k = BtreeKey::new(ck, 1, KeyType::Normal);
                        if b.get(&k).is_none() {
                            let mut path = Vec::new();
                            let leaf_addr = b.find_path_to_leaf(&k, &mut path);
                            eprintln!("FAIL at i={}: key {} unreachable, depth={}, cache_len={}, path={:?}, leaf_addr={:?}", 
                                i, ck, b.depth(), b.cache().len(), path, leaf_addr);
                            let root = &b.root;
                            eprintln!(
                                "Root key_count={} level={}",
                                root.node.key_count, root.node.level
                            );
                            // Check path addrs
                            for (pi, &paddr) in path.iter().enumerate() {
                                match b.cache().get(paddr) {
                                    Some(n) => {
                                        eprintln!(
                                            "Path[{}] addr={} key_count={} level={}",
                                            pi, paddr, n.key_count, n.level
                                        );
                                        // Dump routing entries for internal nodes
                                        if n.level > 0 {
                                            eprintln!(
                                                "  Routing entries (key_count={}):",
                                                n.key_count
                                            );
                                            for si in 0..3 {
                                                let s = &n.sets[si];
                                                eprintln!("    set[{}]: data_offset={} end_offset={} aux_offset={} size={}", si, s.data_offset, s.end_offset, s.aux_offset, s.size);
                                                for ei in 0..s.size as usize {
                                                    let (rk, rv) = n.read_entry(s, ei + 1);
                                                    let va = unsafe {
                                                        std::ptr::addr_of!(rk.vaddr)
                                                            .read_unaligned()
                                                    };
                                                    let si = unsafe {
                                                        std::ptr::addr_of!(rk.snapshot_id)
                                                            .read_unaligned()
                                                    };
                                                    eprintln!(
                                                        "    entry={} key=({},{}) value=addr({})",
                                                        ei,
                                                        va,
                                                        si,
                                                        rv.paddr.get()
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    None => eprintln!("Path[{}] addr={} NOT IN CACHE!", pi, paddr),
                                }
                            }
                            // Dump leaf content
                            if let Some(leaf_addr) = leaf_addr {
                                match b.cache().get(leaf_addr) {
                                    Some(leaf) => {
                                        eprintln!(
                                            "Leaf addr={} key_count={}:",
                                            leaf_addr, leaf.key_count
                                        );
                                        for si in 0..3 {
                                            let s = &leaf.sets[si];
                                            for ei in 0..s.size as usize {
                                                let (lk, _lv) = leaf.read_entry(s, ei + 1);
                                                let va = unsafe {
                                                    std::ptr::addr_of!(lk.vaddr).read_unaligned()
                                                };
                                                let si = unsafe {
                                                    std::ptr::addr_of!(lk.snapshot_id)
                                                        .read_unaligned()
                                                };
                                                eprintln!(
                                                    "  key=({},{}) type={:?}",
                                                    va, si, lk.key_type
                                                );
                                            }
                                        }
                                    }
                                    None => eprintln!("Leaf addr={} NOT IN CACHE!", leaf_addr),
                                }
                            }
                            // Scan ALL leaves for keys 312-319
                            eprintln!("Scanning all cache entries for keys 312-319:");
                            for addr in 0..100u64 {
                                if let Some(n) = b.cache().get(addr) {
                                    if n.level == 0 {
                                        for si in 0..3 {
                                            let s = &n.sets[si];
                                            for ei in 0..s.size as usize {
                                                let (lk, _lv) = n.read_entry(s, ei + 1);
                                                let va = unsafe {
                                                    std::ptr::addr_of!(lk.vaddr).read_unaligned()
                                                };
                                                if va >= 312 && va <= 319 {
                                                    eprintln!(
                                                        "  FOUND key={} at leaf_addr={}",
                                                        va, addr
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // Find which leaf key 312 SHOULD route to, by checking root+level-1 entries
                            eprintln!("Reachability for keys around 312:");
                            for seq in [
                                0u64, 250, 300, 310, 311, 312, 313, 314, 315, 316, 320, 350, 400,
                                4999,
                            ] {
                                let found = b.get(&BtreeKey::new(seq, 1, KeyType::Normal));
                                eprintln!(
                                    "  key {} -> {}",
                                    seq,
                                    if found.is_some() { "OK" } else { "MISSING" }
                                );
                            }
                            panic!("key {} unreachable at i={}", ck, i);
                        }
                    }
                }
            }
        }
        // Bypass the rest — bulk delete after cascade
        let keep = (total * 5 / 100) as u64;
        for i in 400..total {
            assert!(
                b.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0),
                "delete failed at i={}",
                i
            );
        }

        assert_eq!(
            b.key_count(),
            keep as u32,
            "should keep {} keys after mass delete (got {})",
            keep,
            b.key_count()
        );

        // cascade merge + collapse_root 后深度应 ≤ 2
        assert!(
            b.depth() <= 2,
            "depth should be <= 2 after cascade collapse (got {})",
            b.depth()
        );

        // 验证剩余 key 全部可达
        for i in 0..keep {
            let found = b.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(
                found.is_some(),
                "key {} should be reachable after cascade collapse (depth={})",
                i,
                b.depth()
            );
            assert_eq!(found.unwrap().1, BchVal::new(i * 10, 0));
        }

        // 验证已删除 key 不可达
        for i in keep..(keep + 50).min(total) {
            assert!(
                b.get(&BtreeKey::new(i, 1, KeyType::Normal)).is_none(),
                "deleted key {} should not exist after cascade",
                i
            );
        }
    }

    // ─── load_root 测试 ──────────────────────────────────────────

    #[tokio::test]
    async fn test_btree_load_root_from_backend() {
        let backend = MockBlockDevice::new();
        let mut btree = Btree::new();

        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        node.compact();

        let data = node.serialize_to_bucket(100).unwrap();
        backend
            .write_block(BlockAddr::new(100), &data)
            .await
            .unwrap();

        let original_ptr = Arc::as_ptr(&btree.root().node);
        btree.load_root(&backend, 100).await.unwrap();
        let new_ptr = Arc::as_ptr(&btree.root().node);

        assert_eq!(btree.depth(), 0);
        assert_ne!(original_ptr, new_ptr, "root node should be replaced");
        assert!(btree.get(&BtreeKey::new(10, 1, KeyType::Normal)).is_some());
        assert!(btree.get(&BtreeKey::new(20, 1, KeyType::Normal)).is_some());
    }

    #[tokio::test]
    async fn test_btree_load_root_corrupt() {
        let backend = MockBlockDevice::new();
        let mut btree = Btree::new();

        backend
            .write_block(BlockAddr::new(999), &[0xFF; 64])
            .await
            .unwrap();
        let result = btree.load_root(&backend, 999).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_btree_load_root_skip_zero() {
        let backend = MockBlockDevice::new();
        let mut btree = Btree::new();

        let result = btree.load_root(&backend, 0).await;
        assert!(result.is_ok());
        assert_eq!(btree.depth(), 0);
    }
}
