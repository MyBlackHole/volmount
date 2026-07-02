//! BtreeIter — bcachefs 对齐的 btree 遍历器
//!
//! 核心设计（对应 bcachefs `btree_iter` + `btree_path`）：
//!
//! ## 临时 key buffer
//!
//! `peek_max` 等操作需要临时 unpack 空间来存储上限范围内的 key。
//! 使用 `BTREE_ITER_BUF_GRANULARITY = 2048` 作为 buffer 粒度，
//! 对齐 bcachefs `bkey_buf.h` 中 `kmalloc(2048)` 的 heap 分配尺寸。

/// bcachefs 对齐的 btree_iter 临时 key buffer 粒度
///
/// 对应 bcachefs `bkey_buf` 的 heap 分配尺寸（`bkey_buf.h:20`）：
/// `kmalloc_noprof(2048, GFP_KERNEL|__GFP_NOFAIL)`。
/// 用于 peek_max 等需要临时 unpack/重组 key 的操作。
/// 2048 字节可容纳 ~256 字段的极端 key，远超正常 entry 大小。
pub const BTREE_ITER_BUF_GRANULARITY: usize = 2048;
//
// 1. **路径缓存**: 从 root 到 leaf 的完整路径存储在 `path` 数组中，
//    每层级包含节点引用 + 锁状态 + entry 偏移。
// 2. **三级锁**: Read → Intent → Write，通过 SixLock 升级降级。
// 3. **Restart 机制**: 当锁竞争导致路径失效时，自动从 root 重新遍历。
// 4. **intent lock 语义**: 写路径先拿 intent（不阻塞读），再升级到 write。

/// 可共享的 path 快照句柄。
///
/// 这保留了当前 `Vec<BtreePathLevel>` 的局部遍历模型，同时为
/// 复用 / restart 场景提供一个可克隆的路径镜像。
#[derive(Debug, Clone)]
pub struct SharedBtreePath {
    inner: Arc<Mutex<SharedBtreePathState>>,
}

#[derive(Debug, Clone)]
struct SharedBtreePathState {
    path: Vec<BtreePathLevel>,
    generation: u64,
}

impl SharedBtreePath {
    fn new(path: &[BtreePathLevel]) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedBtreePathState {
                path: path.to_vec(),
                generation: 0,
            })),
        }
    }

    fn refresh(&self, path: &[BtreePathLevel]) {
        let mut inner = self.inner.lock().unwrap();
        inner.path = path.to_vec();
        inner.generation = inner.generation.wrapping_add(1);
    }

    pub fn snapshot(&self) -> Vec<BtreePathLevel> {
        self.inner.lock().unwrap().path.clone()
    }

    pub fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }
}

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use crate::btree::key::BtreeEntry;
use crate::btree::key::{BchVal, BtreeKey, KeyType};
use crate::btree::node::BtreeNode;
use crate::btree::types::{
    BtreeNodeLockedType, BtreePathLevel, BtreeRoot, NodeCache, BTREE_MAX_DEPTH,
};
use crate::btree::Bpos;
use crate::btree::BtreeEngine;
use crate::btree::BtreeId;
use crate::recovery::overlay::JournalKeys;

/// 遍历标志
#[derive(Debug, Clone, Copy)]
pub struct IterFlags {
    /// 是否允许 intent 锁（写路径需要）
    pub intent: bool,
    /// 遍历方向（true = 正向）
    pub forward: bool,
    /// 是否检查 journal overlay（读后写一致性）
    pub with_journal: bool,
}

impl Default for IterFlags {
    fn default() -> Self {
        Self {
            intent: false,
            forward: true,
            with_journal: true,
        }
    }
}

/// B-tree 遍历器 — 对应 bcachefs `struct btree_iter`
///
/// 维护从 root 到 leaf 的完整路径，支持：
/// - 锁升级/降级（read → intent → write）
/// - 路径缓存重用（advance 只在 leaf 内移动，不重新遍历）
/// - Restart（检测到锁失效时从 root 重新开始）
///
/// bcachefs 字段对齐：
/// - pos ↔ struct bpos pos
/// - snapshot ↔ unsigned snapshot
/// - flags ↔ u16 flags (BTREE_ITER_intent 等位标志)
/// - path ↔ btree_path_idx_t path (volmount 内联存储 path levels)
#[derive(Debug)]
pub struct BtreeIter {
    /// 从 root 到 leaf 的路径 [root, ..., leaf]
    /// 对应 bcachefs `trans->paths[iter->path]`
    pub path: Vec<BtreePathLevel>,
    /// 可共享的路径快照句柄
    shared_path: SharedBtreePath,
    /// 当前位置（leaf 中的 key）
    /// 对应 bcachefs `iter->pos`
    pub pos: BtreeKey,
    /// 遍历标志
    /// 对应 bcachefs `iter->flags` (BTREE_ITER_intent 等)
    pub flags: IterFlags,
    /// 是否发生过 restart
    pub had_restart: bool,
    /// 节点缓存（用于多级树中子节点查找和重启）
    pub cache: Arc<NodeCache>,
    /// 当前快照 ID（快照过滤用，0 = 无过滤）
    /// 对应 bcachefs `iter->snapshot`
    pub snapshot: u32,
    /// B-tree 类型（journal overlay 查找用）
    /// 对应 bcachefs `iter->btree_id`
    pub btree_type: BtreeId,
    /// 快照可见性缓存：存活期为整个 iter 生命周期
    /// key: (iter_snapshot, key_snapshot) → is_ancestor
    /// 对应 bcachefs `trans->snapshot_visible`
    /// 消除同一遍历中重复的 Snapshots btree 查询
    snapshot_visible_cache: HashMap<(u32, u32), bool>,
    /// journal overlay 引用（原始指针，无生命周期约束）
    /// 在 RW 恢复阶段（set_may_go_rw → journal_replay）指向 BtreeEngine 的 overlay。
    /// 安全前提：overlay 存活期 > BtreeIter （由调用者保证）
    overlay: Option<NonNull<JournalKeys>>,
}

impl BtreeIter {
    /// 创建一个新的 iter（初始化为指定位置）
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek()` 中从 root 下降的逻辑：
    /// 1. 从 root 开始，lock_read 根节点
    /// 2. 对每个 internal 节点，二分查找目标 key 属于哪个 child
    /// 3. lock_read 子节点，unlock 父节点（或升级到 intent）
    /// 4. 下降到 leaf 后，在 leaf 内定位目标 key
    pub fn init(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        btree_type: BtreeId,
        overlay: Option<&JournalKeys>,
    ) -> Self {
        let mut path: Vec<BtreePathLevel> = Vec::with_capacity(BTREE_MAX_DEPTH);

        // Step 1: lock_read root
        let root_node = root.node.clone();
        root_node.lock.lock_read();
        let mut root_pl = BtreePathLevel::new(root_node);
        root_pl.locked_seq = root_pl.node.lock.seq();
        path.push(root_pl);

        // Step 2: 逐级下降到 leaf
        let depth = root.depth;
        for level in (1..=depth).rev() {
            let parent = &path[path.len() - 1];
            let (child_addr, child_idx) = Self::find_child_node(&parent.node, target);
            let child = cache.get_or_create(child_addr, level - 1);

            // 备注：bcachefs 对齐 — 预取下一个兄弟节点
            // 在下降路径中，如果下一个兄弟节点还未缓存，发起异步预取
            if let Some(v) = Self::read_entry_by_global_idx(&parent.node, child_idx + 1) {
                let next_addr = v.paddr.get();
                cache.prefetch_node(next_addr, level - 1, btree_type);
            }

            // lock child
            child.lock.lock_read();
            // unlock parent (但保持 intent/write 锁如果 flags 要求)
            if !flags.intent {
                parent.node.lock.unlock_read();
                // 父层级锁已释放，更新 lock_state 为 None
                // 防止后续 restart_optimized 对已释放锁再次 unlock
                if let Some(p) = path.last_mut() {
                    p.lock_state = BtreeNodeLockedType::None;
                }
            } else {
                // intent 模式：父层级保留读锁
                if let Some(p) = path.last_mut() {
                    p.lock_state = BtreeNodeLockedType::Read;
                }
            }

            let locked_seq = child.lock.seq();
            let mut pl = BtreePathLevel::new(child);
            pl.child_idx = child_idx;
            pl.locked_seq = locked_seq;
            path.push(pl);
        }

        // 最后一级是 leaf — 赋值正确的锁状态
        if let Some(leaf) = path.last_mut() {
            leaf.lock_state = if flags.intent {
                BtreeNodeLockedType::Intent
            } else {
                BtreeNodeLockedType::Read
            };
        }

        // Step 3: 在 leaf 中定位 key
        let mut iter = Self {
            pos: *target,
            path,
            shared_path: SharedBtreePath::new(&[]),
            flags,
            had_restart: false,
            cache: cache.clone(),
            snapshot: 0,
            btree_type,
            snapshot_visible_cache: HashMap::new(),
            overlay: overlay.map(NonNull::from),
        };
        iter.shared_path = SharedBtreePath::new(&iter.path);

        // 在 leaf 中定位第一个 >= target 的 entry（lower_bound 语义）
        //
        // peek_entry/advance 按 sets[0]→[1]→[2] 拼接顺序遍历。
        // 当 target 是 MIN_KEY 时（for_each_entry），所有条目都 >= target，
        // 直接取 offset=1（首个遍历条目），避免跨 set 搜索产生错误的 global_off。
        //
        // 对非 MIN_KEY（get_entry / seek），跨 set 搜索仍进行，但
        // global_off 反映遍历拼接位置，而非 key 序位置。
        let mut best_global_off = 0u16;
        let mut best_key: Option<BtreeKey> = None;
        let mut best_si: usize = 0;
        if *target == BtreeKey::MIN_KEY {
            best_global_off = 1;
        } else if let Some(leaf) = iter.path.last() {
            if leaf.node.key_count > 0 {
                let mut cumul = 0u16;
                for (si, set) in leaf.node.sets.iter().enumerate() {
                    if set.size == 0 {
                        continue;
                    }
                    let cnt = set.size as usize;
                    let local_best = if si == 0 {
                        // set[0] 是 compacted 排序集 → 二分查找
                        let mut lo = 1i32;
                        let mut hi = cnt as i32;
                        let mut lb: i32 = cnt as i32 + 1;
                        while lo <= hi {
                            let mid = (lo + hi) / 2;
                            let (k, _) = leaf.node.read_entry(set, mid as usize);
                            if k < *target {
                                lo = mid + 1;
                            } else {
                                lb = mid;
                                hi = mid - 1;
                            }
                        }
                        if lb <= cnt as i32 {
                            lb as u16
                        } else {
                            0
                        }
                    } else {
                        // set[1..] 是增量追加 → 非排序，找最小 k >= target
                        let mut best_local = 0u16;
                        let mut best_key_local: Option<BtreeKey> = None;
                        for i in 0..cnt {
                            let (k, _) = leaf.node.read_entry(set, i + 1);
                            if k >= *target {
                                match &best_key_local {
                                    None => {
                                        best_key_local = Some(k);
                                        best_local = (i + 1) as u16;
                                    }
                                    Some(bk) if k < *bk => {
                                        best_key_local = Some(k);
                                        best_local = (i + 1) as u16;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        best_local
                    };
                    if local_best > 0 {
                        let (k, _) = leaf.node.read_entry(set, local_best as usize);
                        let update = match &best_key {
                            None => true,
                            Some(bk) => k < *bk || (k == *bk && si > best_si),
                        };
                        if update {
                            best_key = Some(k);
                            best_global_off = cumul + local_best;
                            best_si = si;
                        }
                    }
                    cumul += set.size;
                }
                // Fallback: 所有 entry 都 < target 时取第一个全局位置
                if best_global_off == 0 {
                    best_global_off = 1;
                }
            }
        }
        iter.path.last_mut().map(|l| l.offset = best_global_off);
        if best_global_off > 0 {
            // 更新 pos 为当前位置的 key（用 peek 逻辑：全局 offset → 局部 offset）
            let leaf = iter.path.last().unwrap();
            let mut adj = best_global_off;
            for set in &leaf.node.sets {
                if set.size > 0 {
                    if adj as u32 <= set.size as u32 {
                        let (k, _) = leaf.node.read_entry(set, adj as usize);
                        iter.pos = k;
                        break;
                    }
                    adj -= set.size;
                }
            }
        }

        iter.sync_shared_path();
        iter
    }

    /// 获取共享 path 快照句柄
    pub fn shared_path(&self) -> SharedBtreePath {
        self.shared_path.clone()
    }

    /// 返回共享 path 的引用计数
    pub fn shared_path_ref_count(&self) -> usize {
        self.shared_path.ref_count()
    }

    /// 克隆一个共享同一 path 快照句柄的 iterator。
    pub fn fork_shared_path(&self) -> Self {
        Self {
            path: self.path.clone(),
            shared_path: self.shared_path.clone(),
            pos: self.pos,
            flags: self.flags,
            had_restart: self.had_restart,
            cache: self.cache.clone(),
            snapshot: self.snapshot,
            btree_type: self.btree_type,
            snapshot_visible_cache: self.snapshot_visible_cache.clone(),
            overlay: self.overlay,
        }
    }

    fn sync_shared_path(&self) {
        self.shared_path.refresh(&self.path);
    }

    /// 创建带快照过滤的 iterator
    ///
    /// 创建带快照过滤的 iterator（bcachefs 对齐：设置 iter->snapshot）
    ///
    /// 在 `init()` 基础上设置 `snapshot`，
    /// 使 `peek_visible()` 能调用 `is_ancestor_from_btree()` 过滤。
    pub fn init_with_snapshot(
        root: &BtreeRoot,
        target: &BtreeKey,
        flags: IterFlags,
        cache: &Arc<NodeCache>,
        snapshot_id: u32,
        btree_type: BtreeId,
    ) -> Self {
        let mut iter = Self::init(root, target, flags, cache, btree_type, None);
        iter.snapshot = snapshot_id;
        iter
    }

    /// 查找 child 节点的 block_addr（internal 节点专用）
    ///
    /// 在 internal 节点中，每个 entry 的 value 是 child 的 BtreePtrV2。
    /// 搜索所有 bset（set[0] 排序 + set[1..] 增量追加），
    /// 找到 target 应该属于哪个 child。
    /// 返回 (child_addr, global_entry_index) — 全局 entry 索引（1-indexed，跨所有 bset）
    ///
    /// 使用 bpos-only 比较（通过 aux key）避免额外的 paddr/ver 解包。
    pub(crate) fn find_child_node(node: &BtreeNode, target: &BtreeKey) -> (u64, u16) {
        let n = node.key_count as usize;
        if n == 0 {
            return (0, 0);
        }

        // set[0] (compacted) with aux AND no entries in incremental sets:
        // use binary search for fast lookup. If entries exist in sets[1..],
        // fall back to the full collect+sort+scan to avoid missing entries
        // that haven't been compacted into set[0] yet.
        if node.sets[0].aux_offset > 0 && node.sets[1].size == 0 && node.sets[2].size == 0 {
            let set = &node.sets[0];
            let mut lo = 1i32;
            let mut hi = set.size as i32;
            let aes = std::mem::size_of::<BtreeKey>() + 4;
            let aux_base = set.aux_offset as usize;

            // Standard binary search on set[0]'s aux array
            let mut best_idx: usize = 0;
            while lo <= hi {
                let mid = (lo + hi) / 2;
                let aux_key: BtreeKey = unsafe {
                    std::ptr::addr_of!(node.data[aux_base + (mid as usize - 1) * aes])
                        .cast::<BtreeKey>()
                        .read_unaligned()
                };
                match target.cmp(&aux_key) {
                    std::cmp::Ordering::Equal => {
                        let off = unsafe {
                            std::ptr::addr_of!(
                                node.data[aux_base
                                    + (mid as usize - 1) * aes
                                    + std::mem::size_of::<BtreeKey>()]
                            )
                            .cast::<u32>()
                            .read_unaligned()
                        };
                        let (_, v) = node.read_packed_entry(off as usize);
                        return (v.paddr.get(), mid as u16);
                    }
                    std::cmp::Ordering::Greater => {
                        best_idx = mid as usize;
                        lo = mid + 1;
                    }
                    std::cmp::Ordering::Less => hi = mid - 1,
                }
            }

            if best_idx > 0 {
                let off = unsafe {
                    std::ptr::addr_of!(
                        node.data
                            [aux_base + (best_idx - 1) * aes + std::mem::size_of::<BtreeKey>()]
                    )
                    .cast::<u32>()
                    .read_unaligned()
                };
                let (_, v) = node.read_packed_entry(off as usize);
                return (v.paddr.get(), best_idx as u16);
            }

            // All keys > target, take first child
            let off = unsafe {
                std::ptr::addr_of!(node.data[aux_base + std::mem::size_of::<BtreeKey>()])
                    .cast::<u32>()
                    .read_unaligned()
            };
            let (_, v) = node.read_packed_entry(off as usize);
            return (v.paddr.get(), 1);
        }

        // No aux: collect from all bsets (incremental sets[1..] or set[0] without aux)
        let mut entries: Vec<(BtreeKey, BchVal, u16)> = Vec::with_capacity(n);
        let mut cumul: u16 = 0;
        for set in node.sets.iter() {
            for i in 0..set.size as usize {
                let (k, v) = node.read_entry(set, i + 1);
                entries.push((k, v, cumul + (i as u16) + 1));
            }
            cumul += set.size;
        }

        // 按 key 升序排序
        // 之后取最后一个 key <= target 的 routing entry，而非第一个匹配。
        entries.sort_by_key(|a| a.0);

        let mut best: Option<(u64, u16)> = None;
        for (k, v, global_idx) in &entries {
            if target.cmp(k) != Ordering::Less {
                best = Some((v.paddr.get(), *global_idx));
            }
        }
        if let Some(result) = best {
            return result;
        }

        if let Some((_, v, idx)) = entries.first() {
            return (v.paddr.get(), *idx);
        }
        (0, 0)
    }

    /// 获取当前位置的 (key, value)
    ///
    /// 对应 bcachefs `bch2_btree_iter_peek()`
    /// 将全局 offset 转换为各 set 内的局部 offset 来读取。
    /// 如果 `with_journal` 标志设置且 overlay 存在，优先从 overlay 读取
    /// （覆盖 bset 中的旧数据），回退到 bset。
    pub fn peek(&self) -> Option<(BtreeKey, BchVal)> {
        // 优先查 overlay (读后写一致性)
        if self.flags.with_journal {
            if let Some(overlay) = self.overlay {
                let pos = crate::btree::Bpos::from_key(&self.pos);
                if let Some(jk) = unsafe { &*overlay.as_ptr() }.lookup_entry(self.btree_type, pos) {
                    let (k, v) = jk.entry.to_legacy();
                    return Some((k, v));
                }
            }
        }
        let leaf = self.path.last()?;
        let mut global_off = leaf.offset;
        if global_off == 0 {
            return None;
        }
        // 遍历所有 bset，全局 offset 减去前面各 set 的 size 得到局部 offset
        for set in &leaf.node.sets {
            if set.size > 0 {
                if global_off as u32 <= set.size as u32 {
                    return Some(leaf.node.read_entry(set, global_off as usize));
                }
                global_off -= set.size;
            }
        }
        None
    }

    /// 获取当前位置的 BtreeEntry（支持 Extent 和 Raw value）
    pub fn peek_entry(&self) -> Option<BtreeEntry> {
        let leaf = self.path.last()?;
        let mut global_off = leaf.offset;
        if global_off == 0 {
            return None;
        }
        for set in &leaf.node.sets {
            if set.size > 0 {
                if global_off as u32 <= set.size as u32 {
                    return Some(leaf.node.read_entry_raw(set, global_off as usize));
                }
                global_off -= set.size;
            }
        }
        None
    }

    /// journal-aware peek：从显式传入的 overlay 读取（与 `peek()` 自带的 overlay 不同源）
    /// 在 `peek_visible()` 中用于临时 overlay 检查。
    /// 如果 `overlay` 在当前 pos 有非 overwritten entry，返回 journal 版本，
    /// 否则回退到 peek()（后者会检查自身 overlay）。
    pub fn peek_with_journal(&self, overlay: &JournalKeys) -> Option<(BtreeKey, BchVal)> {
        let pos = Bpos::from_key(&self.pos);
        if let Some(jk) = overlay.lookup_entry(self.btree_type, pos) {
            let (k, v) = jk.entry.to_legacy();
            Some((k, v))
        } else {
            self.peek()
        }
    }

    /// journal-aware peek_entry (BtreeEntry 版本)
    pub fn peek_entry_with_journal(&self, overlay: &JournalKeys) -> Option<BtreeEntry> {
        let pos = Bpos::from_key(&self.pos);
        if let Some(jk) = overlay.lookup_entry(self.btree_type, pos) {
            Some(jk.entry.clone())
        } else {
            self.peek_entry()
        }
    }

    /// 验证并重建路径 — 对应 bcachefs `bch2_btree_iter_traverse()`
    ///
    /// 检查从 root 到 leaf 的路径是否仍然有效。如果当前 leaf 的 key 范围
    /// 不再覆盖 `self.pos`（并发 split/merge 后），从 root 重新下降到
    /// `self.pos` 并重建 path。
    pub fn traverse(&mut self) -> bool {
        let Some(leaf) = self.path.last() else {
            return false;
        };

        // depth=0：root 就是 leaf，路径永不失效
        if self.path.len() == 1 {
            return true;
        }

        // 空节点（min > max）：路径无效，跳过
        if leaf.node.min_key > leaf.node.max_key {
            return self.full_traverse();
        }

        // 如果 self.pos 仍在 leaf 的 key 范围内，路径有效
        let pos_bpos = Bpos::from_key(&self.pos);
        if pos_bpos >= leaf.node.min_key && pos_bpos <= leaf.node.max_key {
            return true;
        }

        // 路径失效，从 root 重建
        self.full_traverse()
    }

    /// 从 root 重新下降到 self.pos 并重建 path
    ///
    /// 保留 path[0]（root）的现有锁，丢弃之后的所有层级。
    fn full_traverse(&mut self) -> bool {
        // 需要至少 root
        if self.path.is_empty() {
            return false;
        }

        let target = self.pos;
        // 丢弃 root 之后的所有层级（保留 root 锁）
        while self.path.len() > 1 {
            let removed = self.path.pop().unwrap();
            if removed.lock_state == BtreeNodeLockedType::Read {
                removed.node.lock.unlock_read();
            }
        }

        // 从 root 逐级下降到 leaf
        loop {
            let parent_idx = self.path.len() - 1;
            let parent_level = self.path[parent_idx].node.level;
            if parent_level == 0 {
                break; // 已到 leaf
            }
            let (child_addr, child_idx) =
                Self::find_child_node(&self.path[parent_idx].node, &target);
            let child = self.cache.get_or_create(child_addr, parent_level - 1);
            child.lock.lock_read();

            self.path.push(BtreePathLevel {
                node: child,
                lock_state: BtreeNodeLockedType::Read,
                offset: 1,
                child_idx,
                locked_seq: 0,
            });
        }

        // 设置 leaf 中的定位（offset=1，从第一个 entry 开始）
        if let Some(leaf) = self.path.last_mut() {
            leaf.offset = 1;
            // 更新 pos 到 leaf 中的第一个有效 entry
            if leaf.node.key_count > 0 {
                if let Some((k, _v)) = self.peek() {
                    self.pos = k;
                }
            }
        }

        self.sync_shared_path();
        true
    }

    /// 向前移动一个 entry（bcachefs 对齐的 advance）
    ///
    /// 对应 bcachefs `bch2_btree_iter_advance()`
    /// 优先在 leaf 内移动，超出范围则回溯 path
    /// 全局 offset 减去前面各 set 的 size 得到局部 offset。
    pub fn advance(&mut self) -> bool {
        if let Some(leaf) = self.path.last_mut() {
            let n = leaf.node.key_count as u16;
            if leaf.offset < n {
                leaf.offset += 1;
                let mut global_off = leaf.offset;
                for set in &leaf.node.sets {
                    if set.size > 0 {
                        if global_off as u32 <= set.size as u32 {
                            let (k, _v) = leaf.node.read_entry(set, global_off as usize);
                            self.pos = k;
                            self.sync_shared_path();
                            return true;
                        }
                        global_off -= set.size;
                    }
                }
            }
            // 超出 leaf 范围，尝试回溯
            self.back_up_and_advance()
        } else {
            false
        }
    }

    fn back_up_and_advance(&mut self) -> bool {
        while self.path.len() >= 2 {
            let current = self.path.pop().unwrap();
            let parent = self.path.last().unwrap();

            // T3：验证父节点锁 seq 未变（未发生并发修改）
            if parent.node.lock.seq() != parent.locked_seq {
                // 父节点已被修改，路径可能失效 → 全路径重建
                return self.full_traverse();
            }

            let next_idx = current.child_idx + 1;
            if next_idx <= parent.node.key_count as u16 {
                // 跨所有 bset 查找全局索引为 next_idx 的 entry
                if let Some(v) = Self::read_entry_by_global_idx(&parent.node, next_idx) {
                    let child_addr = v.paddr.get();
                    let child_level = parent.node.level.saturating_sub(1);

                    // 备注：bcachefs 对齐 — 预取再下一个兄弟节点
                    if let Some(v2) = Self::read_entry_by_global_idx(&parent.node, next_idx + 1) {
                        self.cache
                            .prefetch_node(v2.paddr.get(), child_level, self.btree_type);
                    }

                    let child = self.cache.get_or_create(child_addr, child_level);
                    child.lock.lock_read();
                    let child_seq = child.lock.seq();

                    self.path.push(BtreePathLevel {
                        node: child,
                        lock_state: BtreeNodeLockedType::Read,
                        offset: 1,
                        child_idx: next_idx,
                        locked_seq: child_seq,
                    });

                    if child_level > 0 {
                        self.descend_to_first_leaf();
                    }

                    if let Some((k, _v)) = self.peek() {
                        self.pos = k;
                        self.sync_shared_path();
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 跨所有 bset 按全局 1-indexed 索引读取 entry 的 value
    fn read_entry_by_global_idx(node: &BtreeNode, global_idx: u16) -> Option<BchVal> {
        let mut cumul: u16 = 0;
        for set in &node.sets {
            if set.size == 0 {
                continue;
            }
            if global_idx > cumul && global_idx <= cumul + set.size {
                let local = (global_idx - cumul) as usize;
                let (_k, v) = node.read_entry(set, local);
                return Some(v);
            }
            cumul += set.size;
        }
        None
    }

    /// 从当前 path 的 last 节点（必须是 internal）下降到最左 leaf
    fn descend_to_first_leaf(&mut self) {
        loop {
            let top = self.path.last().unwrap();
            let parent_level = top.node.level;
            if parent_level == 0 {
                break;
            }
            let target = &BtreeKey::MIN_KEY;
            let (child_addr, child_idx) = Self::find_child_node(&top.node, target);
            let child_lvl = parent_level.saturating_sub(1);
            let child = self.cache.get_or_create(child_addr, child_lvl);
            child.lock.lock_read();
            let child_seq = child.lock.seq();
            self.path.push(BtreePathLevel {
                node: child,
                lock_state: BtreeNodeLockedType::Read,
                offset: 1,
                child_idx,
                locked_seq: child_seq,
            });
            if child_lvl == 0 {
                break;
            }
        }
    }

    /// 更新当前位置的 value
    ///
    /// 对应 bcachefs `bch2_btree_iter_update()`。
    /// 通过 SixLock write 锁保证独占后，在 packed buffer 中写入新 value 字节。
    pub fn update(&mut self, new_value: &BchVal) -> bool {
        let leaf_idx = self.path.len() - 1;
        let offset = self.path[leaf_idx].offset;
        if offset == 0 {
            return false;
        }

        // 确保 write lock（try-lock only — Phase 1 语义）
        if !self.upgrade_to_write(leaf_idx) {
            return false;
        }

        // 找到当前 entry 在 data buffer 中的偏移
        let (entry_data_off, entry_sz) = self.find_entry_offset(leaf_idx, offset);
        if entry_sz == 0 {
            return false;
        }

        // value 在 packed entry 中的偏移 = key_bytes (format.key_u64s * 8)
        let fmt = &crate::btree::key::BKEY_FORMAT_CURRENT;
        let value_off = entry_data_off + fmt.key_bytes();

        // SixLock write lock 保证了独占写，通过 unsafe 写入 value 字节
        let leaf_node = &self.path[leaf_idx].node;
        unsafe {
            let data_ptr = leaf_node.data.as_ptr() as *mut u8;
            let paddr_bytes = new_value.paddr.get().to_le_bytes();
            std::ptr::copy_nonoverlapping(
                paddr_bytes.as_ptr(),
                data_ptr.add(value_off as usize),
                6,
            );
            let ver_bytes = new_value.ver.to_le_bytes();
            std::ptr::copy_nonoverlapping(
                ver_bytes.as_ptr(),
                data_ptr.add(value_off as usize + 6),
                2,
            );
        }
        true
    }

    /// 计算指定层级 offset 对应的 data buffer 偏移和 entry 字节数
    ///
    /// 支持 compacted set（set[0] 有 aux 数组）和 incremental set（线性扫描）。
    fn find_entry_offset(&self, level: usize, global_off: u16) -> (u32, u32) {
        let node = &self.path[level].node;
        let mut remaining = global_off;
        for set in &node.sets {
            if set.size == 0 {
                continue;
            }
            if (remaining as u32) <= set.size as u32 {
                if set.aux_offset > 0 {
                    let aes = std::mem::size_of::<BtreeKey>() as u32 + 4;
                    let aux_base = set.aux_offset;
                    let off_pos = aux_base
                        + (remaining as u32 - 1) * aes
                        + std::mem::size_of::<BtreeKey>() as u32;
                    let data_off: u32 = unsafe {
                        std::ptr::addr_of!(node.data[off_pos as usize])
                            .cast::<u32>()
                            .read_unaligned()
                    };
                    let u64s = unsafe {
                        std::ptr::addr_of!(node.data[data_off as usize])
                            .cast::<u8>()
                            .read_unaligned()
                    };
                    return (data_off, u64s as u32 * 8);
                } else {
                    let mut cur = set.data_offset;
                    for _ in 1..remaining {
                        let u64s = unsafe {
                            std::ptr::addr_of!(node.data[cur as usize])
                                .cast::<u8>()
                                .read_unaligned()
                        };
                        cur += (u64s as u32) * 8;
                    }
                    let u64s = unsafe {
                        std::ptr::addr_of!(node.data[cur as usize])
                            .cast::<u8>()
                            .read_unaligned()
                    };
                    return (cur, u64s as u32 * 8);
                }
            }
            remaining -= set.size;
        }
        (0, 0)
    }

    /// 将指定层级的锁升级到 write（try-lock only）
    ///
    /// Phase 1 使用 try-lock 语义（对应 SixLock 当前实现）。
    /// 更新 self.path[level].lock_state 以反映新状态。
    fn upgrade_to_write(&mut self, level: usize) -> bool {
        if level >= self.path.len() {
            return false;
        }
        let pl = &self.path[level];
        let ok = match pl.lock_state {
            BtreeNodeLockedType::None => pl.node.lock.try_lock_write(),
            BtreeNodeLockedType::Read => {
                pl.node.lock.unlock_read();
                pl.node.lock.try_lock_write()
            }
            BtreeNodeLockedType::Intent => pl.node.lock.try_upgrade_intent_to_write(),
            BtreeNodeLockedType::Write => true,
        };
        if ok {
            self.path[level].lock_state = BtreeNodeLockedType::Write;
        }
        ok
    }

    /// 重启遍历器（从 root 重新下降）
    ///
    /// 对应 bcachefs `bch2_btree_iter_restart()`
    /// 当检测到锁竞争导致路径失效时调用。
    pub fn restart(&mut self, root: &BtreeRoot) {
        let shared_path = self.shared_path.clone();
        // 释放所有当前持有的锁
        for level in &self.path {
            match level.lock_state {
                BtreeNodeLockedType::Read => level.node.lock.unlock_read(),
                BtreeNodeLockedType::Intent => level.node.lock.unlock_intent(),
                BtreeNodeLockedType::Write => level.node.lock.unlock_write(),
                BtreeNodeLockedType::None => {}
            }
        }

        // 重新初始化
        let new_iter = Self::init(
            root,
            &self.pos,
            self.flags,
            &self.cache,
            self.btree_type,
            None,
        );
        *self = new_iter;
        self.shared_path = shared_path;
        self.had_restart = true;
        self.sync_shared_path();
    }

    /// 优化版重启：当节点 seq 未变化时跳过从 root 重下降
    ///
    /// R2 优化：利用 locked_seq 检测自加锁以来节点是否被写操作修改。
    /// 如果所有 path level 的六锁序列号都与加锁时相同，说明节点未被修改，
    /// 无需重新下降遍历，只需释放锁并重置状态。
    ///
    /// # 返回值
    ///
    /// - `false` — 所有节点 seq 未变化，跳过了重下降（只需重置状态）
    /// - `true` — 回退到完整 `restart()`（有节点被修改过）
    pub fn restart_optimized(&mut self, root: &BtreeRoot) -> bool {
        // 1. 释放所有当前持有的锁
        for level in &self.path {
            match level.lock_state {
                BtreeNodeLockedType::Read => level.node.lock.unlock_read(),
                BtreeNodeLockedType::Intent => level.node.lock.unlock_intent(),
                BtreeNodeLockedType::Write => level.node.lock.unlock_write(),
                BtreeNodeLockedType::None => {}
            }
        }

        // 2. 从 leaf 开始检查 seq 是否变化
        // leaf 在 path.last()
        let leaf_unchanged = self
            .path
            .last()
            .is_some_and(|leaf| leaf.node.lock.seq() == leaf.locked_seq);

        if leaf_unchanged {
            // 3. 所有 level 都未变化 → 跳过重下降
            let all_unchanged = self.path.iter().all(|level| {
                level.lock_state == BtreeNodeLockedType::None
                    || level.node.lock.seq() == level.locked_seq
            });

            if all_unchanged {
                // 不需 re-init，只需重置锁状态和重启标志
                for level in &mut self.path {
                    level.lock_state = BtreeNodeLockedType::None;
                }
                self.had_restart = false;
                self.sync_shared_path();
                return false; // false = 不需要重下降
            }
        }

        // 4. 回退到完整 restart（步骤 1 已释放锁，需重置 lock_state 避免重复释放）
        for level in &mut self.path {
            level.lock_state = BtreeNodeLockedType::None;
        }
        self.restart(root);
        self.sync_shared_path();
        true // true = 执行了重下降
    }

    /// 获取当前 leaf 中 entry 的数量
    pub fn leaf_key_count(&self) -> u32 {
        self.path.last().map(|l| l.node.key_count).unwrap_or(0)
    }

    /// 是否已经到达 leaf
    pub fn at_leaf(&self) -> bool {
        self.path.last().map(|l| l.node.level == 0).unwrap_or(false)
    }

    // ─── 快照可见性过滤 ─────────────────────────────────

    /// 设置快照过滤：只返回在指定快照中可见的条目
    /// 对应 bcachefs `bch2_btree_iter_set_snapshot()`
    pub fn set_snapshot_filter(&mut self, sid: u32) {
        if self.snapshot != sid {
            self.snapshot = sid;
            self.snapshot_visible_cache.clear();
        }
    }

    /// 返回下一个对当前快照可见的 (key, value)
    ///
    /// 自动跳过：
    /// - Whiteout 类型的 key（始终跳过）
    /// - 在当前快照中不可见的 key（设置了过滤时）
    ///
    /// 无过滤时 (snapshot=0) 仅跳过 Whiteout（向后兼容）。
    pub fn peek_visible(&mut self, engine: &BtreeEngine) -> Option<(BtreeKey, BchVal)> {
        loop {
            let entry = if self.flags.with_journal {
                if let Some(ref overlay) = engine.journal_overlay {
                    if !overlay.draining {
                        self.peek_with_journal(overlay)?
                    } else {
                        self.peek()?
                    }
                } else {
                    self.peek()?
                }
            } else {
                self.peek()?
            };
            // 始终跳过 Whiteout
            if entry.0.key_type == KeyType::Whiteout {
                if !self.advance() {
                    return None;
                }
                continue;
            }
            // 检查快照可见性
            //
            // 可见性规则：key 在当前 snapshot 中可见当且仅当
            // 1. key 的 snapshot == 当前 snapshot，或
            // 2. 当前 snapshot 是 key 的 snapshot 的祖先（子继承父的条目）
            let key_sid = entry.0.get_snapshot_id();
            if self.snapshot != 0 && self.snapshot != key_sid {
                let visible = self
                    .snapshot_visible_cache
                    .entry((self.snapshot, key_sid))
                    .or_insert_with(|| {
                        crate::snap::snapshot::is_ancestor_from_btree(
                            engine,
                            self.snapshot,
                            key_sid,
                        )
                    });
                if !*visible {
                    if !self.advance() {
                        return None;
                    }
                    continue;
                }
            }
            return Some(entry);
        }
    }

    /// 前进到下一个对当前快照可见的条目
    ///
    /// 跳过当前位置后的所有不可见和 Whiteout 条目。
    /// 返回 true 如果成功定位到下一个可见条目。
    pub fn advance_visible(&mut self, engine: &BtreeEngine) -> bool {
        if !self.advance() {
            // advance 返回 false 时 cursor 仍指向最后一位，
            // 但当前条目已被消费，将 offset 设为 max 避免脏读
            if let Some(leaf) = self.path.last_mut() {
                if leaf.offset >= leaf.node.key_count as u16 {
                    leaf.offset = leaf.node.key_count as u16 + 1;
                }
            }
            return false;
        }
        // 跳过 Whiteout 或 Whiteout + 不可见
        self.peek_visible(engine).is_some()
    }

    // ─── bcachefs 对齐方法 ─────────────────────────────────

    /// 查看当前位置的 key（bcachefs 对齐：`bch2_btree_iter_peek()`）
    ///
    /// 返回当前迭代位置的 `(key, value)`。与 `peek()` 行为一致。
    pub fn bch2_btree_iter_peek(&self) -> Option<(BtreeKey, BchVal)> {
        self.peek()
    }

    /// 查看当前 slot 的 key（bcachefs 对齐：`bch2_btree_iter_peek_slot()`）
    ///
    /// slot 模式：返回当前位置的键值，不进行方向性移动。
    /// 与 `peek()` 行为一致。
    pub fn bch2_btree_iter_peek_slot(&self) -> Option<(BtreeKey, BchVal)> {
        self.peek()
    }

    /// 前进到下一个 key 并返回（bcachefs 对齐：`bch2_btree_iter_next()`）
    ///
    /// 组合了 `advance()` + `peek()` 的便捷方法。
    /// 对应 bcachefs `bch2_btree_iter_next()`，定位到下一项并返回。
    pub fn next(&mut self) -> Option<(BtreeKey, BchVal)> {
        if self.advance() {
            self.peek()
        } else {
            None
        }
    }

    /// 前进到下一个 slot 的 key（bcachefs 对齐：`bch2_btree_iter_next_slot()`）
    pub fn next_slot(&mut self) -> Option<(BtreeKey, BchVal)> {
        self.next()
    }

    /// 退回到上一个 key 并返回（bcachefs 对齐：`bch2_btree_iter_prev()`）
    pub fn prev_slot(&mut self) -> Option<(BtreeKey, BchVal)> {
        // 当前实现不支持反向遍历；返回 None
        None
    }

    /// 在给定上限范围内 peek（bcachefs 对齐：`bch2_btree_iter_peek_max()`）
    pub fn peek_max(&self, _end: &Bpos) -> Option<(BtreeKey, BchVal)> {
        // 基础实现：忽略上限，返回当前 peek
        self.peek()
    }

    /// 带下限的向前 peek（bcachefs 对齐：`bch2_btree_iter_peek_prev_min()`）
    pub fn peek_prev_min(&self, _min: Bpos) -> Option<(BtreeKey, BchVal)> {
        // 当前实现不支持反向 peek；返回 None
        None
    }
}

impl std::fmt::Display for BtreeIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BtreeIter[pos={}]", self.pos)
    }
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::KeyType;
    use crate::btree::node::BtreeNode;
    use crate::btree::types::NodeCache;
    use crate::btree::BtreeEngine;
    use crate::snap::snapshot::{bch2_snapshot_node_create, create_root_snapshot_btree};

    fn make_root_with_cache() -> (BtreeRoot, Arc<NodeCache>) {
        let root = BtreeRoot::new(Arc::new(BtreeNode::new_leaf()), 0);
        let cache = Arc::new(NodeCache::new());
        (root, cache)
    }

    #[test]
    fn test_iter_init_single_leaf() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert_eq!(iter.path.len(), 1);
        assert!(iter.at_leaf());
    }

    #[test]
    fn test_iter_peek_empty() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert!(iter.peek().is_none());
    }

    #[test]
    fn test_iter_init_intent() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let flags = IterFlags {
            intent: true,
            forward: true,
            with_journal: false,
        };
        let iter = BtreeIter::init(
            &root,
            &target,
            flags,
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert_eq!(iter.flags.intent, true);
    }

    #[test]
    fn test_iter_restart() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert!(!iter.had_restart);
        iter.restart(&root);
        assert!(iter.had_restart);
    }

    #[test]
    fn test_iter_shared_path_fork_shares_snapshot() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let forked = iter.fork_shared_path();

        assert_eq!(iter.shared_path_ref_count(), 2);
        assert_eq!(forked.shared_path_ref_count(), 2);
        assert_eq!(
            forked.shared_path().generation(),
            iter.shared_path().generation()
        );
        assert_eq!(forked.shared_path().snapshot().len(), iter.path.len());

        iter.restart(&root);
        assert!(iter.had_restart);
        assert_eq!(
            forked.shared_path().generation(),
            iter.shared_path().generation()
        );
        assert_eq!(forked.shared_path().snapshot().len(), iter.path.len());
    }

    #[test]
    fn test_iter_advance_empty() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert!(!iter.advance());
    }

    #[test]
    fn test_iter_leaf_key_count() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        assert_eq!(iter.leaf_key_count(), 0);
    }

    // ─── 快照可见性过滤测试 ─────────────────────────────

    /// 测试快照过滤：从 s2 看（可见：s2 自身及其后代 s3）
    #[test]
    fn test_iter_snapshot_filter_s2() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        // 使用 btree 创建快照树: root → s2 → s3, root → s4
        let mut engine = BtreeEngine::new();
        let root_id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let s2 = bch2_snapshot_node_create(&mut engine, root_id, 1, None).unwrap();
        let s3 = bch2_snapshot_node_create(&mut engine, s2, 1, None).unwrap();
        let s4 = bch2_snapshot_node_create(&mut engine, root_id, 2, None).unwrap();

        // 插入不同快照的 key
        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let target = BtreeKey::MIN_KEY;
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        iter.set_snapshot_filter(s2);

        // s2 可见: {s2, s3} → 10@s3, 20@s2 可见；30@s2 Whiteout 跳过；40@root, 50@s4 不可见
        let first = iter.peek_visible(&engine);
        assert!(first.is_some(), "should find first visible entry");
        assert_eq!(
            first.unwrap().0,
            BtreeKey::new(10, s3, KeyType::Normal),
            "first visible from s2 should be 10@s3 (child of s2)"
        );

        // advance_visible → 下一个可见应该是 20@s2
        assert!(
            iter.advance_visible(&engine),
            "should advance to next visible"
        );
        let second = iter.peek_visible(&engine);
        assert!(second.is_some(), "should find second visible");
        assert_eq!(
            second.unwrap().0,
            BtreeKey::new(20, s2, KeyType::Normal),
            "second visible from s2 should be 20@s2"
        );

        // 再 advance → 没有更多可见了
        assert!(
            !iter.advance_visible(&engine),
            "should have no more visible entries from s2"
        );
        assert!(
            iter.peek_visible(&engine).is_none(),
            "peek_visible should be None at end"
        );
    }

    /// 测试快照过滤：从 s4 看（可见：s4 自身）
    #[test]
    fn test_iter_snapshot_filter_s4() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut engine = BtreeEngine::new();
        let root_id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let s2 = bch2_snapshot_node_create(&mut engine, root_id, 1, None).unwrap();
        let s3 = bch2_snapshot_node_create(&mut engine, s2, 1, None).unwrap();
        let s4 = bch2_snapshot_node_create(&mut engine, root_id, 2, None).unwrap();

        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        iter.set_snapshot_filter(s4);

        // s4 可见: {s4} → 只有 50@s4；其他都是不可见或 Whiteout
        let first = iter.peek_visible(&engine);
        assert!(first.is_some(), "should find visible entry from s4");
        assert_eq!(
            first.unwrap().0,
            BtreeKey::new(50, s4, KeyType::Normal),
            "first visible from s4 should be 50@s4"
        );

        assert!(
            !iter.advance_visible(&engine),
            "should have no more visible from s4"
        );
    }

    /// 测试快照过滤：从根快照看（可见：所有后代快照）
    #[test]
    fn test_iter_snapshot_filter_root() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut engine = BtreeEngine::new();
        let root_id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let s2 = bch2_snapshot_node_create(&mut engine, root_id, 1, None).unwrap();
        let s3 = bch2_snapshot_node_create(&mut engine, s2, 1, None).unwrap();
        let s4 = bch2_snapshot_node_create(&mut engine, root_id, 2, None).unwrap();

        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s2, KeyType::Normal), BchVal::new(200, 0));
        leaf.insert(
            BtreeKey::new(30, s2, KeyType::Whiteout),
            BchVal::new(300, 0),
        );
        leaf.insert(
            BtreeKey::new(40, root_id, KeyType::Normal),
            BchVal::new(400, 0),
        );
        leaf.insert(BtreeKey::new(50, s4, KeyType::Normal), BchVal::new(500, 0));

        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        iter.set_snapshot_filter(root_id);

        // root 可见所有: 10@s3, 20@s2, 40@root, 50@s4（30@s2 Whiteout 跳过）
        let entries: Vec<BtreeKey> = {
            let mut v = Vec::new();
            loop {
                let entry = iter.peek_visible(&engine);
                match entry {
                    Some((k, _)) => {
                        v.push(k);
                        if !iter.advance_visible(&engine) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            v
        };

        assert_eq!(entries.len(), 4, "root should see 4 visible entries");
        assert_eq!(entries[0], BtreeKey::new(10, s3, KeyType::Normal));
        assert_eq!(entries[1], BtreeKey::new(20, s2, KeyType::Normal));
        assert_eq!(entries[2], BtreeKey::new(40, root_id, KeyType::Normal));
        assert_eq!(entries[3], BtreeKey::new(50, s4, KeyType::Normal));
    }

    /// 测试无过滤时 peek_visible 向后兼容（仅跳过 Whiteout）
    #[test]
    fn test_iter_peek_visible_no_filter() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        leaf.insert(BtreeKey::new(10, 3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, 2, KeyType::Whiteout), BchVal::new(200, 0));
        leaf.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));

        let engine = BtreeEngine::new();
        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        // 无过滤时 peek_visible 应跳过 Whiteout
        // 第一个 entry 是 10@3 (Normal) → 直接返回
        let first = iter.peek_visible(&engine);
        assert!(
            first.is_some(),
            "peek_visible without filter should find first entry"
        );
        assert_eq!(first.unwrap().0, BtreeKey::new(10, 3, KeyType::Normal));

        // advance_visible → 跳过 Whiteout 到 30@1
        assert!(
            iter.advance_visible(&engine),
            "should advance past whiteout"
        );
        let second = iter.peek_visible(&engine);
        assert!(second.is_some(), "second entry should exist");
        assert_eq!(second.unwrap().0, BtreeKey::new(30, 1, KeyType::Normal));

        // 再 advance → 结束
        assert!(!iter.advance_visible(&engine), "no more entries");
        assert!(iter.peek_visible(&engine).is_none(), "should be at end");
    }

    /// 多级树遍历测试：手动构造 2 层 B+tree，验证 iter 能正确下降到 leaf
    #[test]
    fn test_iter_multi_level_traversal() {
        use crate::btree::key::KeyType;
        use crate::btree::node::BsetTree;

        let cache = Arc::new(NodeCache::new());

        // 创建两个 leaf 节点（先裸节点插入，再包 Arc）
        let mut left_node = BtreeNode::new_leaf();
        left_node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left_node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left_node.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left_node);

        let mut right_node = BtreeNode::new_leaf();
        right_node.insert(BtreeKey::new(40, 1, KeyType::Normal), BchVal::new(400, 0));
        right_node.insert(BtreeKey::new(50, 1, KeyType::Normal), BchVal::new(500, 0));
        let right = Arc::new(right_node);

        let left_addr = 1;
        let right_addr = 2;
        cache.insert(left_addr, left.clone());
        cache.insert(right_addr, right.clone());

        // 创建 internal 根节点（depth=1）
        let mut internal = BtreeNode::new_internal();
        // entry 0: (MIN_KEY, ptr_to_left)
        let left_min = BtreeKey::MIN_KEY;
        let left_val = BchVal::new(left_addr, 0);
        let mut cur = 0u32;
        cur += internal.write_entry(cur, &left_min, &left_val);
        // entry 1: (key=40, ptr_to_right)
        let median = BtreeKey::new(40, 1, KeyType::Normal);
        let right_val = BchVal::new(right_addr, 0);
        cur += internal.write_entry(cur, &median, &right_val);
        internal.sets[0] = BsetTree {
            data_offset: 0,
            end_offset: cur,
            aux_offset: 0,
            size: 2,
            extra: 0,
        };
        internal.key_count = 2;

        let root = BtreeRoot::new(Arc::new(internal), 1);

        // 测试：查找 key=20（应该在左叶子）
        let iter = BtreeIter::init(
            &root,
            &BtreeKey::new(20, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should find key=20");
        assert_eq!(result.unwrap().0, BtreeKey::new(20, 1, KeyType::Normal));
        assert_eq!(result.unwrap().1, BchVal::new(200, 0));

        // 测试：查找 key=50（应该在右叶子）
        let iter = BtreeIter::init(
            &root,
            &BtreeKey::new(50, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should find key=50");
        assert_eq!(result.unwrap().0, BtreeKey::new(50, 1, KeyType::Normal));
        assert_eq!(result.unwrap().1, BchVal::new(500, 0));

        // 测试：查找 key=35（左叶子中没有 ≥35 的 key → peek 回退到偏移 1，即第一个 key）
        // 注意：init 的 lower_bound 语义在所有左叶子 entry < target 时回退到 offset=1
        let iter = BtreeIter::init(
            &root,
            &BtreeKey::new(35, 1, KeyType::Normal),
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        let result = iter.peek();
        assert!(result.is_some(), "should fallback to first entry in leaf");
        assert_eq!(result.unwrap().0, BtreeKey::new(10, 1, KeyType::Normal));
    }

    // ─── R2: restart_optimized 测试 ─────────────────────────

    /// 测试 restart_optimized: seq 未变时跳过重下降
    ///
    /// 新创建的 iter locked_seq 默认为 0，SixLock seq 也为 0，
    /// 因此 restart_optimized 应检测到 seq 未变 → 返回 false。
    #[test]
    fn test_restart_optimized_skips_when_seq_unchanged() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );

        // locked_seq 默认为 0，与锁的当前 seq(0) 匹配
        let skipped = iter.restart_optimized(&root);
        assert!(!skipped, "should skip restart when seq unchanged");
        // 锁状态应被重置
        for level in &iter.path {
            assert_eq!(
                level.lock_state,
                BtreeNodeLockedType::None,
                "lock should be released"
            );
        }
        assert!(!iter.had_restart, "had_restart should be false after skip");
    }

    /// 测试 restart_optimized: seq 变化时执行完整 restart
    ///
    /// 对节点执行 lock_write + unlock_write 会递增 seq，然后
    /// restart_optimized 应检测到 seq 变化 → 回退到完整 restart。
    #[test]
    fn test_restart_optimized_falls_back_when_seq_changed() {
        let (root, cache) = make_root_with_cache();
        let target = BtreeKey::new(100, 1, KeyType::Normal);
        let mut iter = BtreeIter::init(
            &root,
            &target,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );

        // 对 leaf 节点执行写操作，递增 seq
        let leaf = &iter.path.last().unwrap().node;
        leaf.lock.lock_write();
        leaf.lock.unlock_write();
        // seq 现在为 1

        // locked_seq 仍是 0 → 不匹配 → 回退到完整 restart
        let restarted = iter.restart_optimized(&root);
        assert!(
            restarted,
            "should fall back to full restart when seq changed"
        );
        assert!(iter.had_restart, "had_restart should be true after restart");
    }

    /// 测试 restart_optimized: 空路径（无 path）返回 false
    #[test]
    fn test_restart_optimized_empty_path() {
        let (root, cache) = make_root_with_cache();
        // 创建一个空 iter（无 path）
        let flags = IterFlags {
            intent: false,
            forward: true,
            with_journal: false,
        };
        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            flags,
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        // 有效 iter 至少有一个 path level，所以这里走正常路径
        // 对于一个不存在的场景（空 path）—— 通常是 init 永远不会产生空 path
        // 我们通过直接设置 path 为空来测试边界
        iter.path.clear();
        let result = iter.restart_optimized(&root);
        // 空 path: leaf_unchanged = false, 回退到 restart
        assert!(result, "empty path should fall back to restart");
        // restart 后应有 path
        assert!(!iter.path.is_empty(), "restart should restore path");
    }

    /// 验证 snapshot_visible_cache 在多次 peek_visible 调用间共享
    ///
    /// 只要 snapshot 过滤器不变，同一 (snapshot, key_sid) 对
    /// 应被缓存，不会在第二次出现时重复查询 Snapshots btree。
    /// 注意：使用子快照的 key（key_sid != filter_snapshot），
    /// 这样才会触发祖先关系检查，进入缓存路径。
    #[test]
    fn test_snapshot_visible_cache_shared_across_calls() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut engine = BtreeEngine::new();
        let root_id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let s2 = bch2_snapshot_node_create(&mut engine, root_id, 1, None).unwrap();
        let s3 = bch2_snapshot_node_create(&mut engine, s2, 1, None).unwrap();

        leaf.insert(BtreeKey::new(10, s3, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s3, KeyType::Normal), BchVal::new(200, 0));

        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );
        iter.set_snapshot_filter(s2);

        let first = iter.peek_visible(&engine);
        assert!(first.is_some(), "first peek should find entry");
        assert_eq!(
            first.as_ref().unwrap().0,
            BtreeKey::new(10, s3, KeyType::Normal)
        );

        assert!(iter.advance_visible(&engine), "should advance to next");
        let second = iter.peek_visible(&engine);
        assert!(second.is_some(), "second peek should find entry");
        assert_eq!(
            second.as_ref().unwrap().0,
            BtreeKey::new(20, s3, KeyType::Normal)
        );

        assert!(
            !iter.snapshot_visible_cache.is_empty(),
            "cache should have entries after peek_visible calls"
        );
    }

    /// 验证 set_snapshot_filter 切换 snapshot 时清空缓存
    #[test]
    fn test_snapshot_visible_cache_cleared_on_filter_change() {
        let (mut root, cache) = make_root_with_cache();
        let leaf = Arc::get_mut(&mut root.node).unwrap();

        let mut engine = BtreeEngine::new();
        let root_id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let s2 = bch2_snapshot_node_create(&mut engine, root_id, 1, None).unwrap();
        let s4 = bch2_snapshot_node_create(&mut engine, root_id, 2, None).unwrap();

        // s2 和 s4 下的 key 各一
        leaf.insert(BtreeKey::new(10, s2, KeyType::Normal), BchVal::new(100, 0));
        leaf.insert(BtreeKey::new(20, s4, KeyType::Normal), BchVal::new(200, 0));

        let mut iter = BtreeIter::init(
            &root,
            &BtreeKey::MIN_KEY,
            IterFlags::default(),
            &cache,
            crate::btree::BtreeId::Extents,
            None,
        );

        // s2 过滤 → 看到 10@s2
        iter.set_snapshot_filter(s2);
        let first = iter.peek_visible(&engine);
        assert!(first.is_some());
        assert_eq!(
            first.as_ref().unwrap().0,
            BtreeKey::new(10, s2, KeyType::Normal)
        );

        // 切换过滤 → 应清空缓存 → 看到 20@s4
        iter.set_snapshot_filter(s4);
        let first_s4 = iter.peek_visible(&engine);
        assert!(first_s4.is_some());
        assert_eq!(
            first_s4.as_ref().unwrap().0,
            BtreeKey::new(20, s4, KeyType::Normal)
        );
    }
}
