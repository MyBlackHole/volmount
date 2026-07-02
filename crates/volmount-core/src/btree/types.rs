//! B-tree 共享类型 — bcachefs 对齐
//!
//! 包含：BtreePtrV2, BtreeNodeLockedType, BtreePathLevel, BtreeRoot, NodeCache

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::block_device::BlockDevice;
use crate::btree::cache::BtreeCache;
use crate::btree::node::BtreeNode;
use crate::btree::BtreeId;
use crate::types::StorageError;
use serde::{Deserialize, Serialize};

/// B-tree 最大深度（bcachefs: BTREE_MAX_DEPTH）
pub const BTREE_MAX_DEPTH: usize = 8;

/// depth=0 根节点的特殊 cache 地址（`alloc_addr` 从 1 开始，u64::MAX 永不冲突）
pub const ROOT_CACHE_ADDR: u64 = u64::MAX;

/// 节点持久物理指针 — 对应 bcachefs `struct bch_btree_ptr_v2`。
///
/// `sectors_written` 是节点 extent 中已提交记录的恢复边界；读取方不得
/// 采纳该边界之后的尾部。bcachefs 在节点 append 后把该字段写回 parent
/// pointer（`fs/btree/write.c:90-107, 620-622`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct BtreePtrV2 {
    /// 物理块地址（后端存储的块号）
    pub block_addr: u64,
    /// extent 中已提交的 512-byte sector 数
    pub sectors_written: u16,
    /// 节点在树中的层级（0=leaf, 越大越接近 root）
    pub level: u8,
    /// replacement extent 代号（防止地址复用导致 ABA）
    pub generation: u32,
}

impl BtreePtrV2 {
    /// 固定磁盘编码：addr(8) + sectors(2) + level(1) + reserved(1) + generation(4)。
    pub const DISK_BYTES: usize = 16;

    pub const INVALID: BtreePtrV2 = BtreePtrV2 {
        block_addr: 0,
        sectors_written: 0,
        level: 0,
        generation: 0,
    };

    pub fn is_valid(&self) -> bool {
        self.block_addr != 0 && self.sectors_written != 0
    }

    pub fn to_bytes(self) -> [u8; Self::DISK_BYTES] {
        let mut bytes = [0u8; Self::DISK_BYTES];
        bytes[0..8].copy_from_slice(&self.block_addr.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.sectors_written.to_le_bytes());
        bytes[10] = self.level;
        bytes[12..16].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.len() != Self::DISK_BYTES {
            return Err(StorageError::InvalidData(format!(
                "btree pointer must be {} bytes, got {}",
                Self::DISK_BYTES,
                bytes.len()
            )));
        }
        if bytes[11] != 0 {
            return Err(StorageError::InvalidData(
                "btree pointer reserved byte is non-zero".into(),
            ));
        }

        Ok(Self {
            block_addr: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            sectors_written: u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
            level: bytes[10],
            generation: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        })
    }
}

/// 路径层级上持有的锁状态 — 对应 bcachefs `btree_lock`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BtreeNodeLockedType {
    /// 未持有锁（路径释放或暂未获取）
    None = 0,
    /// 读锁（共享，不阻塞其他读者和 intent 者）
    Read = 1,
    /// 意向锁（intent 之间互斥，但不阻塞读）
    Intent = 2,
    /// 写锁（完全独占）
    Write = 3,
}

impl BtreeNodeLockedType {
    pub fn is_locked(&self) -> bool {
        !matches!(self, BtreeNodeLockedType::None)
    }
}

/// B-tree 路径的一个层级 — 对应 bcachefs `btree_path_level`
#[derive(Debug, Clone)]
pub struct BtreePathLevel {
    /// 当前层级的节点
    pub node: Arc<BtreeNode>,
    /// 在该节点上持有的锁
    pub lock_state: BtreeNodeLockedType,
    /// 在该节点内的当前 entry 偏移（1-indexed，0 表示未定位）
    pub offset: u16,
    /// 在父节点的 routing entries 中的索引（1-indexed；root 为 0）
    pub child_idx: u16,
    /// 加锁时记录的节点 seq（SixLock write unlock 递增）
    ///
    /// 用于 `restart_optimized()`：重启时若 `lock.seq() == locked_seq`，
    /// 说明该节点未被写操作修改，可跳过从 root 重下降。
    pub locked_seq: u64,
}

impl BtreePathLevel {
    pub fn new(node: Arc<BtreeNode>) -> Self {
        Self {
            node,
            lock_state: BtreeNodeLockedType::None,
            offset: 0,
            child_idx: 0,
            locked_seq: 0,
        }
    }
}

/// B-tree 根指针 — 对应 bcachefs `btree_root`
#[derive(Debug, Clone)]
pub struct BtreeRoot {
    /// 根节点
    pub node: Arc<BtreeNode>,
    /// 树的深度（0 = 单 leaf 节点）
    pub depth: u8,
    /// 持久化根指针（含 block_addr / sectors_written / generation）
    pub ptr: BtreePtrV2,
}

impl BtreeRoot {
    pub fn new(node: Arc<BtreeNode>, depth: u8) -> Self {
        Self {
            node,
            depth,
            ptr: BtreePtrV2::INVALID,
        }
    }

    pub fn with_ptr(node: Arc<BtreeNode>, depth: u8, ptr: BtreePtrV2) -> Self {
        Self { node, depth, ptr }
    }
}

/// 节点缓存 — bcachefs 对齐的 btree_node_cache
///
/// 基于 BtreeCache（LRU + dirty tracking + GC retire）实现。
/// 保持与 Phase 1 相同的公开 API，内部委托给 BtreeCache。
#[derive(Debug)]
pub struct NodeCache {
    /// LRU + dirty + GC 缓存
    cache: BtreeCache,
    /// 下一个可用的 block_addr（模拟分配）
    next_block: AtomicU32,
}

impl Default for NodeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeCache {
    pub fn new() -> Self {
        Self {
            cache: BtreeCache::new(),
            next_block: AtomicU32::new(1), // 0 保留
        }
    }

    /// 获取节点，若不存在则创建
    pub fn get_or_create(&self, block_addr: u64, level: u8) -> Arc<BtreeNode> {
        self.cache.get_or_load(block_addr, || {
            Arc::new(if level == 0 {
                BtreeNode::new_leaf()
            } else {
                BtreeNode::new_internal()
            })
        })
    }

    /// 根据 block_addr 查找节点
    pub fn get(&self, block_addr: u64) -> Option<Arc<BtreeNode>> {
        self.cache.get(block_addr)
    }

    /// 插入节点
    pub fn insert(&self, block_addr: u64, node: Arc<BtreeNode>) {
        self.cache.insert(block_addr, node);
    }

    /// 从 cache 中取出节点（移除引用，返回 Arc 用于写操作）
    pub fn take_node(&self, block_addr: u64) -> Option<Arc<BtreeNode>> {
        self.cache.remove(block_addr)
    }

    /// 将节点放回 cache（与 take_node 配对使用）
    pub fn put_node(&self, block_addr: u64, node: Arc<BtreeNode>) {
        self.cache.insert(block_addr, node);
    }

    /// 将已修改的节点直接插入 dirty 列表（跳过 clean）
    pub fn insert_dirty(&self, block_addr: u64, node: Arc<BtreeNode>) {
        self.cache.insert_dirty(block_addr, node);
    }

    /// bcachefs 对齐: bch2_btree_node_prefetch — 基于 block_addr 的预取
    ///
    /// 在 BtreeIter 下降路径中使用，对可能被访问的下一个兄弟节点发起异步预取。
    /// 如果节点已在缓存中，直接返回 true（无需预取）。
    /// 如果不在缓存中，分配新节点、标记 InFlight、发起异步 IO（fire-and-forget）。
    ///
    /// 参考: bcachefs-tools/fs/btree/cache.c:1575
    pub fn prefetch_node(&self, block_addr: u64, level: u8, btree_id: BtreeId) -> bool {
        self.cache
            .bch2_btree_node_prefetch_id(block_addr, level, btree_id)
    }

    /// 分配一个新的地址（不创建节点，与 take/put_node 配对使用）
    pub fn alloc_addr(&self) -> u64 {
        self.next_block.fetch_add(1, Ordering::Relaxed) as u64
    }

    /// 分配一个新的 block_addr 并创建节点
    pub fn alloc_node(&self, level: u8) -> (u64, Arc<BtreeNode>) {
        let addr = self.next_block.fetch_add(1, Ordering::Relaxed) as u64;
        let node = Arc::new(if level == 0 {
            BtreeNode::new_leaf()
        } else {
            BtreeNode::new_internal()
        });
        self.cache.insert(addr, node.clone());
        (addr, node)
    }

    /// 当前缓存中的节点数
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// 访问底层的 BtreeCache（供缓存管理功能使用）
    pub fn cache(&self) -> &BtreeCache {
        &self.cache
    }

    pub fn cache_mut(&self) -> &BtreeCache {
        &self.cache
    }

    /// 为底层缓存设置 backend。
    pub fn set_backend(&self, backend: Arc<dyn BlockDevice>) -> bool {
        self.cache.set_backend(backend)
    }

    /// 为底层缓存设置 writeback coordinator。
    pub fn set_writeback_handle(&self, writeback: Arc<crate::btree::WritebackHandle>) -> bool {
        self.cache.set_writeback_handle(writeback)
    }

    /// drain 并返回所有脏节点（按 level 升序排列，叶子先于内层节点）
    pub fn flush_dirty(&self) -> Vec<(u64, Arc<BtreeNode>)> {
        self.cache.flush_dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_ptr_invalid() {
        assert!(!BtreePtrV2::INVALID.is_valid());
    }

    #[test]
    fn test_node_ptr_valid() {
        let p = BtreePtrV2 {
            block_addr: 1,
            sectors_written: 8,
            level: 0,
            generation: 1,
        };
        assert!(p.is_valid());
    }

    #[test]
    fn test_node_ptr_disk_roundtrip() {
        let ptr = BtreePtrV2 {
            block_addr: 0x1234_5678,
            sectors_written: 24,
            level: 2,
            generation: 17,
        };
        assert_eq!(BtreePtrV2::from_bytes(&ptr.to_bytes()).unwrap(), ptr);
    }

    #[test]
    fn test_lock_state() {
        assert!(!BtreeNodeLockedType::None.is_locked());
        assert!(BtreeNodeLockedType::Read.is_locked());
        assert!(BtreeNodeLockedType::Intent.is_locked());
        assert!(BtreeNodeLockedType::Write.is_locked());
    }

    #[test]
    fn test_node_cache_alloc() {
        let cache = NodeCache::new();
        let (addr, node) = cache.alloc_node(0);
        assert_eq!(addr, 1);
        assert_eq!(node.level, 0);
        assert!(cache.get(addr).is_some());
    }

    #[test]
    fn test_node_cache_insert_get() {
        let cache = NodeCache::new();
        let node = Arc::new(BtreeNode::new_leaf());
        cache.insert(100, node.clone());
        let found = cache.get(100);
        assert!(found.is_some());
        assert!(Arc::ptr_eq(&node, &found.unwrap()));
    }

    #[test]
    fn test_node_cache_get_or_create() {
        let cache = NodeCache::new();
        let n1 = cache.get_or_create(42, 0);
        assert_eq!(n1.level, 0);
        let n2 = cache.get_or_create(42, 0);
        assert!(Arc::ptr_eq(&n1, &n2));
    }

    #[test]
    fn test_node_cache_len() {
        let cache = NodeCache::new();
        assert!(cache.is_empty());
        cache.alloc_node(0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_path_level() {
        let node = Arc::new(BtreeNode::new_leaf());
        let pl = BtreePathLevel::new(node);
        assert_eq!(pl.lock_state, BtreeNodeLockedType::None);
        assert_eq!(pl.offset, 0);
    }
}
