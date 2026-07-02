//! Btree Node Scan — bcachefs 对齐
//!
//! 对应 bcachefs btree_node_scan.c + btree_node_scan.h 中的公开 API。
//! Node scan 用于在恢复过程中扫描设备上的 btree 节点（当 btree root 丢失时）。
//!
//! bcachefs 的 node_scan 在以下场景使用：
//! - 文件系统损坏恢复：扫描所有 block 寻找 btree node header
//! - `bch2_scan_for_btree_nodes()`: 主扫描入口
//! - `bch2_found_btree_node_to_text()`: 将扫描到的节点转为文本
//!
//! volmount 当前实现为最小骨架。

// ─── Types ──────────────────────────────────────────────────────────────

/// bcachefs 对齐: struct found_btree_node — 扫描到的 btree 节点信息
#[derive(Debug, Clone)]
pub struct FoundBtreeNode {
    pub btree_id: u8,
    pub level: u8,
    pub sectors_written: u32,
    pub seq: u32,
    pub journal_seq: u64,
    pub min_key: u64,
    pub max_key: u64,
    pub nr_ptrs: u8,
    pub ptrs: Vec<u64>,
}

/// bcachefs 对齐: struct find_btree_nodes — 扫描结果集合
#[derive(Debug)]
pub struct FindBtreeNodes {
    pub ret: i32,
    pub nodes: Vec<FoundBtreeNode>,
}

impl FindBtreeNodes {
    pub fn new() -> Self {
        Self {
            ret: 0,
            nodes: Vec::new(),
        }
    }
}

impl Default for FindBtreeNodes {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Public API ─────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_scan_for_btree_nodes — 扫描设备上的 btree 节点
///
/// 遍历设备的每个 block，检查是否存在 btree node header。
/// 返回找到的节点列表。
pub fn bch2_scan_for_btree_nodes(_result: &mut FindBtreeNodes) -> i32 {
    // volmount: 骨架实现 — 实际扫描需要访问 backend
    0
}

/// bcachefs 对齐: bch2_found_btree_node_to_text — 将扫描节点信息格式化为文本
pub fn bch2_found_btree_node_to_text(node: &FoundBtreeNode) -> String {
    format!(
        "FoundBtreeNode(btree_id={}, level={}, seq={}, journal_seq={}, min_key={}, max_key={}, sectors={}, nr_ptrs={})",
        node.btree_id, node.level, node.seq, node.journal_seq,
        node.min_key, node.max_key, node.sectors_written, node.nr_ptrs
    )
}

/// bcachefs 对齐: bch2_btree_node_is_stale — 检查节点是否过时
///
/// 比较节点的 seq 和当前已知的 seq，判断该节点是否已被覆盖。
pub fn bch2_btree_node_is_stale(_node: &FoundBtreeNode) -> bool {
    // volmount: 骨架实现
    false
}

/// bcachefs 对齐: bch2_btree_has_scanned_nodes — 检查是否有已扫描的节点
///
/// 检查指定 btree_id 是否存在扫描得到的节点。
pub fn bch2_btree_has_scanned_nodes(_result: &FindBtreeNodes, _btree_id: u32) -> bool {
    // volmount: 骨架实现
    false
}

/// bcachefs 对齐: bch2_get_scanned_nodes — 获取已扫描的节点列表
///
/// 按范围和数量限制返回扫描到的节点。
pub fn bch2_get_scanned_nodes(
    result: &FindBtreeNodes,
    _btree_id: u32,
    _level: u16,
    _min_pos: u64,
    _max_pos: u64,
    _max_nodes: usize,
) -> Vec<FoundBtreeNode> {
    result.nodes.clone()
}

/// bcachefs 对齐: bch2_find_btree_nodes_init — 初始化扫描结果容器
pub fn bch2_find_btree_nodes_init(result: &mut FindBtreeNodes) {
    result.ret = 0;
    result.nodes.clear();
}

/// bcachefs 对齐: bch2_find_btree_nodes_exit — 释放扫描结果容器
pub fn bch2_find_btree_nodes_exit(result: &mut FindBtreeNodes) {
    result.nodes.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_found_btree_node_to_text() {
        let node = FoundBtreeNode {
            btree_id: 0,
            level: 1,
            sectors_written: 8,
            seq: 42,
            journal_seq: 100,
            min_key: 0,
            max_key: 1000,
            nr_ptrs: 2,
            ptrs: vec![100, 200],
        };
        let text = bch2_found_btree_node_to_text(&node);
        assert!(text.contains("btree_id=0"));
        assert!(text.contains("level=1"));
        assert!(text.contains("seq=42"));
    }

    #[test]
    fn test_find_nodes_init_exit() {
        let mut result = FindBtreeNodes::new();
        result.nodes.push(FoundBtreeNode {
            btree_id: 1,
            level: 0,
            sectors_written: 4,
            seq: 1,
            journal_seq: 10,
            min_key: 0,
            max_key: 500,
            nr_ptrs: 1,
            ptrs: vec![50],
        });
        assert_eq!(result.nodes.len(), 1);

        bch2_find_btree_nodes_exit(&mut result);
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn test_get_scanned_nodes() {
        let mut result = FindBtreeNodes::new();
        result.nodes.push(FoundBtreeNode {
            btree_id: 1,
            level: 0,
            sectors_written: 4,
            seq: 1,
            journal_seq: 10,
            min_key: 0,
            max_key: 500,
            nr_ptrs: 1,
            ptrs: vec![50],
        });
        let nodes = bch2_get_scanned_nodes(&result, 1, 0, 0, 1000, 10);
        assert_eq!(nodes.len(), 1);
    }
}
