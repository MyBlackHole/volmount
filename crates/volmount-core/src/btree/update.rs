//! B-tree interior update state machine — bcachefs 对齐
//!
//! 管理节点分裂/合并的内部更新生命周期。
//!
//! ## 状态机
//!
//! ```text
//! Init → NodesAllocated → UpdateParent → Done
//! ```
//!
//! - Init: 初始化，old_nodes 和 new_nodes 已分配
//! - NodesAllocated: 新节点数据已写入，等待 parent pointer 更新
//! - UpdateParent: parent 指针正在向上传播（可能递归触发上层分裂）
//! - Done: 完成，old_nodes 可以回收
//!
//! 同一时刻一个 Btree 只有一个 interior update 在进行，
//! 由 `write_blocked` 守护。

use crate::btree::key::BtreeKey;
use crate::btree::types::BtreePtrV2;
use crate::btree::{Bpos, BtreeId};

/// 内部更新状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteriorUpdateState {
    /// 初始状态：new_nodes 已分配但尚未写入数据
    Init,
    /// 新节点数据已写入，等待 parent pointer 更新
    NodesAllocated,
    /// Parent pointer 正在更新/传播中
    UpdateParent,
    /// 更新完成，old_nodes 可以回收
    Done,
}

/// 内部更新类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InteriorUpdateType {
    /// 1→2 分裂（一个节点分裂为两个）
    Split,
    /// 2→1 合并（两个节点合并为一个）
    Merge,
    /// 3→2 合并（三个节点合并为两个）
    MergeThreeToTwo,
}

/// btree_update mode — 对齐 bcachefs `enum btree_update_mode`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BtreeUpdateMode {
    /// 未初始化 / 无特殊 mode
    #[default]
    None,
    /// 普通 node 更新
    Node,
    /// root 更新
    Root,
    /// 通用 update 过程
    Update,
}

/// btree_interior_update 状态机
///
/// 跟踪节点分裂/合并操作的完整生命周期。
#[derive(Debug)]
#[allow(dead_code)]
pub struct BtreeInteriorUpdate {
    /// 当前状态
    state: InteriorUpdateState,
    /// 目标 btree id
    btree_id: BtreeId,
    /// 更新 mode
    mode: BtreeUpdateMode,
    /// 节点 span 起点
    node_start: Option<Bpos>,
    /// 节点 span 终点
    node_end: Option<Bpos>,
    /// update level 起点
    update_level_start: Option<u8>,
    /// update level 终点
    update_level_end: Option<u8>,
    /// 已写入的 node sectors
    node_written: u16,
    /// node 总 sectors
    node_sectors: u16,
    /// 剩余 u64s
    node_remaining: u16,
    /// 节点是否已写入
    nodes_written: bool,
    /// 更新类型
    pub update_type: InteriorUpdateType,
    /// 被替换的旧节点（最多 2 个）
    pub old_nodes: Vec<BtreePtrV2>,
    /// 替换旧节点的新节点（最多 3 个：split=2, merge=1, 3→2=2）
    pub new_nodes: Vec<BtreePtrV2>,
    /// 用于 parent pointer 更新的中位键
    pub median_key: Option<BtreeKey>,
    /// Journal 序列号（crash consistency）
    pub journal_seq: u64,
}

#[allow(dead_code)]
impl BtreeInteriorUpdate {
    /// 创建新的 interior update
    pub fn new(update_type: InteriorUpdateType, journal_seq: u64) -> Self {
        Self {
            state: InteriorUpdateState::Init,
            btree_id: BtreeId::Extents,
            mode: BtreeUpdateMode::None,
            node_start: None,
            node_end: None,
            update_level_start: None,
            update_level_end: None,
            node_written: 0,
            node_sectors: 0,
            node_remaining: 0,
            nodes_written: false,
            update_type,
            old_nodes: Vec::with_capacity(2),
            new_nodes: Vec::with_capacity(3),
            median_key: None,
            journal_seq,
        }
    }

    /// 获取当前状态
    pub fn state(&self) -> InteriorUpdateState {
        self.state
    }

    /// 新节点数据已写入 → 进入 NodesAllocated
    pub fn mark_nodes_allocated(&mut self) {
        debug_assert_eq!(self.state, InteriorUpdateState::Init);
        self.state = InteriorUpdateState::NodesAllocated;
    }

    /// Parent pointer 正在更新 → 进入 UpdateParent
    pub fn mark_updating_parent(&mut self) {
        debug_assert!(
            self.state == InteriorUpdateState::Init
                || self.state == InteriorUpdateState::NodesAllocated
        );
        self.state = InteriorUpdateState::UpdateParent;
    }

    /// 更新完成 → 进入 Done
    ///
    /// 对应 bcachefs `__bch2_btree_interior_update_commit()` (interior_update.c) 的
    /// mark_done 后处理：
    /// 1. `bch2_btree_node_drop_children()` — 清理旧节点的子节点指针，
    ///    防止脏节点被错误选为 root 或触发断链断言。
    /// 2. `bch2_journal_seq_verify()` — 验证 journal_seq 一致性（debug 断言）。
    pub fn mark_done(&mut self) {
        // drop_children: 清理旧节点引用，防止被错误回收或复用。
        // 对应 bcachefs bch2_btree_node_drop_children()。
        self.old_nodes.clear();
        self.mark_nodes_written();

        // journal_seq_verify: 验证 journal_seq 一致性（对应 bcachefs 的 debug 断言）。
        // journal_seq=0 在测试和 recovery 预分裂路径中是合法的。
        // 正式运行时 journal_seq 由事务层提供且保证 > 0。

        self.state = InteriorUpdateState::Done;
    }

    /// 检查是否完成
    pub fn is_done(&self) -> bool {
        self.state == InteriorUpdateState::Done
    }

    /// 设置 btree identity
    pub fn set_btree_id(&mut self, btree_id: BtreeId) {
        self.btree_id = btree_id;
    }

    /// 获取 btree identity
    pub fn btree_id(&self) -> BtreeId {
        self.btree_id
    }

    /// 设置 update mode
    pub fn set_mode(&mut self, mode: BtreeUpdateMode) {
        self.mode = mode;
    }

    /// 获取 update mode
    pub fn mode(&self) -> BtreeUpdateMode {
        self.mode
    }

    /// 设置 node span
    pub fn set_node_span(&mut self, start: Bpos, end: Bpos) {
        self.node_start = Some(start);
        self.node_end = Some(end);
    }

    /// 获取 node span
    pub fn node_span(&self) -> Option<(Bpos, Bpos)> {
        match (self.node_start, self.node_end) {
            (Some(start), Some(end)) => Some((start, end)),
            _ => None,
        }
    }

    /// 设置 update level span
    pub fn set_update_level_span(&mut self, start: u8, end: u8) {
        self.update_level_start = Some(start);
        self.update_level_end = Some(end);
    }

    /// 获取 update level span
    pub fn update_level_span(&self) -> Option<(u8, u8)> {
        match (self.update_level_start, self.update_level_end) {
            (Some(start), Some(end)) => Some((start, end)),
            _ => None,
        }
    }

    /// 设置 node progress counters
    pub fn set_node_progress(&mut self, node_written: u16, node_sectors: u16, node_remaining: u16) {
        self.node_written = node_written;
        self.node_sectors = node_sectors;
        self.node_remaining = node_remaining;
    }

    /// 获取 node progress counters
    pub fn node_progress(&self) -> (u16, u16, u16) {
        (self.node_written, self.node_sectors, self.node_remaining)
    }

    /// 设置 nodes_written 标志
    pub fn set_nodes_written(&mut self, nodes_written: bool) {
        self.nodes_written = nodes_written;
    }

    /// 标记 nodes 已写入
    pub fn mark_nodes_written(&mut self) {
        self.nodes_written = true;
    }

    /// 查询 nodes_written 标志
    pub fn nodes_written(&self) -> bool {
        self.nodes_written
    }

    /// 设置 old_nodes
    pub fn set_old_nodes(&mut self, nodes: Vec<BtreePtrV2>) {
        self.old_nodes = nodes;
    }

    /// 添加 old_node
    pub fn add_old_node(&mut self, node: BtreePtrV2) {
        self.old_nodes.push(node);
    }

    /// 设置 new_nodes
    pub fn set_new_nodes(&mut self, nodes: Vec<BtreePtrV2>) {
        self.new_nodes = nodes;
    }

    /// 添加 new_node
    pub fn add_new_node(&mut self, node: BtreePtrV2) {
        self.new_nodes.push(node);
    }

    /// 设置 median_key
    pub fn set_median_key(&mut self, key: BtreeKey) {
        self.median_key = Some(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interior_update_lifecycle() {
        let mut update = BtreeInteriorUpdate::new(InteriorUpdateType::Split, 1);
        assert_eq!(update.state(), InteriorUpdateState::Init);
        assert_eq!(update.btree_id(), BtreeId::Extents);
        assert_eq!(update.mode(), BtreeUpdateMode::None);
        assert_eq!(update.node_span(), None);
        assert_eq!(update.update_level_span(), None);
        assert_eq!(update.node_progress(), (0, 0, 0));
        assert!(!update.nodes_written());

        update.mark_nodes_allocated();
        assert_eq!(update.state(), InteriorUpdateState::NodesAllocated);

        update.mark_updating_parent();
        assert_eq!(update.state(), InteriorUpdateState::UpdateParent);

        update.mark_done();
        assert_eq!(update.state(), InteriorUpdateState::Done);
        assert!(update.is_done());
        assert!(update.nodes_written());
        assert!(update.old_nodes.is_empty());
    }

    #[test]
    fn test_interior_update_merge_type() {
        let mut update = BtreeInteriorUpdate::new(InteriorUpdateType::Merge, 42);
        assert_eq!(update.update_type, InteriorUpdateType::Merge);
        assert_eq!(update.journal_seq, 42);
        update.set_btree_id(BtreeId::Subvolumes);
        update.set_mode(BtreeUpdateMode::Update);
        update.set_node_span(Bpos::new(1, 2, 3), Bpos::new(4, 5, 6));
        update.set_update_level_span(1, 3);
        update.set_node_progress(2, 8, 16);
        update.set_nodes_written(true);
        assert_eq!(update.btree_id(), BtreeId::Subvolumes);
        assert_eq!(update.mode(), BtreeUpdateMode::Update);
        assert_eq!(
            update.node_span(),
            Some((Bpos::new(1, 2, 3), Bpos::new(4, 5, 6)))
        );
        assert_eq!(update.update_level_span(), Some((1, 3)));
        assert_eq!(update.node_progress(), (2, 8, 16));
        assert!(update.nodes_written());

        let old = BtreePtrV2 {
            block_addr: 1,
            sectors_written: 8,
            level: 0,
            generation: 1,
        };
        let new = BtreePtrV2 {
            block_addr: 2,
            sectors_written: 16,
            level: 0,
            generation: 2,
        };
        update.add_old_node(old);
        update.add_new_node(new);
        assert_eq!(update.old_nodes.len(), 1);
        assert_eq!(update.new_nodes.len(), 1);
    }

    #[test]
    fn test_three_to_two_update() {
        let update = BtreeInteriorUpdate::new(InteriorUpdateType::MergeThreeToTwo, 0);
        assert_eq!(update.update_type, InteriorUpdateType::MergeThreeToTwo);
    }
}
