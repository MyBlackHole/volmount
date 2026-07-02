//! Btree 搜索实现 — 节点优先搜索
//!
//! 搜索优先级（bcachefs 对齐）：
//! 1. **节点本地搜索** — 先找到目标 key 所在的 leaf 节点，在该节点内搜索所有 bsets。
//!    节点本地始终比 journal overlay 更新，因为写入操作总是先写入 overlay
//!    再通过 replay 落盘。
//! 2. **overlay 读穿透** — 由 `BtreeEngine::search()` / `get_entry()` 负责。
//!
//! # 设计说明
//!
//! `Btree::search()` 与 `Btree::get()` 的区别：
//! - `get()` 使用 `BtreeIter::init()` 做完整树遍历，返回第一个 key 匹配的 entry。
//! - `search()` 只负责节点本地（node-local）搜索。
//! - `BtreeEngine::search()` / `get_entry()` 先查 journal overlay，再回落到节点本地。
//!
//! 这对应 bcachefs 的 btree_node 搜索模式：先在本节点的 bsets 中找，
//! 再到 overflow btree（溢出树）中找。本任务只实现 Phase 1（node-local），
//! Phase 2（overflow）留到 Phase C2。

use std::sync::Arc;

use crate::btree::iter::BtreeIter;
use crate::btree::key::{BchVal, BtreeKey};
use crate::btree::node::BtreeNode;
use crate::btree::types::{BtreeRoot, NodeCache};
use crate::btree::Btree;

impl Btree {
    /// 在 btree 中搜索 target key。
    ///
    /// # 搜索顺序
    ///
    /// 1. **节点本地搜索** — 从 root 下降到目标 leaf 节点，在该节点内搜索所有 bsets。
    ///    如果 key 在节点本地找到，立即返回（无需继续搜索 overflow）。
    /// 2. **overlay 读穿透** — 由 `BtreeEngine::search()` / `get_entry()` 负责。
    ///
    /// # 返回值
    ///
    /// 返回匹配的 `(BtreeKey, BchVal)`，未找到则返回 `None`。
    pub fn search(&self, target: &BtreeKey) -> Option<(BtreeKey, BchVal)> {
        let root = self.root();
        let cache = self.cache();

        // Phase 1: 节点本地搜索
        // depth=0: 单 leaf 树，直接使用根节点
        // depth>0: 从 root 下降到目标 leaf 节点
        let leaf: Option<Arc<BtreeNode>> = if root.depth == 0 {
            Some(Arc::clone(&root.node))
        } else {
            find_leaf_node(root, target, cache)
        };

        if let Some(ref node) = leaf {
            if let Some(result) = node.search(target) {
                return Some(result);
            }
        }

        None
    }
}

/// 从 root 下降到目标 key 所在的 leaf 节点。
///
/// 使用与 `BtreeIter::init` 一致的逐级下降策略：
/// 每层通过 `BtreeIter::find_child_node` 定位下一个 child 地址，
/// 直到下降到 level=0 的 leaf 节点。
///
/// # 参数
///
/// * `root` — 树根（包含根节点和树的深度）
/// * `target` — 搜索目标 key
/// * `cache` — 节点缓存，用于获取/创建中间节点
///
/// # 返回值
///
/// 返回目标 leaf 节点的 `Arc<BtreeNode>` 引用。
/// 如果树损坏（如空 internal 节点找不到子节点），返回 `None`。
fn find_leaf_node(
    root: &BtreeRoot,
    target: &BtreeKey,
    cache: &NodeCache,
) -> Option<Arc<BtreeNode>> {
    let mut current = Arc::clone(&root.node);

    // 从最深层逐级下降：level 从 root.depth 递减到 1
    // level 对应从 root 到 parent 的距离
    for level in (1..=root.depth).rev() {
        let (child_addr, _child_idx) = BtreeIter::find_child_node(&current, target);
        if child_addr == 0 {
            // child_addr == 0 表示树损坏（空 internal 节点）
            return None;
        }

        if level == 1 {
            // 下一跳就是 leaf（level 1 = internal parent, level 0 = leaf）
            return cache.get(child_addr);
        }

        // 内部节点：获取子节点继续下降
        current = cache.get_or_create(child_addr, level - 1);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::KeyType;
    use crate::btree::node::BsetTree;
    use crate::btree::types::NodeCache;
    use std::sync::Arc;

    // ─── Test Helpers ────────────────────────────────────────

    /// 构造 depth=0 的单 leaf btree 并插入少量数据
    fn make_single_leaf_tree() -> Btree {
        let mut b = Btree::new();
        b.insert(
            BtreeKey::new(10, 1, KeyType::Normal),
            BchVal::new(100, 0),
            0,
        );
        b.insert(
            BtreeKey::new(20, 1, KeyType::Normal),
            BchVal::new(200, 0),
            0,
        );
        b.insert(
            BtreeKey::new(30, 1, KeyType::Normal),
            BchVal::new(300, 0),
            0,
        );
        b
    }

    /// 构造 depth=1 的两层 B+tree（internal root + 2 leaves）
    ///
    /// 结构：
    /// ```text
    ///        internal root (depth=1)
    ///       /                 \
    ///   left leaf            right leaf
    ///  (10, 20, 30)         (40, 50)
    /// ```
    fn make_two_level_tree() -> Btree {
        let cache = Arc::new(NodeCache::new());

        // left leaf: keys 10, 20, 30
        let mut left = BtreeNode::new_leaf();
        left.insert(BtreeKey::new(10, 1, KeyType::Normal), BchVal::new(100, 0));
        left.insert(BtreeKey::new(20, 1, KeyType::Normal), BchVal::new(200, 0));
        left.insert(BtreeKey::new(30, 1, KeyType::Normal), BchVal::new(300, 0));
        let left = Arc::new(left);

        // right leaf: keys 40, 50
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

    // ─── depth=0 (single leaf) 测试 ──────────────────────────

    #[test]
    fn test_search_empty_tree() {
        let b = Btree::new();
        let result = b.search(&BtreeKey::new(42, 1, KeyType::Normal));
        assert!(
            result.is_none(),
            "empty tree should return None for any key"
        );
    }

    #[test]
    fn test_search_single_leaf_found() {
        let b = make_single_leaf_tree();
        let result = b.search(&BtreeKey::new(20, 1, KeyType::Normal));
        assert!(
            result.is_some(),
            "key 20 should be found in single-leaf tree"
        );
        assert_eq!(result.unwrap().1, BchVal::new(200, 0));
    }

    #[test]
    fn test_search_single_leaf_not_found() {
        let b = make_single_leaf_tree();
        let result = b.search(&BtreeKey::new(999, 1, KeyType::Normal));
        assert!(
            result.is_none(),
            "non-existing key should return None in single-leaf tree"
        );
    }

    #[test]
    fn test_search_single_leaf_all_keys() {
        let b = make_single_leaf_tree();
        for vaddr in [10u64, 20, 30] {
            let result = b.search(&BtreeKey::new(vaddr, 1, KeyType::Normal));
            assert!(result.is_some(), "key {} should be found", vaddr);
            assert_eq!(result.unwrap().0.get_vaddr(), vaddr);
        }
    }

    // ─── depth=1 (multi-level) 测试 ──────────────────────────

    #[test]
    fn test_search_multi_level_left_leaf() {
        let b = make_two_level_tree();
        // key=20 → left leaf
        let result = b.search(&BtreeKey::new(20, 1, KeyType::Normal));
        assert!(result.is_some(), "key 20 (left leaf) should be found");
        assert_eq!(result.unwrap().1, BchVal::new(200, 0));
    }

    #[test]
    fn test_search_multi_level_right_leaf() {
        let b = make_two_level_tree();
        // key=50 → right leaf
        let result = b.search(&BtreeKey::new(50, 1, KeyType::Normal));
        assert!(result.is_some(), "key 50 (right leaf) should be found");
        assert_eq!(result.unwrap().1, BchVal::new(500, 0));
    }

    #[test]
    fn test_search_multi_level_not_found() {
        let b = make_two_level_tree();
        // key=999 — 不存在
        let result = b.search(&BtreeKey::new(999, 1, KeyType::Normal));
        assert!(
            result.is_none(),
            "non-existing key should return None in multi-level tree"
        );
    }

    #[test]
    fn test_search_multi_level_boundary_key() {
        let b = make_two_level_tree();
        // 右侧 leaf 的第一个 key（边界值）
        let result = b.search(&BtreeKey::new(40, 1, KeyType::Normal));
        assert!(result.is_some(), "boundary key 40 should be found");
        assert_eq!(result.unwrap().1, BchVal::new(400, 0));
    }

    #[test]
    fn test_search_multi_level_after_insert() {
        let mut b = make_two_level_tree();

        // 插入到 left leaf
        b.insert(
            BtreeKey::new(15, 1, KeyType::Normal),
            BchVal::new(150, 0),
            0,
        );
        // 插入到 right leaf
        b.insert(
            BtreeKey::new(45, 1, KeyType::Normal),
            BchVal::new(450, 0),
            0,
        );

        let r1 = b.search(&BtreeKey::new(15, 1, KeyType::Normal));
        assert!(r1.is_some(), "key 15 after insert should be found");
        assert_eq!(r1.unwrap().1, BchVal::new(150, 0));

        let r2 = b.search(&BtreeKey::new(45, 1, KeyType::Normal));
        assert!(r2.is_some(), "key 45 after insert should be found");
        assert_eq!(r2.unwrap().1, BchVal::new(450, 0));

        // 原有 key 仍可搜索
        assert!(
            b.search(&BtreeKey::new(10, 1, KeyType::Normal)).is_some(),
            "original left leaf key 10 still findable"
        );
        assert!(
            b.search(&BtreeKey::new(50, 1, KeyType::Normal)).is_some(),
            "original right leaf key 50 still findable"
        );
    }

    // ─── 一致性验证 ──────────────────────────────────────────

    /// search() 和 get() 对于已插入的 key 应返回一致的结果
    #[test]
    fn test_search_consistent_with_get() {
        let mut b = make_single_leaf_tree();
        b.insert(
            BtreeKey::new(15, 1, KeyType::Normal),
            BchVal::new(150, 0),
            0,
        );

        let search_result = b.search(&BtreeKey::new(15, 1, KeyType::Normal));
        let get_result = b.get(&BtreeKey::new(15, 1, KeyType::Normal));
        assert_eq!(
            search_result, get_result,
            "search() and get() should return consistent results"
        );
    }

    /// search() 在 depth=0 和 depth≥1 树中对同一 key 返回一致
    #[test]
    fn test_search_consistent_across_depths() {
        // depth=0 树（单 leaf，只有 key=25）
        let mut flat = Btree::new();
        flat.insert(
            BtreeKey::new(25, 1, KeyType::Normal),
            BchVal::new(250, 0),
            0,
        );

        // depth≥1 树（通过小节点大小强制分裂）
        let mut deep = Btree::new();
        deep.set_root_node_size(256);
        // 插入足够 key 触发分裂到 depth≥1
        for i in 0..20u64 {
            deep.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            );
        }
        // 确认 deep 树已分裂（depth≥1）
        assert!(deep.depth() >= 1, "deep tree should have depth >= 1");

        // 验证 keys 0-19 只在 deep 树中存在（flat 树没有）
        for i in 0..20u64 {
            let flat_r = flat.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(flat_r.is_none(), "key {} should NOT be in flat tree", i);

            let deep_r = deep.search(&BtreeKey::new(i, 1, KeyType::Normal));
            assert!(deep_r.is_some(), "key {} should be in deep tree", i);
            assert_eq!(deep_r.unwrap().1, BchVal::new(i * 10, 0));
        }

        // 验证 key=25 只在 flat 树中存在（deep 没有）
        let flat_25 = flat.search(&BtreeKey::new(25, 1, KeyType::Normal));
        assert!(flat_25.is_some(), "key 25 should be in flat tree");
        assert_eq!(flat_25.unwrap().1, BchVal::new(250, 0));

        let deep_25 = deep.search(&BtreeKey::new(25, 1, KeyType::Normal));
        assert!(deep_25.is_none(), "key 25 should NOT be in deep tree");
    }
}
