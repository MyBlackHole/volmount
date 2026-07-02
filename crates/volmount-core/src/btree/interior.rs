//! B-tree 内部节点操作（split/merge/rewrite/set_root）— bcachefs 对齐
//!
//! 对应 bcachefs `interior.h` + `interior.c`，提供节点级拓扑变更操作。
//!
//! ## API 概述
//!
//! | 函数 | bcachefs 对应 | 说明 |
//! |------|--------------|------|
//! | `btree_split_leaf` | `bch2_btree_split_leaf` | 叶节点分裂（触发递归向上） |
//! | `btree_split` | `btree_split` | 通用节点分裂（递归向上至 root） |
//! | `btree_merge` | `__bch2_foreground_maybe_merge` | 节点合并（underfull 检查） |
//! | `btree_increase_depth` | `bch2_btree_increase_depth` | 创建新根，增加树深度 |
//! | `btree_node_rewrite` | `bch2_btree_node_rewrite_*` | 节点重写（compact/重写） |
//! | `btree_set_root` | `bch2_btree_set_root_inmem` | 更新根节点指针 |
//! | `btree_set_root_for_read` | `bch2_btree_set_root_for_read` | 读路径设置根节点 |
//! | `btree_root_alloc_fake` | `bch2_btree_root_alloc_fake` | 分配假根节点（恢复阶段） |
//!
//! ## 设计说明
//!
//! volmount 的节点分裂/合并已经在 `Btree` 上实现（`split_root`、`insert_multi`、
//! `insert_routing_entry_at`、`try_merge_node`、`collapse_root`）。
//! 本模块将这些功能的内部接口整理为 bcachefs 对齐的公开 API，并补充
//! 缺少的操作（`increase_depth`、`rewrite`、`set_root`、`root_alloc_fake`）。
//!
//! 生命周期（bcachefs `btree_update` 状态机）：
//! ```text
//! Init → NodesAllocated → UpdateParent → Done
//! ```

use std::sync::Arc;

use crate::btree::key::{BchVal, Bpos, BtreeKey};
use crate::btree::node::{BsetTree, BtreeNode};
use crate::btree::types::{NodeCache, BTREE_MAX_DEPTH};
use crate::btree::Btree;

// ---------------------------------------------------------------------------
// 更新模式 & 重写原因 — bcachefs 对齐
// ---------------------------------------------------------------------------

/// Btree 节点重写原因 — 对应 bcachefs `enum btree_node_rewrite_reason`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BtreeNodeRewriteReason {
    /// 非重写（默认）
    None = 0,
    /// 格式迁移：节点 key format 已变更需重写
    Format = 1,
    /// 节点损伤：CRC 校验失败等需要重写恢复
    Corrupt = 2,
    /// 内部节点需重新计算格式
    InternalFormat = 3,
    /// 预分裂后需重写节点（shard 对齐等）
    PreSplit = 4,
    /// 快照清理后需重写节点
    SnapshotCleanup = 5,
}

// ---------------------------------------------------------------------------
// 预留计算辅助函数
// ---------------------------------------------------------------------------

/// 计算最坏情况下节点分裂所需的预留节点数 — 对应 bcachefs `btree_update_reserve_required`
///
/// 一直分裂到根节点，然后分配一个新根（除非已达最大深度）。
pub fn btree_update_reserve_required(depth: u8, node_level: u8) -> usize {
    let depth = depth as usize;
    let node_level = node_level as usize;
    if depth < BTREE_MAX_DEPTH {
        (depth - node_level) * 2 + 1
    } else {
        (depth - node_level) * 2 - 1
    }
}

/// 检查节点是否需要合并 — 对应 bcachefs `btree_node_needs_merge`
///
/// 基于实际 data 字节数与 node_size 的比例判断。
pub fn btree_node_needs_merge(b: &BtreeNode) -> bool {
    // 当 node 数据量不足 1/3 时触发合并
    node_underfull(b)
}

/// 重置节点的 sibling u64s 估计值
pub fn btree_node_reset_sib_u64s(_b: &BtreeNode) {
    // volmount 没有 sib_u64s 字段，此为空操作
    // 在完整实现中，应更新节点两侧兄弟的大小估计值
}

// ---------------------------------------------------------------------------
// BtreeInteriorUpdate — 内部更新的 Rust 封装（引用 update.rs 的现有类型）
// ---------------------------------------------------------------------------

/// 重新导出 `BtreeInteriorUpdate` 类型别名
pub use crate::btree::update::{
    BtreeInteriorUpdate, BtreeUpdateMode, InteriorUpdateState, InteriorUpdateType,
};

// ---------------------------------------------------------------------------
// 分裂操作（Split）
// ---------------------------------------------------------------------------

/// 叶节点分裂 — 对应 bcachefs `bch2_btree_split_leaf`
///
/// 当叶节点满时触发分裂。如果分裂向上传播至根节点，触发根节点分裂。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `target` — 触发分裂的 key
/// * `value` — 触发分裂的 value
///
/// # 返回值
///
/// `true` 表示分裂成功，`false` 表示分裂失败（如被阻塞或资源不足）。
pub fn btree_split_leaf(btree: &mut Btree, target: &BtreeKey, value: &BchVal) -> bool {
    btree.insert_with_transaction(*target, *value, None, 0)
}

/// 通用节点分裂 — 对应 bcachefs `btree_split`（interior.c 中的核心实现）
///
/// 对指定地址的节点执行分裂。如果节点是根节点，分裂后深度增加；
/// 否则向上传播到 parent。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `node_addr` — 需要分裂的节点地址
/// * `trigger_key` — 触发分裂的 key（可能已插入到节点中）
/// * `trigger_val` — 触发分裂的 value
///
/// # 返回值
///
/// `true` 表示分裂成功。
///
/// # 说明
///
/// 当前实现将分裂代理到 btree 的内部 `split_root`/`insert_multi` 方法。
/// 将当节点不是根节点时，使用 `insert_routing_entry_at` 将分裂向上传播。
pub fn btree_split(
    btree: &mut Btree,
    _node_addr: u64,
    trigger_key: &BtreeKey,
    trigger_val: &BchVal,
) -> bool {
    // 检查是否为根节点
    if btree.depth() == 0 {
        // depth=0: 根节点就是唯一的 leaf
        return btree.insert(*trigger_key, *trigger_val, 0);
    }

    // 构建从 root 到目标节点的路径
    let mut path = Vec::new();
    let leaf_addr = btree.find_path_to_leaf_internal(trigger_key, &mut path);

    if Some(_node_addr) == leaf_addr {
        // 到达叶节点：使用 insert（内含分裂逻辑）
        btree.insert(*trigger_key, *trigger_val, 0)
    } else {
        // 非叶节点分裂：当前简化实现返回 false
        // （完整实现需要遍历内部节点路径、取出节点、执行分裂并更新 parent）
        false
    }
}

/// 根节点更新 — 对应 bcachefs `bch2_btree_set_root_inmem`
///
/// 将指定节点设置为 btree 的新根节点。
/// 该节点应为从原根节点分裂出的新 internal 节点。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `new_root` — 新的根节点
///
/// # 说明
///
/// 新根的 `level` 必须等于原根 `level + 1`。
/// 完成后 `depth` 增加 1。
pub fn btree_set_root(btree: &mut Btree, new_root: Arc<BtreeNode>) {
    // volmount 中 depth = root.node.level，新根的 level 就是新深度
    debug_assert!(
        new_root.level == btree.depth() + 1 || new_root.level == btree.depth(),
        "new root level {} must be within 1 of current depth {}",
        new_root.level,
        btree.depth()
    );
    btree.set_root_internal(new_root);
}

/// 读路径设置根节点 — 对应 bcachefs `bch2_btree_set_root_for_read`
///
/// 从后端读出根节点后调用，直接将节点设置为根。
/// 与 `btree_set_root` 不同，此函数假设节点已经是当前树的根，
/// 不验证 level，但必须不是已经挂在树上的当前 root。
pub fn btree_set_root_for_read(btree: &mut Btree, node: Arc<BtreeNode>) {
    assert!(
        !Arc::ptr_eq(&btree.root().node, &node),
        "btree_set_root_for_read must not be called with the current root"
    );
    btree.set_root_internal(node);
    // 重新统计 key 数
    btree.reset_key_count();
}

// ---------------------------------------------------------------------------
// 深度增长（Increase Depth）
// ---------------------------------------------------------------------------

/// 增加 btree 深度 — 对应 bcachefs `bch2_btree_increase_depth`
///
/// 创建一个新的 internal 根节点，将原有根节点作为其唯一的 child。
/// 当所有叶节点都接近满负荷且深度增长有助于减少分裂频率时调用。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `new_internal` — 新根节点（可选，不传则自动创建）
/// * `child_addr` — 作为新根唯一 child 的节点地址
///
/// # 返回值
///
/// 返回新根节点的 block_addr。
/// # 说明
///
/// volmount 中节点使用 cache 分配，所以 `child_addr` 必须已被
/// `NodeCache` 持有。
///
/// 对应 bcachefs `__btree_increase_depth`：
/// 1. 创建 level = root.level + 1 的新节点
/// 2. 将旧 root 作为新根的唯一 child routing entry
/// 3. 设置新根为 btree 的根
/// 4. 原根节点不再是根（但数据不变）
pub fn btree_increase_depth(btree: &mut Btree, child_addr: u64) -> Option<u64> {
    let cache = btree.cache_mut();
    let old_root = btree.root();
    let old_depth = old_root.depth;

    // 已达最大深度，无法继续增长
    if old_depth as usize >= BTREE_MAX_DEPTH {
        return None;
    }

    let new_level = old_depth + 1; // 新根 level = 原深度 + 1
    let new_addr = cache.alloc_addr();
    let mut new_root = BtreeNode::new_internal();
    new_root.level = new_level;
    new_root.node_size = old_root.node.node_size;

    // 新根只有一个 routing entry：MIN_KEY -> 旧 root
    let mut cur = 0u32;
    cur += new_root.write_entry(
        cur,
        &BtreeKey::MIN_KEY,
        &crate::btree::key::BchVal::new(child_addr, 0),
    );

    new_root.sets[0] = BsetTree {
        data_offset: 0,
        end_offset: cur,
        aux_offset: 0,
        size: 1,
        extra: 0,
    };
    new_root.key_count = 1;
    new_root.min_key = old_root.node.min_key;
    new_root.max_key = old_root.node.max_key;

    let new_root_arc = Arc::new(new_root);
    // bcachefs 对齐：新节点在首次写入前设置 will_make_reachable，阻止 eviction
    new_root_arc.set_will_make_reachable();
    cache.insert_dirty(new_addr, new_root_arc);

    // 更新根节点
    let new_root_arc = cache.get_or_create(new_addr, new_level);
    btree.set_root_internal(new_root_arc);

    Some(new_addr)
}

// ---------------------------------------------------------------------------
// 合并操作（Merge）
// ---------------------------------------------------------------------------

/// 前台合并尝试 — 对应 bcachefs `bch2_foreground_maybe_merge`
///
/// 检查指定 level 的节点是否需要合并（underfull），
/// 如果是则尝试与相邻兄弟合并。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `node_addr` — 需要检查的节点地址
/// * `ancestors` — 从 root 到 node.parent 的路径地址列表
///
/// # 返回值
///
/// `true` 表示合并在当前 level 完成（或无需合并），
/// `false` 表示合并失败（如被阻塞）。
pub fn btree_merge(btree: &mut Btree, node_addr: u64, ancestors: &[u64]) -> bool {
    btree.try_merge_node(node_addr, ancestors)
}

/// 检查节点是否 underfull — 用于合并决策
fn node_underfull(node: &BtreeNode) -> bool {
    node.total_data_bytes() < node.node_size / 3
}

// ---------------------------------------------------------------------------
// 重写操作（Rewrite）
// ---------------------------------------------------------------------------

/// 节点重写 — 对应 bcachefs `bch2_btree_node_rewrite_pos`
///
/// 重新打包指定节点的所有 entries（compact + 重写）。
/// 适用于：
/// - 节点格式迁移（format 变化）
/// - 节点碎片整理
/// - 节点内容在内存中已过期需重建
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `node_addr` — 需要重写的节点地址
///
/// # 返回值
///
/// `true` 表示重写成功。
///
/// # 说明
///
/// 重写不改变节点的 key 内容，只重新整理内部存储格式。
/// 重写后节点在 cache 中的 block_addr 不变。
/// 对应 bcachefs 中的 `btree_node_rewrite` — 这是一个轻量级操作，
/// 通常由后台线程或前台 compact 触发。
pub fn btree_node_rewrite(btree: &mut Btree, node_addr: u64) -> bool {
    let cache = btree.cache_mut();

    // 取出节点
    let mut node_arc = match cache.take_node(node_addr) {
        Some(n) => n,
        None => return false,
    };
    let node = match Arc::get_mut(&mut node_arc) {
        Some(n) => n,
        None => {
            cache.put_node(node_addr, node_arc);
            return false;
        }
    };

    // 执行 compact（排序去重过滤 whiteout + 重建 aux）
    node.compact();

    // 放回 cache（标记 dirty，确保 flush）
    cache.insert_dirty(node_addr, node_arc);
    true
}

/// 节点 key 更新重写 — 对应 bcachefs `bch2_btree_node_rewrite_key`
///
/// 重写节点同时更新其 key 元数据。
/// 用于节点指针变更（如 journal_seq 更新）后的原地重写。
///
/// # 参数
///
/// * `btree` — 目标 btree
/// * `node_addr` — 需要重写的节点地址
/// * `_btree_id` — btree 类型标识
/// * `_level` — 节点层级
///
/// # 返回值
///
/// `true` 表示重写成功。
pub fn btree_node_rewrite_key(
    btree: &mut Btree,
    node_addr: u64,
    _btree_id: u8,
    _level: u8,
) -> bool {
    btree_node_rewrite(btree, node_addr)
}

// ---------------------------------------------------------------------------
// 假根节点分配（Fake Root Alloc）
// ---------------------------------------------------------------------------

/// 分配假根节点（恢复阶段使用）— 对应 bcachefs `bch2_btree_root_alloc_fake`
///
/// 在 btree 恢复阶段（journal replay 之前）调用，
/// 为指定 btree type 分配一个内存占位根节点。
/// 该节点会被标记为 "fake"，在后续重写操作中才会真正写入后端。
///
/// # 参数
///
/// * `cache` — 节点缓存
/// * `level` — 假根节点的层级（通常为 0：leaf）
///
/// # 返回值
///
/// 返回 `(addr, Arc<BtreeNode>)`，其中 addr 是分配给假根的 block_addr。
///
/// # 说明
///
/// 假根节点与普通节点的区别：
/// - 数据为空（0 entries）
/// - 被标记为 "need_rewrite"（对应 bcachefs `set_btree_node_need_rewrite`）
/// - 后续首次写入时会被真实数据替换
pub fn btree_root_alloc_fake(cache: &NodeCache, level: u8) -> (u64, Arc<BtreeNode>) {
    let addr = cache.alloc_addr();
    // 手动创建节点（避免 alloc_node 的 Arc.clone 导致 get_mut 失败）
    let mut node = if level == 0 {
        BtreeNode::new_leaf()
    } else {
        BtreeNode::new_internal()
    };
    node.set_need_rewrite();
    // 假节点覆盖整个 key 空间
    node.min_key = Bpos::MIN;
    node.max_key = Bpos::MAX;
    let node_arc = Arc::new(node);
    cache.insert(addr, node_arc.clone());
    (addr, node_arc)
}

// ---------------------------------------------------------------------------
// 内存节点操作辅助
// ---------------------------------------------------------------------------

/// 从 cache 中取出节点并执行 compact 重写
///
/// 用于需要确保节点内存状态有序的场景（如分裂前的准备）。
pub fn btree_node_compact(cache: &NodeCache, node_addr: u64) -> bool {
    let mut node_arc = match cache.take_node(node_addr) {
        Some(n) => n,
        None => return false,
    };
    let node = match Arc::get_mut(&mut node_arc) {
        Some(n) => n,
        None => {
            cache.put_node(node_addr, node_arc);
            return false;
        }
    };
    node.compact();
    cache.insert_dirty(node_addr, node_arc);
    true
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{BtreeKey, KeyType};
    use crate::btree::node::BtreeNode;
    use crate::btree::types::NodeCache;

    #[test]
    fn test_btree_update_reserve_required_leaf() {
        // depth=3, node_level=0（leaf）→ (3-0)*2+1 = 7
        let r = btree_update_reserve_required(3, 0);
        assert_eq!(r, 7);
    }

    #[test]
    fn test_btree_update_reserve_required_max_depth() {
        // depth = BTREE_MAX_DEPTH, node_level = 0 → (BTREE_MAX_DEPTH-0)*2-1
        let r = btree_update_reserve_required(BTREE_MAX_DEPTH as u8, 0);
        assert_eq!(r, BTREE_MAX_DEPTH * 2 - 1);
    }

    #[test]
    fn test_btree_node_needs_merge_empty_leaf() {
        let node = BtreeNode::new_leaf();
        // 空节点 data_bytes = 0 < node_size/3 → true
        assert!(btree_node_needs_merge(&node));
    }

    #[test]
    fn test_btree_node_needs_merge_full_leaf() {
        let mut node = BtreeNode::new_leaf();
        // DEFAULT_NODE_SIZE = 256 * 1024 ≈ 262144
        // 1/3 ≈ 87381；每个 key ~24B → 约 4000 个 key 可填满 1/3
        for i in 0..6000u64 {
            if !node.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 10, 0)) {
                break;
            }
        }
        // 填充较多数据后节点应超过 1/3 阈值
        // 注：总 data_bytes = ∑ end_offset，非精确计数
        assert!(
            node.total_data_bytes() > node.node_size / 3,
            "node {} bytes > 1/3 of {} after 6000 entries",
            node.total_data_bytes(),
            node.node_size
        );
        assert!(!btree_node_needs_merge(&node));
    }

    #[test]
    fn test_btree_set_root_basic() {
        let mut btree = Btree::new();
        let old_depth = btree.depth();

        let new_node = Arc::new(BtreeNode::new_internal());
        assert_eq!(new_node.level, 1);

        btree_set_root(&mut btree, new_node);
        assert_eq!(btree.depth(), old_depth + 1);
    }

    #[test]
    fn test_btree_set_root_for_read() {
        let mut btree = Btree::new();
        let node = Arc::new(BtreeNode::new_leaf());
        btree_set_root_for_read(&mut btree, node);
        assert_eq!(btree.depth(), 0);
    }

    #[test]
    #[should_panic(expected = "btree_set_root_for_read must not be called with the current root")]
    fn test_btree_set_root_for_read_rejects_current_root() {
        let mut btree = Btree::new();
        let node = btree.root().node.clone();
        btree_set_root_for_read(&mut btree, node);
    }

    #[test]
    fn test_btree_root_alloc_fake_leaf() {
        let cache = NodeCache::new();
        let (addr, node) = btree_root_alloc_fake(&cache, 0);
        assert!(addr > 0, "fake root addr should be > 0");
        assert_eq!(node.level, 0);
        assert!(node.need_rewrite(), "fake root should require rewrite");
        assert_eq!(node.min_key, Bpos::MIN);
        assert_eq!(node.max_key, Bpos::MAX);
        // 验证节点已缓存
        assert!(cache.get(addr).is_some());
    }

    #[test]
    fn test_btree_root_alloc_fake_internal() {
        let cache = NodeCache::new();
        let (addr, node) = btree_root_alloc_fake(&cache, 1);
        assert_eq!(node.level, 1);
        assert!(node.need_rewrite(), "fake root should require rewrite");
        assert!(cache.get(addr).is_some());
    }

    #[test]
    fn test_btree_node_rewrite_small_node() {
        let mut btree = Btree::new();
        // 在 btree 的 cache 中分配节点（rewrite 使用 btree.cache_mut）
        let addr = btree.cache_mut().alloc_addr();
        let mut node = BtreeNode::new_leaf();
        for i in 0..10u64 {
            node.insert(BtreeKey::new(i, 1, KeyType::Normal), BchVal::new(i * 10, 0));
        }
        btree.cache_mut().insert(addr, Arc::new(node));

        // 执行 rewrite
        assert!(btree_node_rewrite(&mut btree, addr));

        // 验证节点还在 cache 中且 key 可查
        let node_after = btree
            .cache_mut()
            .get(addr)
            .expect("node should be in cache after rewrite");
        assert!(node_after
            .search(&BtreeKey::new(5, 1, KeyType::Normal))
            .is_some());
    }

    #[test]
    fn test_btree_node_compact_after_insert() {
        let cache = NodeCache::new();
        // 手动创建并插入节点（避免 alloc_node 的 clone 导致 Arc::get_mut 失败）
        let addr = cache.alloc_addr();
        let mut node = BtreeNode::new_leaf();
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        node.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        node.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(150, 0));
        cache.insert(addr, Arc::new(node));

        assert!(btree_node_compact(&cache, addr));

        // compact 后应有 2 个 key（20 保持，10 覆盖为 150）
        let n = cache.get(addr).unwrap();
        let found = n.search(&BtreeKey::new(10, 1, KeyType::Normal));
        assert!(found.is_some());
        assert_eq!(found.unwrap().1, BchVal::new(150, 0));
    }

    #[test]
    fn test_btree_split_leaf_basic() {
        let mut btree = Btree::new();
        // 小节点加速分裂
        let root = btree.root_node_mut_internal();
        root.node_size = 256;

        // 插入 key 直到触发分裂
        let mut inserted = 0u64;
        for i in 0..50u64 {
            if btree.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ) {
                inserted += 1;
            }
        }
        // 分裂后 key 应该都在
        assert_eq!(inserted, 50);
        for i in 0..50u64 {
            let found = btree.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist after splits", i);
        }
    }

    #[test]
    fn test_btree_increase_depth_small() {
        let mut btree = Btree::new();
        let child_addr = 42;
        let new_addr = btree_increase_depth(&mut btree, child_addr).unwrap();

        assert_eq!(btree.depth(), 1);
        assert!(new_addr > 0);
        assert_eq!(btree.root().node.key_count, 1);

        let set = &btree.root().node.sets[0];
        let (key, value) = btree.root().node.read_entry(set, 1);
        assert_eq!(key, BtreeKey::MIN_KEY);
        assert_eq!(value, BchVal::new(child_addr, 0));

        let dirty = btree.flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].0, new_addr);
    }

    #[test]
    fn test_btree_merge_after_delete() {
        let mut btree = Btree::new();

        // 小节点强制分裂
        btree.root_node_mut_internal().node_size = 512;

        // 插入 30 个 key → 多个 leaf
        for i in 0..30u64 {
            assert!(btree.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            ));
        }

        // 从左 leaf 删除大量 key，触发合并
        for i in 0..12u64 {
            btree.delete(&BtreeKey::new(i, 1, KeyType::Normal), 0);
        }

        // 验证剩余 key 可达
        for i in 12..30u64 {
            let found = btree.get(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(found.is_some(), "key {} should exist after merge", i);
        }
        assert_eq!(btree.key_count(), 18);
    }

    #[test]
    fn test_btree_reserve_required_constants() {
        // depth=2, node_level=1 → (2-1)*2+1 = 3
        assert_eq!(btree_update_reserve_required(2, 1), 3);
        // depth=5, node_level=0 → (5-0)*2+1 = 11
        assert_eq!(btree_update_reserve_required(5, 0), 11);
    }
}
