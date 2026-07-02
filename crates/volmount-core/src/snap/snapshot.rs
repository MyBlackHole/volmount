//! Snapshot btree 操作 — bcachefs 对齐的 Snapshots btree 原生实现
//!
//! 所有快照操作直接读写 Snapshots btree，无需独立内存缓存。
//!
//! # 架构
//!
//! - `bch2_snapshot_is_ancestor_btree()`: 使用 SnapshotT.skip[3] 持久化 skiplist
//!   直接从 Snapshots btree 查询祖先关系（O(log depth)）
//! - `bch2_snapshot_node_create()`: 在 Snapshots btree 中插入新快照节点
//! - `bch2_snapshot_node_set_deleted()`: 标记快照为已删除
//! - `list_snapshots_from_btree()`: 遍历 Snapshots btree 列出快照
//!
//! 命名对齐 bcachefs：
//! - `bch_snapshot.skip[]` → SnapshotT.skip
//! - `bch2_snapshot_is_ancestor()` → bch2_snapshot_is_ancestor_btree()
//! - `bch2_snapshot_node_create()` → bch2_snapshot_node_create()

use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
use crate::btree::{BatchEntry, BtreeEngine, BtreeId};
use crate::types::StorageError;
use rand::Rng;
use std::collections::HashSet;

use super::meta::{BchSnapshotFlags, SnapshotIdState, SnapshotT, SnapshotTreeT};
use super::table::SnapshotTable;

/// 从 Snapshots btree 读取 SnapshotT
///
/// 对齐 bcachefs `bch2_snapshot_lookup()` 但简化（跳过事务层）。
/// 注意：对写入 Whiteout 的已删除节点返回 None。
pub fn read_snapshot_value(engine: &BtreeEngine, id: u32) -> Option<SnapshotT> {
    let entry = engine.get_entry_raw(BtreeId::Snapshots, Bpos::new(0, 0, id))?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => return None,
    };
    bincode::deserialize(bytes).ok()
}

/// 从 Snapshots btree 读取 SnapshotT（允许已删除节点）。
///
/// 与 `read_snapshot_value` 不同，此函数能读取被 Whiteout 覆盖的快照节点，
/// 因为它的 parent 和 skip 信息在祖先链遍历中仍然必要。
/// 用于 `is_ancestor_from_btree` 中的已删除节点遍历。
pub fn read_snapshot_value_allow_deleted(engine: &BtreeEngine, id: u32) -> Option<SnapshotT> {
    let btree = engine.get(BtreeId::Snapshots);
    let entry = btree.get_entry_allow_whiteout(Bpos::new(0, 0, id))?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => return None,
    };
    bincode::deserialize(bytes).ok()
}

// 别名：为外部模块（volume、subvol）提供旧名称兼容
pub use read_snapshot_value as bch2_snapshot_lookup;

/// 从 Snapshots btree 批量读取多个快照。
///
/// 通过遍历 btree 收集所有未删除的快照，使用 HashMap 去重（处理 Whiteout 覆盖）。
pub fn list_snapshots_from_btree(engine: &BtreeEngine) -> Vec<(u32, SnapshotT)> {
    use std::collections::HashMap;
    let mut map: HashMap<u32, SnapshotT> = HashMap::new();
    let btree = engine.get(BtreeId::Snapshots);
    btree.for_each_entry(|entry| {
        let sid = entry.pos.snapshot;
        let bytes = match &entry.value {
            KeyValue::Raw(b) => b.clone(),
            _ => return,
        };
        if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
            // 先全部插入：后插入的 Whiteout（deleted=true）覆盖前一个 Normal 条目
            map.insert(sid, snap);
        }
    });
    // 过滤掉已删除的快照（Whiteout 覆盖后的 deleted=true 条目）
    map.retain(|_, snap| !snap.deleted);
    // 按 snapshot_id 降序排列（父优先）
    let mut result: Vec<(u32, SnapshotT)> = map.into_iter().collect();
    result.sort_by(|a, b| b.0.cmp(&a.0));
    result
}

/// 检查 `ancestor` 是否为 `descendant` 的祖先（btree 版本）。
///
/// 直接从 Snapshots btree 读取 SnapshotT，使用持久化 skip list（skip[3]）
/// 实现 O(log depth) 跳跃遍历。不对齐 bcachefs 的 bitmap O(1) 优化。
///
/// # 算法
///
/// 从 `descendant` 向上遍历：
/// 1. 读取当前节点的 SnapshotT（从 Snapshots btree）
/// 2. 优先尝试 skip[2]（最远祖先）→ skip[1] → skip[0] → parent 跳跃
/// 3. 每个 skip 必须满足 `skip > current && skip <= ancestor` 才安全（不跳过目标）
/// 4. 命中 ancestor 返回 true，遇到 parent=0（根）返回 false
///
/// 由于 parent_id > child_id（ID 从 u32::MAX 向下分配），skip 中的 ID 也大于 current。
pub fn is_ancestor_from_btree(engine: &BtreeEngine, ancestor: u32, descendant: u32) -> bool {
    if ancestor == descendant {
        return true;
    }
    // 父 ID > 子 ID，所以 ancestor 必须大于 descendant
    if ancestor <= descendant || descendant == 0 {
        return false;
    }

    let mut current = descendant;
    loop {
        let snap = match read_snapshot_value(engine, current) {
            Some(s) => s,
            None => {
                // 节点可能被 set_deleted 写入 Whiteout 覆盖，但仍保留 parent/skip 信息
                // 尝试读取已删除节点的数据以继续祖先遍历
                match read_snapshot_value_allow_deleted(engine, current) {
                    Some(s) => s,
                    None => return false,
                }
            }
        };

        // 已到达根 → 不再有祖先
        // 注意：snap.deleted 不阻断遍历——已删除节点仍在快照树中，
        // 其祖先链对如 bch2_delete_dead_snapshots 等操作仍然必要。
        if snap.parent == 0 {
            return false;
        }

        // Skiplist 跳跃（指数级）：优先尝试 skip[2]（最远，4 步）
        // 使用 bch2_snapshot_skiplist_good 做健壮性检查 + 递归回退
        if bch2_snapshot_skiplist_good(snap.skip[2], current, ancestor) {
            if snap.skip[2] == ancestor {
                return true;
            }
            current = snap.skip[2];
            continue;
        }
        if bch2_snapshot_skiplist_good(snap.skip[1], current, ancestor) {
            if snap.skip[1] == ancestor {
                return true;
            }
            current = snap.skip[1];
            continue;
        }
        if bch2_snapshot_skiplist_good(snap.skip[0], current, ancestor) {
            if snap.skip[0] == ancestor {
                return true;
            }
            current = snap.skip[0];
            continue;
        }

        // 位图检查：当 ancestor 在当前节点的 128 位 bitmap 范围内时，
        // 使用 O(1) 位图判定替代线性父链遍历。
        // 对齐 bcachefs `__bch2_snapshot_is_ancestor()` 的 bitmap phase：
        //   while (id && id < ancestor - IS_ANCESTOR_BITMAP) → skiplist
        //   → test_ancestor_bitmap(t, id, ancestor) → O(1)
        let dist = ancestor - current;
        if dist < 128 {
            return (snap.is_ancestor >> (dist - 1)) & 1 == 1;
        }

        // 线性回退：检查 parent
        if snap.parent == ancestor {
            return true;
        }
        current = snap.parent;
    }
}

/// bch2_snapshot_is_ancestor_btree — bcachefs 对齐的 btree 版本祖先检查。
///
/// 参数顺序对齐 bcachefs：`(trans, id, ancestor)` 中的 `(engine, id, ancestor)`。
/// 此处 id = descendant, ancestor = 潜在的祖先。
/// 对齐 bcachefs `bch2_snapshot_is_ancestor()`。
pub fn bch2_snapshot_is_ancestor_btree(
    engine: &BtreeEngine,
    descendant: u32,
    ancestor: u32,
) -> bool {
    if descendant == ancestor {
        return true;
    }
    if ancestor <= descendant || descendant == 0 {
        return false;
    }
    // 走 btree 版本的 skip list 路径
    is_ancestor_from_btree(engine, ancestor, descendant)
}

/// 从子卷可见性角度检查 ancestor 是否对 descendant 可见。
///
/// 除了直接祖先关系外，还通过 descendant 子树中叶节点的 subvol 做间接判定。
/// 适用于 interior 节点（无直接 subvol）的可见性判断。
///
/// 算法：
/// 1. 正常祖先检查通过 → 返回 true
/// 2. 不通过时，遍历 descendant 子树的叶节点（subvol != 0 的节点）
/// 3. 如果叶节点的 ancestors 包含目标 ancestor，视为可见
///
/// 参数：
/// - `engine`: BtreeEngine
/// - `descendant`: 待检快照
/// - `ancestor`: 潜在的祖先
/// - `subvol`: descendant 关联的子卷（可能为 0）
pub fn bch2_snapshot_is_ancestor_subvol(
    engine: &BtreeEngine,
    descendant: u32,
    ancestor: u32,
    subvol: u32,
) -> bool {
    // 1. 先检查正常祖先关系
    if bch2_snapshot_is_ancestor_btree(engine, descendant, ancestor) {
        return true;
    }

    // 2. descendant 存在直接 subvol 引用且匹配 → 可见
    if subvol != 0 {
        if let Some(snap) = read_snapshot_value(engine, descendant) {
            if snap.subvol == subvol {
                // 自身持有 subvol，检查 ancestor 是否也是该 subvol 链上的祖先
                return bch2_snapshot_is_ancestor_btree(engine, snap.parent, ancestor);
            }
        }
    }

    // 3. 遍历 descendant 子树的叶节点（深度优先），检查叶节点的 ancestors
    let mut check_stack = vec![descendant];
    while let Some(id) = check_stack.pop() {
        let snap = match read_snapshot_value(engine, id) {
            Some(s) => s,
            None => continue,
        };
        // 叶节点（有 subvol）— 检查其祖先链是否包含 ancestor
        if snap.subvol != 0 {
            if is_ancestor_from_btree(engine, ancestor, id) {
                return true;
            }
        }
        // 非叶节点：将子节点入栈继续遍历
        if snap.children[0] != 0 {
            check_stack.push(snap.children[0]);
        }
        if snap.children[1] != 0 {
            check_stack.push(snap.children[1]);
        }
    }

    false
}

/// 基于 bitmap 的快照 ID 分配器。
///
/// 管理一组可回收的快照 ID，避免每次从 u32::MAX 递减分配。
/// 用于替代裸 `get_next_snapshot_id`，支持 ID 回收。
///
/// 当 bitmap 空间不足时回退到 `get_next_snapshot_id`。
pub struct SnapshotIdBitmap {
    /// bitmap 数据，每个 bit 代表一个 ID（0=空闲, 1=已分配）
    bits: Vec<u64>,
    /// bitmap 管理的起始 ID（从 u32::MAX 往下计）
    start_id: u32,
    /// bitmap 管理的 ID 数量
    capacity: u32,
    /// 当前已用 ID 数量
    used: u32,
}

impl SnapshotIdBitmap {
    /// 创建一个管理 `capacity` 个 ID 的 bitmap。
    ///
    /// `start_id` 是 bitmap 上界（从该值往下分配）。
    pub fn new(start_id: u32, capacity: u32) -> Self {
        let word_count = ((capacity + 63) / 64) as usize;
        Self {
            bits: vec![0u64; word_count],
            start_id,
            capacity,
            used: 0,
        }
    }

    /// 分配一个新 ID。
    ///
    /// 1. 在 bitmap 中找第一个 free bit
    /// 2. 有 → 标记为 used，返回对应 ID
    /// 3. 无 → 返回 None（调用者应回退到 get_next_snapshot_id）
    pub fn alloc(&mut self) -> Option<u32> {
        if self.used >= self.capacity {
            return None;
        }
        for (word_idx, word) in self.bits.iter_mut().enumerate() {
            if *word != u64::MAX {
                // 找第一个 0 bit
                let bit_pos = (!*word).trailing_zeros();
                *word |= 1u64 << bit_pos;
                self.used += 1;
                let id = self.start_id - (word_idx as u32 * 64 + bit_pos);
                return Some(id);
            }
        }
        None
    }

    /// 释放一个已分配的 ID，使其可被回收重用。
    ///
    /// 如果 ID 在 bitmap 管理范围内，清除对应 bit。
    pub fn free(&mut self, id: u32) {
        if id > self.start_id || self.start_id - id >= self.capacity {
            return; // 不在管理范围内
        }
        let offset = self.start_id - id;
        let word_idx = (offset / 64) as usize;
        let bit_pos = offset % 64;
        if word_idx < self.bits.len() {
            let mask = 1u64 << bit_pos;
            if self.bits[word_idx] & mask != 0 {
                self.bits[word_idx] &= !mask;
                self.used -= 1;
            }
        }
    }
}

/// 从 Snapshots btree 获取下一个可用的快照 ID。
///
/// ID 从 u32::MAX 向下分配（父 > 子，对齐 bcachefs 但方向相反）。
/// 扫描 btree 中所有 entry（含已删除）找到最小 ID，然后减 1。
/// 空 btree 返回 u32::MAX。
pub fn get_next_snapshot_id(engine: &BtreeEngine) -> u32 {
    let btree = engine.get(BtreeId::Snapshots);
    let mut min_id = u32::MAX;
    let mut has_entries = false;
    btree.for_each_entry(|entry| {
        let sid = entry.pos.snapshot;
        if sid < min_id {
            min_id = sid;
        }
        has_entries = true;
    });
    if !has_entries {
        u32::MAX // 空 btree，从最大值开始
    } else {
        min_id.wrapping_sub(1) // 下一个 ID = 最小 ID - 1
    }
}

/// 批量重建所有快照的指数级 skip list。
///
/// 遍历 Snapshots btree，为每个快照节点使用 `bch2_snapshot_skiplist_get` 计算
/// bcachefs 对齐的指数级 skip（Batch B: skip[2] = skip[1].skip[1] 实现 4 步跳），然后写回。
fn build_skip_list_from_btree(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    let snap_ids: Vec<u32> = {
        let btree = engine.get(BtreeId::Snapshots);
        let mut ids = Vec::new();
        btree.for_each_entry(|entry| {
            ids.push(entry.pos.snapshot);
        });
        ids
    };

    for &id in &snap_ids {
        if let Some(skiplist) = bch2_snapshot_skiplist_get(engine, id) {
            let mut snap = match read_snapshot_value(engine, id) {
                Some(s) => s,
                None => continue,
            };
            snap.skip = skiplist;
            let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
    }

    Ok(())
}

/// 检查 skip 条目对 ancestor 跳跃是否"良好"。
///
/// skip 合法的条件：
/// 1. skip != 0（有效值）
/// 2. skip <= ancestor（不跳过目标）
/// 3. skip > current（向前跳跃）
///
/// 当条件不满足时返回上一个有效的 skip 索引或 parent。
/// 用于 `is_ancestor_from_btree` 中的健壮跳跃。
pub fn bch2_snapshot_skiplist_good(skip: u32, current: u32, ancestor: u32) -> bool {
    skip != 0 && skip <= ancestor && skip > current
}

/// 从 `id` 出发向上走 `n` 步父链。
///
/// 对齐 bcachefs `bch2_snapshot_nth_parent()`（snapshot.h:106-114）。
/// bcachefs 版本使用 O(1) 内存表遍历，此 btree 版本为 O(n) 次 btree 读取，
/// 仅在创建快照节点和重建 skiplist 时调用，对性能影响可忽略。
fn bch2_snapshot_nth_parent_btree(engine: &BtreeEngine, mut id: u32, mut n: u32) -> Option<u32> {
    while n > 0 {
        let snap = read_snapshot_value_allow_deleted(engine, id)?;
        if snap.parent == 0 {
            return None;
        }
        id = snap.parent;
        n -= 1;
    }
    Some(id)
}

/// bcachefs 对齐的快照 skiplist 获取。返回 3 个随机深度跳跃祖先（对齐 bcachefs `bch2_snapshot_skiplist_get`）。
///
/// 对齐 bcachefs 算法（check_snapshots.c:211-221）：
///   1. 检查 `id` 是否为有效快照
///   2. 如果有效且有 parent，生成 `get_random_u32_below(s->depth)` 随机步数
///   3. 调用 `bch2_snapshot_nth_parent()` 向上跳跃
///   4. 重复 3 次并对结果排序（bubble_sort）
///
/// vs 旧的指数步进（1,2,4）：随机分布避免了跳跃簇集，
/// 在深度大的快照树中提供更均匀的祖先覆盖。
pub fn bch2_snapshot_skiplist_get(engine: &BtreeEngine, id: u32) -> Option<[u32; 3]> {
    let snap = read_snapshot_value(engine, id)?;

    if snap.parent == 0 {
        return Some([0, 0, 0]);
    }

    let depth = snap.depth;
    let mut rng = rand::thread_rng();
    let mut skiplist = [0u32; 3];

    for j in 0..3 {
        let n = rng.gen_range(0..depth) as u32;
        skiplist[j] = bch2_snapshot_nth_parent_btree(engine, id, n)?;
    }

    skiplist.sort_unstable();
    Some(skiplist)
}

/// 在 Snapshots btree 中创建根快照节点。
///
/// 根快照的 parent=0、depth=1、skip=[0,0,0]。
/// 返回分配的 snapshot ID（初始为 u32::MAX）。
pub fn create_root_snapshot_btree(
    engine: &mut BtreeEngine,
    subvol: u32,
) -> Result<u32, StorageError> {
    let id = get_next_snapshot_id(engine);
    let snap_val = SnapshotT::new_leaf(0, subvol, 0, 1, current_timestamp());
    let bytes = bincode::serialize(&snap_val).map_err(StorageError::Serialization)?;
    let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
    engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
    Ok(id)
}

/// 在 Snapshots btree 中创建子快照节点。
///
/// 对齐 bcachefs `bch2_snapshot_node_create()`。
/// 1. 从 parent_id 读取父节点，获取 depth
/// 2. 计算 depth = parent.depth + 1
/// 3. 构建 skip list（基于 depth 比例步进）
/// 4. 序列化并插入 Snapshots btree
/// 5. 更新父节点的 children 列表
///
/// 当 `extra_child_subvol` 为 `Some(src_subvol)` 时（快照的 bcachefs "1变2" 语义）：
///   同时分配两个子节点 ID（一个给新子卷，一个给源子卷），
///   一次原子 batch_write 写入两个子节点 + 父节点更新。
/// 当 `extra_child_subvol` 为 `None` 时，保持原有的单子节点行为。
///
/// 返回新分配的 snapshot ID（第一个子节点）。
pub fn bch2_snapshot_node_create(
    engine: &mut BtreeEngine,
    parent_id: u32,
    subvol: u32,
    extra_child_subvol: Option<u32>,
) -> Result<u32, StorageError> {
    let parent = read_snapshot_value(engine, parent_id).ok_or_else(|| {
        StorageError::NotFound(format!("parent snapshot {} not found", parent_id))
    })?;

    let depth = parent.depth + 1;
    let id = get_next_snapshot_id(engine);
    // 随机深度跳跃 skiplist（对齐 bcachefs `bch2_snapshot_node_create`）：
    // for j in 0..3: skip[j] = bch2_snapshot_skiplist_get(c, parent)
    // bubble_sort(skip) — 3 次随机 get_random_u32_below(depth) + nth_parent
    let skip = bch2_snapshot_skiplist_get(engine, parent_id)
        .map(|mut s| {
            s.sort_unstable();
            s
        })
        .unwrap_or([0, 0, 0]);

    // 更新父节点的 children 列表
    // 父节点从 leaf 变为 interior：清除 subvol 和 SUBVOL 标志（对齐 bcachefs create_snapids）
    let mut new_parent = parent.clone();
    new_parent.subvol = 0;
    new_parent.flags.remove(BchSnapshotFlags::SUBVOL);

    match extra_child_subvol {
        Some(extra_subvol) => {
            // bcachefs "1变2" 语义：同时创建两个子节点
            // 注意：不能用 get_next_snapshot_id(engine) 获取第二个 ID，
            // 因为 id 尚未插入 btree，两次调用会返回相同的值。
            // 快照 ID 从 u32::MAX 向下分配，所以 extra_id = id - 1。
            let extra_id = id.wrapping_sub(1);

            new_parent.children = [id, extra_id];

            // 计算两个子节点的 is_ancestor 位图：
            // child.bitmap = parent.bitmap << gap | 1 << (gap - 1)
            // 对齐 bcachefs `__bch2_snapshot_is_ancestor()` 的 bitmap 构建。
            let gap = parent_id - id;
            let extra_gap = parent_id - extra_id;
            let child_bitmap = if parent_id == 0 {
                0
            } else if gap >= 128 {
                0
            } else {
                parent.is_ancestor.wrapping_shl(gap) | (1u128 << (gap - 1))
            };
            let extra_child_bitmap = if parent_id == 0 {
                0
            } else if extra_gap >= 128 {
                0
            } else {
                parent.is_ancestor.wrapping_shl(extra_gap) | (1u128 << (extra_gap - 1))
            };

            let snap_val = SnapshotT {
                state: super::meta::SnapshotIdState::Live,
                parent: parent_id,
                children: [0, 0],
                subvol,
                tree: parent.tree,
                skip,
                is_ancestor: child_bitmap,
                depth,
                btime: current_timestamp(),
                deleted: false,
                flags: BchSnapshotFlags::SUBVOL,
            };

            let extra_snap_val = SnapshotT {
                state: super::meta::SnapshotIdState::Live,
                parent: parent_id,
                children: [0, 0],
                subvol: extra_subvol,
                tree: parent.tree,
                skip,
                is_ancestor: extra_child_bitmap,
                depth,
                btime: current_timestamp(),
                deleted: false,
                flags: BchSnapshotFlags::SUBVOL,
            };

            let bytes = bincode::serialize(&snap_val).map_err(StorageError::Serialization)?;
            let extra_bytes =
                bincode::serialize(&extra_snap_val).map_err(StorageError::Serialization)?;
            let parent_bytes =
                bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;

            let entries = [
                (
                    BatchEntry::Insert {
                        pos: Bpos::new(0, 0, id),
                        data: bytes,
                    },
                    0,
                ),
                (
                    BatchEntry::Insert {
                        pos: Bpos::new(0, 0, extra_id),
                        data: extra_bytes,
                    },
                    0,
                ),
                (
                    BatchEntry::Insert {
                        pos: Bpos::new(0, 0, parent_id),
                        data: parent_bytes,
                    },
                    0,
                ),
            ];
            if !engine.batch_write(BtreeId::Snapshots, &entries) {
                return Err(StorageError::Transaction(
                    "batch_write failed in bch2_snapshot_node_create".into(),
                ));
            }

            engine.get_mut(BtreeId::Snapshots).compact();

            Ok(id)
        }
        None => {
            new_parent.children[0] = id;

            let child_bitmap = if parent_id == 0 {
                0
            } else {
                let gap = parent_id - id;
                if gap >= 128 {
                    0
                } else {
                    parent.is_ancestor.wrapping_shl(gap) | (1u128 << (gap - 1))
                }
            };

            let snap_val = SnapshotT {
                state: super::meta::SnapshotIdState::Live,
                parent: parent_id,
                children: [0, 0],
                subvol,
                tree: parent.tree,
                skip,
                is_ancestor: child_bitmap,
                depth,
                btime: current_timestamp(),
                deleted: false,
                flags: BchSnapshotFlags::SUBVOL,
            };

            let bytes = bincode::serialize(&snap_val).map_err(StorageError::Serialization)?;
            let parent_bytes =
                bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;

            let entries = [
                (
                    BatchEntry::Insert {
                        pos: Bpos::new(0, 0, id),
                        data: bytes,
                    },
                    0,
                ),
                (
                    BatchEntry::Insert {
                        pos: Bpos::new(0, 0, parent_id),
                        data: parent_bytes,
                    },
                    0,
                ),
            ];
            if !engine.batch_write(BtreeId::Snapshots, &entries) {
                return Err(StorageError::Transaction(
                    "batch_write failed in bch2_snapshot_node_create".into(),
                ));
            }

            engine.get_mut(BtreeId::Snapshots).compact();

            Ok(id)
        }
    }
}

/// 在 Snapshots btree 中标记快照为已删除。
///
/// 对齐 bcachefs `bch2_snapshot_node_set_deleted()`。
/// 对应 bcachefs 的 WILL_DELETE 标记 + deleted 标志。
/// 真正的垃圾回收（清理数据键）需要 Journal replay（Δ6）支持。
pub fn bch2_snapshot_node_set_deleted(
    engine: &mut BtreeEngine,
    id: u32,
) -> Result<(), StorageError> {
    let mut snap = read_snapshot_value(engine, id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", id)))?;

    snap.mark_deleted();
    snap.flags.insert(BchSnapshotFlags::WILL_DELETE);

    let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
    let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Whiteout, bytes);
    engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);

    Ok(())
}

/// 返回当前 Unix 时间戳（秒）
fn current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 读取 SnapshotTreeT 从 SnapshotTrees btree。
///
/// 对齐 bcachefs `bch2_snapshot_tree_lookup()`。
pub fn read_snapshot_tree_value(engine: &BtreeEngine, tree_id: u32) -> Option<SnapshotTreeT> {
    let entry = engine.get_entry_raw(BtreeId::SnapshotTrees, Bpos::new(0, 0, tree_id))?;
    let bytes = match &entry.value {
        KeyValue::Raw(b) => b,
        _ => return None,
    };
    bincode::deserialize(bytes).ok()
}

/// 在 SnapshotTrees btree 中写入 SnapshotTreeT。
pub fn write_snapshot_tree_value(
    engine: &mut BtreeEngine,
    tree_id: u32,
    tree_val: &SnapshotTreeT,
) -> Result<(), StorageError> {
    let bytes = bincode::serialize(tree_val).map_err(StorageError::Serialization)?;
    let entry = BtreeEntry::raw(Bpos::new(0, 0, tree_id), KeyType::Normal, bytes);
    engine.insert_entry_raw(BtreeId::SnapshotTrees, entry, 0);
    Ok(())
}

/// 更新快照树的 master_subvol（级联主卷管理）。
///
/// 对齐 bcachefs `bch2_snapshot_tree_set_master_subvol()`。
///
/// 当子卷被快照后，新子卷成为该快照树的主卷。
/// 此函数更新 SnapshotTrees btree 中的 master_subvol 字段。
pub fn bch2_snapshot_tree_master_subvol(
    engine: &mut BtreeEngine,
    tree_id: u32,
    new_master: u32,
) -> Result<(), StorageError> {
    let mut tree_val = read_snapshot_tree_value(engine, tree_id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot tree {} not found", tree_id)))?;
    tree_val.master_subvol = new_master;
    write_snapshot_tree_value(engine, tree_id, &tree_val)
}

/// 快照树子树注册表（白名单）。
///
/// 管理 snapshot_tree 之间的子树关系，用于操作权限验证。
/// 当一个快照树被创建为另一个树的子分支时需要注册。
///
/// 使用 `std::collections::HashSet` 存储 (parent, child) 对，O(1) 查表。
#[derive(Debug, Clone, Default)]
pub struct SubtreeRegistry {
    /// (parent_tree_id, child_tree_id) 对
    pairs: HashSet<(u32, u32)>,
}

impl SubtreeRegistry {
    /// 创建一个新的空注册表。
    pub fn new() -> Self {
        Self {
            pairs: HashSet::new(),
        }
    }

    /// 注册 parent_tree 下的 child_tree 子树。
    ///
    /// 当从 parent_tree 创建新的快照树时调用。
    pub fn register(&mut self, parent_tree: u32, child_tree: u32) {
        self.pairs.insert((parent_tree, child_tree));
    }

    /// 取消注册子树关系。
    ///
    /// 当子树被删除时调用。
    pub fn unregister(&mut self, parent_tree: u32, child_tree: u32) {
        self.pairs.remove(&(parent_tree, child_tree));
    }

    /// 检查 candidate 是否在 tree_id 的白名单中。
    ///
    /// 递归验证：不仅检查直接注册的子树，还沿注册链向上追溯。
    /// 即如果 B 注册在 A 下，C 注册在 B 下，则 C 对 A 可见。
    pub fn is_whitelisted(&self, tree_id: u32, candidate: u32) -> bool {
        if tree_id == candidate {
            return true;
        }
        // 直接子节点检查
        if self.pairs.contains(&(tree_id, candidate)) {
            return true;
        }
        // 递归子节点检查：candidate 是否在 tree_id 的任何下级子树中
        for &(parent, child) in &self.pairs {
            if parent == tree_id && self.is_whitelisted(child, candidate) {
                return true;
            }
        }
        false
    }

    /// 获取 tree_id 的直接子节点列表。
    pub fn children_of(&self, tree_id: u32) -> Vec<u32> {
        self.pairs
            .iter()
            .filter(|&&(parent, _)| parent == tree_id)
            .map(|&(_, child)| child)
            .collect()
    }
}

// ─── 深度优先遍历 ───────────────────────────────

/// 返回 subtree 所有节点的后序列表（叶子→根）。
///
/// 遍历顺序：children[0]（左）→ children[1]（右）→ self
/// 包含 snapshot_id 自身。
pub fn dfs_descendants(engine: &BtreeEngine, snapshot_id: u32) -> Vec<u32> {
    let mut result = Vec::new();
    dfs_descendants_inner(engine, snapshot_id, &mut result);
    result
}

fn dfs_descendants_inner(engine: &BtreeEngine, id: u32, result: &mut Vec<u32>) {
    let snap = match read_snapshot_value(engine, id) {
        Some(s) => s,
        None => return,
    };
    // 二叉树 children[0]=left, children[1]=right
    if snap.children[0] != 0 {
        dfs_descendants_inner(engine, snap.children[0], result);
    }
    if snap.children[1] != 0 {
        dfs_descendants_inner(engine, snap.children[1], result);
    }
    result.push(id);
}

/// 返回 subtree 中所有未删除（!deleted）节点的后序列表。
///
/// 遍历子树时跳过已删除节点自身，但不跳过已删除节点的子节点
///（子节点可能仍然存活）。
pub fn dfs_descendants_alive(engine: &BtreeEngine, snapshot_id: u32) -> Vec<u32> {
    let mut result = Vec::new();
    dfs_descendants_alive_inner(engine, snapshot_id, &mut result);
    result
}

fn dfs_descendants_alive_inner(engine: &BtreeEngine, id: u32, result: &mut Vec<u32>) {
    let snap = match read_snapshot_value(engine, id) {
        Some(s) => s,
        None => return,
    };
    if snap.children[0] != 0 {
        dfs_descendants_alive_inner(engine, snap.children[0], result);
    }
    if snap.children[1] != 0 {
        dfs_descendants_alive_inner(engine, snap.children[1], result);
    }
    if !snap.deleted {
        result.push(id);
    }
}

/// 物理删除快照节点。
///
/// 对齐 bcachefs `bch2_snapshot_node_delete()` (delete.c:167-290)。
///
/// 功能：
/// - 更新父节点的 children 指针（将当前节点替换为其子节点或清空）
/// - `delete_interior=true` 时：将子节点的 parent 指向当前节点的父节点（祖父）
/// - 子节点成为 root（parent=0）时：更新 SnapshotTrees btree 的 root_snapshot
/// - 写入 Whiteout 删除自身
///
/// 两个孩子节点时返回 `StorageError::InvalidData`（对齐 bcachefs -EBUSY, delete.c:186-193）。
pub fn bch2_snapshot_node_delete(
    engine: &mut BtreeEngine,
    id: u32,
    delete_interior: bool,
) -> Result<(), StorageError> {
    // delete.c:176: 读取快照数据（允许已删除，因为可能先 set_deleted 再调本函数）
    let snap = read_snapshot_value_allow_deleted(engine, id)
        .ok_or_else(|| StorageError::NotFound(format!("snapshot {} not found", id)))?;

    // delete.c:186-193: 两个孩子节点不能直接删除
    if snap.children[0] != 0 && snap.children[1] != 0 {
        return Err(StorageError::InvalidData(format!(
            "snapshot {} has two children, cannot delete",
            id
        )));
    }

    // 确定生存的子节点（如果有）
    let child = if snap.children[0] != 0 {
        snap.children[0]
    } else {
        snap.children[1]
    };

    // delete.c:206-252: 更新父节点的 children 指针
    if snap.parent != 0 {
        if let Some(parent_snap) = read_snapshot_value_allow_deleted(engine, snap.parent) {
            let mut new_parent = parent_snap;
            for slot in new_parent.children.iter_mut() {
                if *slot == id {
                    // bcachefs delete.c:240: le32_add_cpu(&parent->v.children[i], child - id)
                    // 用 Rust 直接赋值语义等价的 child（child=0 时清空，child>0 时替换）
                    *slot = child;
                    break;
                }
            }
            let bytes = bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;
            let entry = BtreeEntry::raw(Bpos::new(0, 0, snap.parent), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
    }

    // delete.c:272-280: delete_interior && child 存在 → 子节点 parent 指向祖父
    if delete_interior && child != 0 {
        if let Some(mut child_snap) = read_snapshot_value_allow_deleted(engine, child) {
            child_snap.parent = snap.parent;

            // 重新计算 depth
            if snap.parent != 0 {
                if let Some(grandparent) = read_snapshot_value_allow_deleted(engine, snap.parent) {
                    child_snap.depth = grandparent.depth + 1;
                }
            } else {
                // 成为新 root
                child_snap.depth = 1;
            }

            let bytes = bincode::serialize(&child_snap).map_err(StorageError::Serialization)?;
            let entry = BtreeEntry::raw(Bpos::new(0, 0, child), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
    }

    // delete.c:256-271: child 存在但无祖父 → 更新 SnapshotTrees root_snapshot
    if snap.parent == 0 && child != 0 {
        let tree_id = snap.tree;
        if tree_id != 0 {
            if let Some(mut tree_val) = read_snapshot_tree_value(engine, tree_id) {
                tree_val.root_snapshot = child;
                write_snapshot_tree_value(engine, tree_id, &tree_val)?;
            }
        }
    }

    // delete.c:284-290: 写入 Whiteout 删除自身
    bch2_snapshot_node_set_deleted(engine, id)?;

    Ok(())
}

/// 修复被删除 interior 节点的子节点的 depth/skip 字段。
///
/// 对齐 bcachefs `bch2_fix_child_of_deleted_snapshot()` (delete.c:611-662)。
/// 遍历所有快照节点，对拥有被删祖先的节点重新计算 depth 和 skip。
pub fn bch2_fix_child_of_deleted_snapshot(
    engine: &mut BtreeEngine,
    deleted_ids: &[u32],
) -> Result<(), StorageError> {
    let btree = engine.get(BtreeId::Snapshots);
    // 收集所有节点 ID 用于遍历
    let mut all_ids: Vec<(u32, Vec<u8>)> = Vec::new();
    btree.for_each_entry(|entry| {
        let sid = entry.pos.snapshot;
        let bytes = match &entry.value {
            KeyValue::Raw(b) => b.clone(),
            _ => return,
        };
        all_ids.push((sid, bytes));
    });
    // 释放 for_each_entry 的锁，后续需要写 BtreeEngine
    let _ = btree;

    for (id, bytes) in &all_ids {
        let snap: SnapshotT = match bincode::deserialize(bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // delete.c:621-622: 跳过自身在 deleted 列表中的节点
        if deleted_ids.contains(id) {
            continue;
        }
        // bch2_snapshot_is_ancestor_btree(descendant, ancestor)
        // deleted_id 是祖先，*id 是后代
        let nr_deleted_ancestors: u32 = deleted_ids
            .iter()
            .filter(|deleted_id| bch2_snapshot_is_ancestor_btree(engine, *id, **deleted_id))
            .count() as u32;

        if nr_deleted_ancestors == 0 {
            continue;
        }

        let depth = snap.depth.saturating_sub(nr_deleted_ancestors);
        let mut new_skip = [0u32; 3];

        if depth == 0 {
            // keep zeroed skip list
        } else {
            // 找第一个不在 deleted 中的祖先作为 effective_parent
            let parent_id = snap.parent;
            let effective_parent = if parent_id != 0 && deleted_ids.contains(&parent_id) {
                let mut p = parent_id;
                loop {
                    if !deleted_ids.contains(&p) {
                        break p;
                    }
                    if let Some(ps) = read_snapshot_value_allow_deleted(engine, p) {
                        if ps.parent == 0 {
                            break 0u32;
                        }
                        p = ps.parent;
                    } else {
                        break 0u32;
                    }
                }
            } else {
                parent_id
            };

            // 像 bch2_snapshot_node_create 一样从 effective_parent 重建 skip list
            if effective_parent != 0 {
                new_skip[0] = effective_parent;
                if let Some(ep_snap) = read_snapshot_value_allow_deleted(engine, effective_parent) {
                    if ep_snap.skip[0] != 0 {
                        new_skip[1] = ep_snap.skip[0];
                        if let Some(skip1_snap) =
                            read_snapshot_value_allow_deleted(engine, ep_snap.skip[0])
                        {
                            new_skip[2] = skip1_snap.skip[1];
                        }
                    }
                }
                new_skip.sort();
            }
        }

        let mut updated = snap;
        updated.depth = depth;
        updated.skip = new_skip;

        let new_bytes = bincode::serialize(&updated).map_err(StorageError::Serialization)?;
        let entry = BtreeEntry::raw(Bpos::new(0, 0, *id), KeyType::Normal, new_bytes);
        engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
    }

    Ok(())
}

/// 对齐 bcachefs `bch2_check_snapshot_needs_deletion()` (delete.c:853-878)。
/// 检查 snapshot 是否需要 delete 处理。
pub fn bch2_check_snapshot_needs_deletion(snap: &SnapshotT) -> bool {
    if snap.flags.contains(BchSnapshotFlags::NO_KEYS) {
        return false;
    }
    if snap.flags.contains(BchSnapshotFlags::WILL_DELETE) {
        return true;
    }
    if snap.children[0] != 0 && snap.children[1] == 0
        || snap.children[0] == 0 && snap.children[1] != 0
    {
        return true;
    }
    false
}

/// 检测快照是否可删除及其类型。
///
/// 对齐 bcachefs `check_should_delete_snapshot()` (delete.c:532-610)。
///
/// 返回:
/// - `None` — 不可删除（有 subvol 引用）
/// - `Some(DeadSnapshotType::Leaf)` — 叶子节点可删除
/// - `Some(DeadSnapshotType::Interior)` — interior 节点可删除（NO_KEYS, subvol==0）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadSnapshotType {
    Leaf,
    Interior,
}

pub fn check_should_delete_snapshot(snap: &SnapshotT) -> Option<DeadSnapshotType> {
    if snap.deleted || snap.flags.contains(BchSnapshotFlags::WILL_DELETE) {
        return Some(DeadSnapshotType::Leaf);
    }
    if snap.subvol == 0 && snap.children == [0, 0] {
        return Some(DeadSnapshotType::Leaf);
    }
    if snap.flags.contains(BchSnapshotFlags::NO_KEYS) && snap.subvol == 0 {
        return Some(DeadSnapshotType::Interior);
    }
    None
}

/// 批量删除所有标记为 deleted 的死快照。
///
/// 对齐 bcachefs `bch2_delete_dead_snapshots()`。
///
/// 流程：
/// 1. 全量扫描 Snapshots btree，收集 deleted==true 的节点
/// 2. 调用 fix_child_of_deleted_snapshot 修复受影响子节点的 depth/skip
/// 3. 对每个已删除节点，DFS 遍历其子树
/// 4. 如果子树中有被 volume 引用的快照（subvol != 0），跳过该子树
/// 5. 叶子→根后序删除，更新父节点 children
/// 6. 返回跳过的 snapshot_id 列表（被 volume 引用）
pub fn bch2_delete_dead_snapshots(engine: &mut BtreeEngine) -> Result<Vec<u32>, StorageError> {
    use std::collections::HashMap;

    // 1. 全量扫描，收集所有快照（含已删除）
    let mut all_snaps: HashMap<u32, SnapshotT> = HashMap::new();
    {
        let btree = engine.get(BtreeId::Snapshots);
        btree.for_each_entry(|entry| {
            let sid = entry.pos.snapshot;
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                all_snaps.insert(sid, snap);
            }
        });
    }

    // 2. 找出已删除的快照
    let deleted_ids: Vec<u32> = all_snaps
        .iter()
        .filter(|(_, snap)| snap.deleted)
        .map(|(&id, _)| id)
        .collect();

    // 3. 先调用 fix_child_of_deleted_snapshot 修复所有受影响的子节点（depth/skip）
    //    这一步必须在实际删除之前做，因为删除会清除父指针
    if !deleted_ids.is_empty() {
        bch2_fix_child_of_deleted_snapshot(engine, &deleted_ids)?;
    }

    let mut skipped = Vec::new();

    // 工具函数：在 HashMap 上 DFS 检查 volume 引用
    fn has_volume_ref(all_snaps: &HashMap<u32, SnapshotT>, id: u32) -> bool {
        let snap = match all_snaps.get(&id) {
            Some(s) => s,
            None => return false,
        };
        if snap.has_subvol() {
            return true;
        }
        if snap.children[0] != 0 && has_volume_ref(all_snaps, snap.children[0]) {
            return true;
        }
        if snap.children[1] != 0 && has_volume_ref(all_snaps, snap.children[1]) {
            return true;
        }
        false
    }

    for &dead_id in &deleted_ids {
        // 4. 检查子树 volume 引用
        if has_volume_ref(&all_snaps, dead_id) {
            skipped.push(dead_id);
            continue;
        }

        let dead_snap = match all_snaps.get(&dead_id) {
            Some(s) => s.clone(),
            None => continue,
        };

        // 5. 从父节点 children 列表移除自身
        if dead_snap.parent != 0 {
            if let Some(parent_snap) = all_snaps.get(&dead_snap.parent) {
                let mut new_parent = parent_snap.clone();
                if new_parent.children[0] == dead_id {
                    new_parent.children[0] = 0;
                }
                if new_parent.children[1] == dead_id {
                    new_parent.children[1] = 0;
                }
                let parent_bytes =
                    bincode::serialize(&new_parent).map_err(StorageError::Serialization)?;
                let parent_entry = BtreeEntry::raw(
                    Bpos::new(0, 0, dead_snap.parent),
                    KeyType::Normal,
                    parent_bytes,
                );
                engine.insert_entry_raw(BtreeId::Snapshots, parent_entry, 0);
            }
        }

        // 6. 删除自身（写入 Whiteout）
        //    - 如果是 leaf（无 children）：直接删除
        //    - 如果是 interior（有 children）：
        //      fix_child_of_deleted_snapshot 已在步骤 2 中修复子节点的 depth/skip，
        //      子节点保持存活（parent 指针仍指向已删节点，但 skip list 跳过它）
        if read_snapshot_value(engine, dead_id).is_some() {
            bch2_snapshot_node_set_deleted(engine, dead_id)?;
        }
    }

    if !skipped.is_empty() || !deleted_ids.is_empty() {
        engine.get_mut(BtreeId::Snapshots).compact();
    }

    Ok(skipped)
}

/// 删除所有 NO_KEYS 标记且仅有一个子节点的 interior 快照。
///
/// 对齐 bcachefs `bch2_delete_dead_interior_snapshots()` (delete.c:811-851)。
///
/// 流程:
/// 1. 遍历 Snapshots btree，收集 NO_KEYS + 单子节点 + 非 deleted 的 interior
/// 2. 调用 fix_child_of_deleted_snapshot 修复受影响子节点的 depth/skip
/// 3. 对每个 interior 调用 bch2_snapshot_node_delete(id, delete_interior=true)
///
/// 注意：调用者应先运行 bch2_check_snapshots 保证树结构一致（对齐 bcachefs delete.c:828）。
pub fn bch2_delete_dead_interior_snapshots(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    // 1. 遍历收集 NO_KEYS + 单子节点 interior
    let mut interior_deletes: Vec<(u32, u32)> = Vec::new();
    {
        let btree = engine.get(BtreeId::Snapshots);
        btree.for_each_entry(|entry| {
            let sid = entry.pos.snapshot;
            if sid == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                if snap.deleted {
                    return;
                }
                // NO_KEYS + 恰好一个子节点
                if snap.flags.contains(BchSnapshotFlags::NO_KEYS)
                    && ((snap.children[0] != 0) != (snap.children[1] != 0))
                {
                    interior_deletes.push((sid, snap.parent));
                }
            }
        });
    }

    if interior_deletes.is_empty() {
        return Ok(());
    }

    // 2. fix_child_of_deleted_snapshot 修复受影响子节点
    let deleted_ids: Vec<u32> = interior_deletes.iter().map(|&(id, _)| id).collect();
    bch2_fix_child_of_deleted_snapshot(engine, &deleted_ids)?;

    // 3. 逐个删除 interior（从叶子端开始处理）
    for &(id, _parent) in &interior_deletes {
        // 跳过已被前序删除影响的节点
        if read_snapshot_value(engine, id).is_none()
            && read_snapshot_value_allow_deleted(engine, id).is_none()
        {
            continue;
        }
        bch2_snapshot_node_delete(engine, id, true)?;
    }

    engine.get_mut(BtreeId::Snapshots).compact();
    Ok(())
}

/// 迭代器风格的 DFS 遍历器（基于栈，不递归）。
pub struct DfsIter {
    /// 待遍历的栈 (id, visited_children)
    stack: Vec<(u32, bool)>,
    engine: *const BtreeEngine,
}

impl DfsIter {
    /// 创建一个新的 DFS 遍历器，从 snapshot_id 开始。
    pub fn new(engine: &BtreeEngine, snapshot_id: u32) -> Self {
        Self {
            stack: vec![(snapshot_id, false)],
            engine: engine as *const BtreeEngine,
        }
    }

    /// 内部读取快照（通过裸指针转换，需要调用者保证 engine 存活期长于 iter）
    fn read_snap(&self, id: u32) -> Option<SnapshotT> {
        // SAFETY: DfsIter 不修改 engine，且要求调用者保证 engine 存活
        read_snapshot_value(unsafe { &*self.engine }, id)
    }
}

impl Iterator for DfsIter {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let peek_id = match self.stack.last() {
                Some(&(id, _)) => id,
                None => return None,
            };
            let snap = self.read_snap(peek_id)?;
            // 检查栈顶是否已完成 children
            let (_id, visited) = self.stack.last_mut()?;
            if *visited {
                let (id, _) = self.stack.pop().unwrap();
                return Some(id);
            }
            *visited = true;
            // 右子节点先入栈（后入先出，保证左子先出）
            if snap.children[1] != 0 {
                self.stack.push((snap.children[1], false));
            }
            if snap.children[0] != 0 {
                self.stack.push((snap.children[0], false));
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════
// Layer 3: Key Snapshot 验证 + 重建
// ═══════════════════════════════════════════════════════════════

/// Key 快照 ID 检查结果。
///
/// 对齐 bcachefs `__bch2_check_key_has_snapshot()` 的返回值语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKeySnapshotResult {
    /// 快照 ID 存在且活跃，key 有效
    Valid,
    /// 快照已被删除，key 应被删除
    ShouldDelete,
    /// 快照 ID 不存在（缺失），需要重建快照
    Missing,
}

/// 检查 key 的快照 ID 是否有效。
///
/// 对齐 bcachefs `bch2_check_key_has_snapshot()` 的简化版，
/// 通过 SnapshotTable 查询状态，无需 btree 遍历。
///
/// # bcachefs 对齐
///
/// bcachefs 原版：
/// ```c
/// static inline int bch2_check_key_has_snapshot(struct btree_trans *trans,
///                                               struct btree_iter *iter,
///                                               struct bkey_s_c k)
/// {
///     return likely(bch2_snapshot_exists(trans->c, k.k->p.snapshot))
///         ? 0
///         : __bch2_check_key_has_snapshot(trans, iter, k);
/// }
/// ```
/// 本函数合并了快速路径（exists 检查）和慢速路径（状态分析）。
pub fn bch2_check_key_has_snapshot(
    table: &SnapshotTable,
    snapshot_id: u32,
) -> CheckKeySnapshotResult {
    if snapshot_id == 0 {
        // snapshot_id=0: 非快照空间（根/特殊键），始终有效
        return CheckKeySnapshotResult::Valid;
    }
    match table.id_state(snapshot_id) {
        SnapshotIdState::Live => CheckKeySnapshotResult::Valid,
        SnapshotIdState::Deleted => CheckKeySnapshotResult::ShouldDelete,
        SnapshotIdState::Empty => CheckKeySnapshotResult::Missing,
    }
}

/// 快照感知的 btree 列表。
///
/// 对齐 bcachefs 中 `btree_type_has_snapshots()` 检查的 btree：
/// - Extents: 数据块映射（每 key 含 snapshot_id）
/// - Subvolumes: 子卷记录（每 entry 含 snapshot 字段）
///
/// 注意：Snapshots 和 SnapshotTrees btree 本身不在此列表中，
/// 它们是快照元数据的存储载体。
const SNAPSHOT_AWARE_BTREES: [BtreeId; 2] = [BtreeId::Extents, BtreeId::Subvolumes];

/// 重建缺失的快照条目。
///
/// 对齐 bcachefs `bch2_reconstruct_snapshots()`。
///
/// 算法：
/// 1. 扫描所有快照感知 btree，收集被引用的 snapshot ID
/// 2. 与 Snapshots btree 中已存在的 ID 比较
/// 3. 对每个缺失的 ID：
///    a. 检查 SnapshotTrees btree 中是否有对应的 tree 条目，没有则创建
///    b. 创建快照条目（tree、btime、如果子卷引用则设 subvol/SUBVOL）
///
/// # bcachefs 对齐
///
/// bcachefs 原版使用 `snapshot_tree_reconstruct` 状态机将 ID 分类为树组，
/// 然后使用 `check_snapshot_exists()` 逐个重建。本函数是简化版，
/// 直接扫描所有 btree 收集引用 ID，对缺失 ID 创建独立条目。
pub fn bch2_reconstruct_snapshots(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    // 1. 从 Snapshots btree 收集已存在的 snapshot ID
    let existing: HashSet<u32> = {
        let btree = engine.get(BtreeId::Snapshots);
        let mut set = HashSet::new();
        btree.for_each_entry(|entry| {
            let sid = entry.pos.snapshot;
            if sid != 0 {
                set.insert(sid);
            }
        });
        set
    };

    // 2. 扫描所有快照感知 btree，收集被引用的 snapshot ID
    //    使用 HashSet 自动去重
    let mut referenced: HashSet<u32> = HashSet::new();

    for btree_id in &SNAPSHOT_AWARE_BTREES {
        let btree = engine.get(*btree_id);
        btree.for_each_entry(|entry| {
            let sid = entry.pos.snapshot;
            if sid != 0 {
                referenced.insert(sid);
            }
        });
    }

    // 3. 找出缺失的 ID：被引用但不在 Snapshots btree 中
    let missing: Vec<u32> = {
        let mut v: Vec<u32> = referenced.difference(&existing).copied().collect();
        v.sort_unstable();
        v
    };

    if missing.is_empty() {
        return Ok(());
    }

    // 4. 获取当前时间作为 btime
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // 5. 收集已有的 tree_id → root_snapshot 映射
    let mut tree_roots: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    {
        let btree = engine.get(BtreeId::SnapshotTrees);
        btree.for_each_entry(|entry| {
            let tree_id = entry.pos.snapshot;
            if tree_id != 0 {
                let bytes = match &entry.value {
                    KeyValue::Raw(b) => b,
                    _ => return,
                };
                if let Ok(tree) = bincode::deserialize::<SnapshotTreeT>(bytes) {
                    tree_roots.insert(tree_id, tree.root_snapshot);
                }
            }
        });
    }

    // 6. 为每个缺失 ID 重建快照条目
    //    从 SnapshotTrees 中找到匹配的 root，或创建新条目
    //    同时扫描 Subvolumes btree 查找相关的子卷。
    //    注意：在迭代中创建新条目，因此需要克隆
    let missing_clone = missing.clone();
    let mut pending_trees: Vec<(u32, u32)> = Vec::new(); // (snap_id, tree_id)

    for &snap_id in &missing_clone {
        // 6a. 寻找或分配 tree_id
        //     检查是否有已有的 SnapshotTree 以此 ID 为 root
        let tree_id = match tree_roots.iter().find(|(_, &root)| root == snap_id) {
            Some((&tid, _)) => tid,
            None => {
                // 在 SnapshotTrees btree 中找空位分配新 tree_id
                let max_existing_id = tree_roots.keys().max().copied().unwrap_or(0);
                let new_tree_id = max_existing_id + 1;

                // 创建 SnapshotTree 条目
                let tree_entry = SnapshotTreeT::new(snap_id, 0);
                let bytes = bincode::serialize(&tree_entry).map_err(StorageError::Serialization)?;
                let entry = BtreeEntry::raw(Bpos::new(0, 0, new_tree_id), KeyType::Normal, bytes);
                engine.insert_entry_raw(BtreeId::SnapshotTrees, entry, 0);
                tree_roots.insert(new_tree_id, snap_id);
                new_tree_id
            }
        };

        pending_trees.push((snap_id, tree_id));
    }

    // 7. 扫描 Subvolumes btree 获取 snapshot→subvol 映射
    //    注意：Subvolumes 的 Bpos 是 (subvol_id, 0, snapshot_id)
    let mut snap_to_subvol: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    {
        let btree = engine.get(BtreeId::Subvolumes);
        btree.for_each_entry(|entry| {
            let sid = entry.pos.snapshot;
            let subvol_id = entry.pos.inode as u32;
            if sid != 0 && subvol_id != 0 {
                snap_to_subvol.insert(sid, subvol_id);
            }
        });
    }

    // 8. 创建 Snapshot 条目
    for (snap_id, tree_id) in &pending_trees {
        let subvol = snap_to_subvol.get(snap_id).copied().unwrap_or(0);
        let snap = if subvol != 0 {
            SnapshotT::new_leaf(0, subvol, *tree_id, 1, now)
        } else {
            // 无子卷的快照：空 leaf，无 SUBVOL 标志
            SnapshotT {
                state: SnapshotIdState::Live,
                parent: 0,
                children: [0, 0],
                subvol: 0,
                tree: *tree_id,
                skip: [0, 0, 0],
                is_ancestor: 0,
                depth: 1,
                btime: now,
                deleted: false,
                flags: BchSnapshotFlags::empty(),
            }
        };

        let bytes = bincode::serialize(&snap).map_err(StorageError::Serialization)?;
        let entry = BtreeEntry::raw(Bpos::new(0, 0, *snap_id), KeyType::Normal, bytes);
        engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
    }

    // 9. 压缩 btree
    engine.get_mut(BtreeId::Snapshots).compact();
    engine.get_mut(BtreeId::SnapshotTrees).compact();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::BtreeEngine;

    fn make_engine() -> BtreeEngine {
        BtreeEngine::new()
    }

    // ─── 根快照创建测试 ───

    #[test]
    fn test_create_root_snapshot() {
        let mut engine = make_engine();
        let id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        assert_eq!(id, u32::MAX, "first root snapshot should get u32::MAX");

        let snap = read_snapshot_value(&engine, id).unwrap();
        assert_eq!(snap.parent, 0);
        assert_eq!(snap.subvol, 1);
        assert_eq!(snap.depth, 1);
        assert!(!snap.deleted);
        assert!(snap.is_leaf());
        assert!(snap.has_subvol());
    }

    #[test]
    fn test_create_root_snapshot_twice() {
        let mut engine = make_engine();
        let id1 = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let id2 = create_root_snapshot_btree(&mut engine, 2).unwrap();
        assert_eq!(id1, u32::MAX);
        assert_eq!(id2, u32::MAX - 1, "second root should decrement");

        let snap2 = read_snapshot_value(&engine, id2).unwrap();
        assert_eq!(snap2.subvol, 2);
    }

    // ─── 子快照创建测试 ───

    #[test]
    fn test_create_child_snapshot() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();

        assert_eq!(child, u32::MAX - 1);

        let child_snap = read_snapshot_value(&engine, child).unwrap();
        assert_eq!(child_snap.parent, root);
        assert_eq!(child_snap.depth, 2);
        assert!(!child_snap.deleted);

        // 父节点的 children 应已更新
        let root_snap = read_snapshot_value(&engine, root).unwrap();
        assert_eq!(root_snap.children[0], child);
    }

    #[test]
    fn test_create_deep_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        let mut ids = vec![root];
        for _ in 0..10 {
            let id = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
            ids.push(id);
            prev = id;
        }

        // 验证 depth 递增
        for (i, &id) in ids.iter().enumerate() {
            let snap = read_snapshot_value(&engine, id).unwrap();
            assert_eq!(snap.depth, i as u32 + 1, "depth mismatch for id={}", id);
        }
    }

    // ─── is_ancestor_from_btree 测试 ───

    #[test]
    fn test_is_ancestor_self() {
        let engine = make_engine();
        assert!(is_ancestor_from_btree(&engine, 42, 42));
    }

    #[test]
    fn test_is_ancestor_root_and_child() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();

        assert!(is_ancestor_from_btree(&engine, root, child));
        assert!(!is_ancestor_from_btree(&engine, child, root));
    }

    #[test]
    fn test_is_ancestor_no_relation() {
        let mut engine = make_engine();
        let t1 = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let t2 = create_root_snapshot_btree(&mut engine, 2).unwrap();

        assert!(!is_ancestor_from_btree(&engine, t1, t2));
        assert!(!is_ancestor_from_btree(&engine, t2, t1));
    }

    #[test]
    fn test_is_ancestor_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        let mut ids = vec![root];
        for _ in 0..20 {
            let id = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
            ids.push(id);
            prev = id;
        }

        // root 是所有后代祖先
        for &id in &ids {
            assert!(
                is_ancestor_from_btree(&engine, root, id),
                "root should be ancestor of {}",
                id
            );
        }

        // 每层的祖先关系
        for i in 0..ids.len() {
            for j in i..ids.len() {
                assert!(
                    is_ancestor_from_btree(&engine, ids[i], ids[j]),
                    "{} should be ancestor of {}",
                    ids[i],
                    ids[j]
                );
            }
        }

        // 反向不是祖先
        for i in 0..ids.len() {
            for j in 0..i {
                assert!(
                    !is_ancestor_from_btree(&engine, ids[i], ids[j]),
                    "{} should NOT be ancestor of {}",
                    ids[i],
                    ids[j]
                );
            }
        }
    }

    #[test]
    fn test_is_ancestor_deleted_node() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();

        bch2_snapshot_node_set_deleted(&mut engine, child).unwrap();
        // 删除后，祖先链仍然有效（对齐 bcachefs：WILL_DELETE 节点仍在树中可遍历）
        assert!(
            is_ancestor_from_btree(&engine, root, child),
            "deleted node should still have valid ancestor chain"
        );
    }

    // ─── Skiplist 测试 ───

    #[test]
    fn test_skip_list_depth_under_4() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // depth=1（root）: skip 全为 0
        let root_snap = read_snapshot_value(&engine, root).unwrap();
        assert_eq!(root_snap.skip, [0, 0, 0]);

        // depth=2: skip 均为 root 或 d2（两者互为祖先），且已排序
        let d2 = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let snap2 = read_snapshot_value(&engine, d2).unwrap();
        assert_eq!(snap2.depth, 2);
        for &s in &snap2.skip {
            assert!(
                s == 0 || s == root || s == d2,
                "depth-2 skip entry {s} must be 0, root({root}), or d2({d2})"
            );
        }
        assert!(
            snap2.skip[0] <= snap2.skip[1] && snap2.skip[1] <= snap2.skip[2],
            "depth-2 skip should be sorted"
        );

        // depth=3: skip 均为 root, d2, 或 d3，且已排序
        let d3 = bch2_snapshot_node_create(&mut engine, d2, 1, None).unwrap();
        let snap3 = read_snapshot_value(&engine, d3).unwrap();
        assert_eq!(snap3.depth, 3);
        for &s in &snap3.skip {
            assert!(
                s == 0 || s == root || s == d2 || s == d3,
                "depth-3 skip entry {s} must be 0, root({root}), d2({d2}), or d3({d3})"
            );
        }
        assert!(
            snap3.skip[0] <= snap3.skip[1] && snap3.skip[1] <= snap3.skip[2],
            "depth-3 skip should be sorted"
        );
    }

    #[test]
    fn test_skip_list_populated_exponential() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        for _ in 0..5 {
            prev = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
        }
        let snap = read_snapshot_value(&engine, prev).unwrap();
        assert_eq!(snap.depth, 6);
        assert!(snap.skip[0] != 0, "skip[0] should be populated at depth 6");
        assert!(snap.skip[1] != 0, "skip[1] should be populated at depth 6");
        assert!(snap.skip[2] != 0, "skip[2] should be populated at depth 6");
    }

    #[test]
    fn test_skip_list_ordered() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        for _ in 0..20 {
            prev = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
        }
        let snap = read_snapshot_value(&engine, prev).unwrap();
        // skip[0] <= skip[1] <= skip[2]（非降序，bubble_sort 允许相等）
        // 快照 ID 从 u32::MAX 向下分配（父 > 子），祖先越老 ID 越大。
        if snap.skip[1] != 0 {
            assert!(
                snap.skip[0] <= snap.skip[1],
                "skip[0]={} <= skip[1]={}",
                snap.skip[0],
                snap.skip[1]
            );
        }
        if snap.skip[2] != 0 {
            assert!(
                snap.skip[1] <= snap.skip[2],
                "skip[1]={} <= skip[2]={}",
                snap.skip[1],
                snap.skip[2]
            );
        }
    }

    #[test]
    fn test_skip_list_ancestor_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        for _ in 0..20 {
            prev = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
        }
        let snap = read_snapshot_value(&engine, prev).unwrap();
        // 确认 skip 中的 ID 确实是 prev 的祖先
        for &s in &snap.skip {
            if s != 0 {
                assert!(
                    is_ancestor_from_btree(&engine, s, prev),
                    "skip {} should be ancestor of {}",
                    s,
                    prev
                );
            }
        }
    }

    // ─── 删除操作测试 ───

    #[test]
    fn test_delete_snapshot() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();

        bch2_snapshot_node_set_deleted(&mut engine, child).unwrap();

        // 删除后 Whiteout 取代 Normal，read_snapshot_value 应返回 None
        assert!(
            read_snapshot_value(&engine, child).is_none(),
            "deleted snapshot should not be readable"
        );

        // list 不应包含已删除的快照
        let list = list_snapshots_from_btree(&engine);
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&root), "root should still be in list");
        assert!(!ids.contains(&child), "child should not be in list");
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut engine = make_engine();
        let result = bch2_snapshot_node_set_deleted(&mut engine, 999);
        assert!(result.is_err());
    }

    // ─── 列表查询测试 ───

    #[test]
    fn test_list_snapshots_empty() {
        let engine = make_engine();
        let list = list_snapshots_from_btree(&engine);
        assert!(list.is_empty());
    }

    #[test]
    fn test_list_snapshots() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let c1 = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let c2 = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        let list = list_snapshots_from_btree(&engine);
        assert_eq!(list.len(), 3);

        // 按 ID 降序排列（父优先）
        assert_eq!(list[0].0, root); // u32::MAX
                                     // c1 和 c2 在后面
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&c1));
        assert!(ids.contains(&c2));
    }

    #[test]
    fn test_list_snapshots_after_delete() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let _child2 = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        bch2_snapshot_node_set_deleted(&mut engine, child).unwrap();
        let list = list_snapshots_from_btree(&engine);
        // 被删除的 child 不应出现
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&root));
        assert!(!ids.contains(&child));
    }

    // ─── get_next_snapshot_id 测试 ───

    #[test]
    fn test_next_id_empty() {
        let engine = make_engine();
        assert_eq!(get_next_snapshot_id(&engine), u32::MAX);
    }

    #[test]
    fn test_next_id_after_creation() {
        let mut engine = make_engine();
        let id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        assert_eq!(id, u32::MAX);
        // 创建后，next_id 应返回 u32::MAX - 1
        assert_eq!(get_next_snapshot_id(&engine), u32::MAX - 1);
    }

    // ─── 序列化 roundtrip 测试 ───

    #[test]
    fn test_snapshot_value_serde_in_btree() {
        let mut engine = make_engine();
        let id = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let snap = read_snapshot_value(&engine, id).unwrap();

        assert_eq!(snap.parent, 0);
        assert_eq!(snap.subvol, 1);
        assert_eq!(snap.depth, 1);
        assert!(!snap.deleted);

        // 验证 btree 中存在
        let entry = engine.get_entry_raw(BtreeId::Snapshots, Bpos::new(0, 0, id));
        assert!(entry.is_some(), "snapshot should exist in btree");
    }

    // ─── 大规模树祖先测试 ───

    #[test]
    fn test_is_ancestor_large_tree() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let mut prev = root;
        for _ in 0..100 {
            prev = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
        }

        // root 是最后一个节点的祖先
        assert!(is_ancestor_from_btree(&engine, root, prev));
        // 中间节点也是祖先
        let mid = u32::MAX - 50;
        assert!(is_ancestor_from_btree(&engine, mid, prev));
        // 反向不是
        assert!(!is_ancestor_from_btree(&engine, prev, mid));
    }

    // ─── DFS 深度优先遍历测试 ───

    #[test]
    fn test_dfs_descendants_single_node() {
        let engine = make_engine();
        let result = dfs_descendants(&engine, 42);
        assert!(result.is_empty(), "single non-existent node");
    }

    #[test]
    fn test_dfs_descendants_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let c1 = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let c2 = bch2_snapshot_node_create(&mut engine, c1, 1, None).unwrap();

        let result = dfs_descendants(&engine, root);
        assert!(!result.is_empty(), "should have descendants");
        assert_eq!(result.len(), 3);
        // 后序：子→父→根
        assert_eq!(result[0], c2, "first should be leaf (c2)");
        assert_eq!(result[2], root, "last should be root");
    }

    #[test]
    fn test_dfs_descendants_binary_tree() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let left = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        // 手动将 left 设为 children[0]
        {
            let mut root_snap = read_snapshot_value(&engine, root).unwrap();
            root_snap.children[0] = left;
            let bytes = bincode::serialize(&root_snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let right = get_next_snapshot_id(&engine);
        // 手动创建 right 节点
        let right_snap = SnapshotT::new_leaf(root, 1, 0, 2, current_timestamp());
        let bytes = bincode::serialize(&right_snap).unwrap();
        let entry = BtreeEntry::raw(Bpos::new(0, 0, right), KeyType::Normal, bytes);
        engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        // 更新 root.children[1] = right
        {
            let mut root_snap = read_snapshot_value(&engine, root).unwrap();
            root_snap.children[1] = right;
            let bytes = bincode::serialize(&root_snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        engine.get_mut(BtreeId::Snapshots).compact();

        // DFS 应遍历所有节点
        let result = dfs_descendants(&engine, root);
        assert_eq!(result.len(), 3, "should find all 3 nodes");
        assert!(result.contains(&left));
        assert!(result.contains(&right));
        assert!(result.contains(&root));
    }

    #[test]
    fn test_dfs_descendants_alive_filters_deleted() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let _grandchild = bch2_snapshot_node_create(&mut engine, child, 1, None).unwrap();

        // 标记 child 为已删除
        bch2_snapshot_node_set_deleted(&mut engine, child).unwrap();

        let alive = dfs_descendants_alive(&engine, child);
        assert!(!alive.contains(&child), "child should be skipped");
    }

    // ─── DfsIter 迭代器测试 ───

    #[test]
    fn test_dfs_iter_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let c1 = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let c2 = bch2_snapshot_node_create(&mut engine, c1, 1, None).unwrap();

        let iter = DfsIter::new(&engine, root);
        let ids: Vec<u32> = iter.collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], c2, "DFS iter should start with leaf");
        assert_eq!(ids[2], root, "DFS iter should end with root");
    }

    // ─── 参数化 Skip List 有序性测试 ───

    /// 验证不同深度下 skip list 的有序性：skip[0] < skip[1] < skip[2]。
    /// 测试深度 1~20，确保在各级深度上都满足递增顺序。
    #[test]
    fn test_skip_list_ordered_depth_1() {
        test_skip_ordered_at_depth(1);
    }
    #[test]
    fn test_skip_list_ordered_depth_2() {
        test_skip_ordered_at_depth(2);
    }
    #[test]
    fn test_skip_list_ordered_depth_3() {
        test_skip_ordered_at_depth(3);
    }
    #[test]
    fn test_skip_list_ordered_depth_4() {
        test_skip_ordered_at_depth(4);
    }
    #[test]
    fn test_skip_list_ordered_depth_5() {
        test_skip_ordered_at_depth(5);
    }
    #[test]
    fn test_skip_list_ordered_depth_6() {
        test_skip_ordered_at_depth(6);
    }
    #[test]
    fn test_skip_list_ordered_depth_7() {
        test_skip_ordered_at_depth(7);
    }
    #[test]
    fn test_skip_list_ordered_depth_8() {
        test_skip_ordered_at_depth(8);
    }
    #[test]
    fn test_skip_list_ordered_depth_9() {
        test_skip_ordered_at_depth(9);
    }
    #[test]
    fn test_skip_list_ordered_depth_10() {
        test_skip_ordered_at_depth(10);
    }
    #[test]
    fn test_skip_list_ordered_depth_11() {
        test_skip_ordered_at_depth(11);
    }
    #[test]
    fn test_skip_list_ordered_depth_12() {
        test_skip_ordered_at_depth(12);
    }
    #[test]
    fn test_skip_list_ordered_depth_13() {
        test_skip_ordered_at_depth(13);
    }
    #[test]
    fn test_skip_list_ordered_depth_14() {
        test_skip_ordered_at_depth(14);
    }
    #[test]
    fn test_skip_list_ordered_depth_15() {
        test_skip_ordered_at_depth(15);
    }
    #[test]
    fn test_skip_list_ordered_depth_16() {
        test_skip_ordered_at_depth(16);
    }
    #[test]
    fn test_skip_list_ordered_depth_17() {
        test_skip_ordered_at_depth(17);
    }
    #[test]
    fn test_skip_list_ordered_depth_18() {
        test_skip_ordered_at_depth(18);
    }
    #[test]
    fn test_skip_list_ordered_depth_19() {
        test_skip_ordered_at_depth(19);
    }
    #[test]
    fn test_skip_list_ordered_depth_20() {
        test_skip_ordered_at_depth(20);
    }

    fn test_skip_ordered_at_depth(target_depth: u32) {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();

        // 创建链到 target_depth（root 是 depth=1）
        let mut prev = root;
        for _ in 1..target_depth {
            prev = bch2_snapshot_node_create(&mut engine, prev, 1, None).unwrap();
        }

        let snap = read_snapshot_value(&engine, prev).unwrap();
        assert_eq!(snap.depth, target_depth, "depth mismatch");

        // depth = 1 时 skip 全为 0，不检查有序性
        if target_depth == 1 {
            assert_eq!(snap.skip, [0, 0, 0], "depth=1 should have empty skip");
            return;
        }
        // depth >= 4 时才可能有 skip[2] != 0；depth=2,3 只有跳过 skip[2] 的有序性检查

        // depth >= 4 时验证 skip[0] <= skip[1] <= skip[2]（非降序，bubble_sort 允许相等）
        // 快照 ID 从 u32::MAX 向下分配（父 > 子），祖先越老 ID 越大。
        if snap.skip[1] != 0 {
            assert!(
                snap.skip[0] <= snap.skip[1],
                "depth={}: skip[0]={} <= skip[1]={}",
                target_depth,
                snap.skip[0],
                snap.skip[1]
            );
        }
        if snap.skip[2] != 0 {
            assert!(
                snap.skip[1] <= snap.skip[2],
                "depth={}: skip[1]={} <= skip[2]={}",
                target_depth,
                snap.skip[1],
                snap.skip[2]
            );
        }
    }

    // ─── bch2_fix_child_of_deleted_snapshot 测试 ───

    #[test]
    fn test_fix_child_of_deleted_depth_adjust() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // root → a → b → leaf
        let a = bch2_snapshot_node_create(&mut engine, root, 0, None).unwrap();
        let b = bch2_snapshot_node_create(&mut engine, a, 0, None).unwrap();
        // 手动清除 SUBVOL 让 non-leaf 节点无 subvol
        for id in [a, b] {
            let mut snap = read_snapshot_value(&engine, id).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf = bch2_snapshot_node_create(&mut engine, b, 2, None).unwrap();

        // 删除 b（interior），leaf 的 depth 应从 4 减到 3，skip 应更新
        bch2_snapshot_node_set_deleted(&mut engine, b).unwrap();
        bch2_fix_child_of_deleted_snapshot(&mut engine, &[b]).unwrap();

        let leaf_snap = read_snapshot_value(&engine, leaf).unwrap();
        assert_eq!(leaf_snap.depth, 3, "depth should reduce from 4 to 3");
        // skip 按值升序排列（0 < a < root），不应包含已删节点 b
        for &s in &leaf_snap.skip {
            if s != 0 {
                assert_ne!(s, b, "skip should not reference deleted node b");
                // 所有非 0 skip 必须是 leaf 的祖先
                assert!(
                    bch2_snapshot_is_ancestor_btree(&engine, leaf, s),
                    "skip {s} should be ancestor of leaf"
                );
            }
        }
    }

    #[test]
    fn test_fix_child_of_deleted_skip_replacement() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // 建一条长链用于测试 skip 重定向
        let mut ids = vec![root];
        for _ in 0..8 {
            let id = bch2_snapshot_node_create(&mut engine, *ids.last().unwrap(), 0, None).unwrap();
            ids.push(id);
        }
        // 清除 interior 的 SUBVOL
        for i in 1..8 {
            let mut snap = read_snapshot_value(&engine, ids[i]).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, ids[i]), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf_id =
            bch2_snapshot_node_create(&mut engine, *ids.last().unwrap(), 2, None).unwrap();

        // 记录删除前的 leaf skip
        let before = read_snapshot_value(&engine, leaf_id).unwrap();
        let before_skip = before.skip;

        // 删除 ids[6]（id at position 6, one before leaf）
        let deleted = ids[6];
        bch2_snapshot_node_set_deleted(&mut engine, deleted).unwrap();
        bch2_fix_child_of_deleted_snapshot(&mut engine, &[deleted]).unwrap();

        let after = read_snapshot_value(&engine, leaf_id).unwrap();
        assert_eq!(after.depth, before.depth - 1, "depth reduced by 1");
        // skip 应该变化（移除了对被删节点的引用）
        assert_ne!(after.skip, before_skip, "skip should change after fixup");
        // 所有 skip 不应指向已删节点
        for s in after.skip {
            if s != 0 {
                assert_ne!(s, deleted, "skip should not point to deleted node");
            }
        }
    }

    #[test]
    fn test_fix_child_of_deleted_empty_deleted_list() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let _child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        // 空 deleted_ids → 不应有变化
        bch2_fix_child_of_deleted_snapshot(&mut engine, &[]).unwrap();

        let snap = read_snapshot_value(&engine, root).unwrap();
        assert_eq!(snap.depth, 1, "should not change");
    }

    #[test]
    fn test_fix_child_of_deleted_self_in_list() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        // 自身在 deleted_ids 中应跳过
        bch2_fix_child_of_deleted_snapshot(&mut engine, &[child]).unwrap();
        let snap = read_snapshot_value(&engine, root).unwrap();
        assert_eq!(snap.depth, 1, "root should not change");
    }

    // ─── bch2_snapshot_node_delete 测试 ───

    #[test]
    fn test_snapshot_node_delete_leaf() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        bch2_snapshot_node_delete(&mut engine, child, false).unwrap();

        let root_snap = read_snapshot_value(&engine, root).unwrap();
        assert_eq!(
            root_snap.children,
            [0, 0],
            "parent children should be cleared after leaf delete"
        );
        assert!(
            read_snapshot_value(&engine, child).is_none(),
            "leaf should be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_interior() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        // interior 节点位于 root 和 leaf 之间，需要手动更新 leaf.parent
        let interior = create_interior_snapshot(&mut engine, root, [leaf, 0], 0, 0);
        {
            let mut snap = read_snapshot_value(&engine, leaf).unwrap();
            snap.parent = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_snapshot_node_delete(&mut engine, interior, true).unwrap();

        let leaf_snap = read_snapshot_value(&engine, leaf).unwrap();
        assert_eq!(
            leaf_snap.parent, root,
            "leaf parent re-parented to grandparent (root)"
        );
        assert!(
            read_snapshot_value(&engine, interior).is_none(),
            "interior should be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_two_children() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // extra_child_subvol 创建两个子节点（1变2 语义）
        let _pair = bch2_snapshot_node_create(&mut engine, root, 2, Some(3)).unwrap();

        let result = bch2_snapshot_node_delete(&mut engine, root, false);
        assert!(
            result.is_err(),
            "snapshot with two children cannot be deleted"
        );
    }

    #[test]
    fn test_snapshot_node_delete_root_leaf() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        bch2_snapshot_node_delete(&mut engine, root, false).unwrap();
        assert!(
            read_snapshot_value(&engine, root).is_none(),
            "root leaf should be deletable"
        );
    }

    #[test]
    fn test_snapshot_node_delete_root_interior() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        bch2_snapshot_node_delete(&mut engine, root, true).unwrap();

        let leaf_snap = read_snapshot_value(&engine, leaf).unwrap();
        assert_eq!(leaf_snap.parent, 0, "leaf becomes new root");
        assert_eq!(leaf_snap.depth, 1, "new root depth is 1");
        assert!(read_snapshot_value(&engine, root).is_none(), "root deleted");
    }

    #[test]
    fn test_snapshot_node_delete_nonexistent() {
        let mut engine = make_engine();
        let result = bch2_snapshot_node_delete(&mut engine, 999, false);
        assert!(result.is_err(), "nonexistent should error");
    }

    // ─── 死快照批量清理测试 ───

    #[test]
    fn test_delete_dead_no_snapshots() {
        let mut engine = make_engine();
        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert!(skipped.is_empty(), "no snapshots → no skipped");
    }

    #[test]
    fn test_delete_dead_no_dead_snapshots() {
        let mut engine = make_engine();
        let _root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert!(skipped.is_empty(), "no deleted → nothing to clean");
    }

    // ─── 辅助：创建 interior 快照（无 SUBVOL 标志） ───

    fn create_interior_snapshot(
        engine: &mut crate::btree::BtreeEngine,
        parent: u32,
        children: [u32; 2],
        subvol: u32,
        tree: u32,
    ) -> u32 {
        let parent_snap = read_snapshot_value(engine, parent).unwrap();
        let depth = parent_snap.depth + 1;
        let id = get_next_snapshot_id(engine);
        let gap = parent - id;
        let bitmap = if gap >= 128 {
            0
        } else {
            parent_snap.is_ancestor.wrapping_shl(gap) | (1u128 << (gap - 1))
        };
        let mut interior =
            SnapshotT::new_interior(parent, children, tree, depth, current_timestamp());
        interior.is_ancestor = bitmap;
        // interior 节点不持有子卷引用，但可以设置 subvol 为被引用的 leaf ID
        let mut interior_with_subvol = interior;
        interior_with_subvol.subvol = subvol;
        let bytes = bincode::serialize(&interior_with_subvol).unwrap();
        let entry = crate::btree::key::BtreeEntry::raw(
            crate::btree::key::Bpos::new(0, 0, id),
            crate::btree::key::KeyType::Normal,
            bytes,
        );
        engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        id
    }

    #[test]
    fn test_delete_dead_single_dead_leaf() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let interior = create_interior_snapshot(&mut engine, root, [leaf, 0], leaf, 0);

        // 标记 interior 为已删除
        bch2_snapshot_node_set_deleted(&mut engine, interior).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert_eq!(
            skipped.len(),
            1,
            "interior has volume ref leaf → should skip"
        );
        assert_eq!(skipped[0], interior, "interior should be in skip list");
    }

    #[test]
    fn test_delete_dead_skips_volume_ref() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let _child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();

        // 标记 root 为已删除——但 root 有 SUBVOL（被 volume 引用），应跳过
        bch2_snapshot_node_set_deleted(&mut engine, root).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert_eq!(skipped.len(), 1, "root should be skipped");
        assert_eq!(skipped[0], root, "skipped should be root");

        if let Some(entry) = engine.get_entry_raw(BtreeId::Snapshots, Bpos::new(0, 0, root)) {
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => panic!("expected Raw value"),
            };
            let snap: SnapshotT = bincode::deserialize(&bytes).unwrap();
            assert!(snap.deleted, "root should still be marked deleted");
        }
    }

    #[test]
    fn test_delete_dead_removes_interior_without_subvol() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf1 = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let leaf2 = bch2_snapshot_node_create(&mut engine, root, 3, None).unwrap();

        // 创建一个 interior 节点链接两个 leaf
        let interior = create_interior_snapshot(&mut engine, root, [leaf1, leaf2], 0, 0);
        // 更新 leaf 的 parent 为 interior
        let update_leaf = |engine: &mut crate::btree::BtreeEngine, id: u32, parent: u32| {
            let mut snap = read_snapshot_value(engine, id).unwrap();
            snap.parent = parent;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = crate::btree::key::BtreeEntry::raw(
                crate::btree::key::Bpos::new(0, 0, id),
                crate::btree::key::KeyType::Normal,
                bytes,
            );
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        };
        update_leaf(&mut engine, leaf1, interior);
        update_leaf(&mut engine, leaf2, interior);

        // 标记 interior 为已删除（无 SUBVOL）
        bch2_snapshot_node_set_deleted(&mut engine, interior).unwrap();

        // leaf1, leaf2 有 SUBVOL → 跳过
        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert_eq!(skipped.len(), 1, "subtree has volume ref leaves → skip");
    }

    #[test]
    fn test_delete_dead_idempotent() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // interior 无 SUBVOL
        let interior = create_interior_snapshot(&mut engine, root, [0, 0], 0, 0);

        bch2_snapshot_node_set_deleted(&mut engine, interior).unwrap();

        // 第一次清理：interior 无 SUBVOL 引用 → 应被删除
        let skipped1 = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert!(skipped1.is_empty(), "first cleanup should delete interior");

        // 第二次清理，应空运行不 panic
        let skipped2 = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert!(skipped2.is_empty(), "second cleanup should be no-op");
    }

    #[test]
    fn test_delete_dead_interior_preserves_children() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // root → a → b → leaf，a 和 b 无 subvol
        let a = bch2_snapshot_node_create(&mut engine, root, 0, None).unwrap();
        {
            let mut snap = read_snapshot_value(&engine, a).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, a), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let b = bch2_snapshot_node_create(&mut engine, a, 0, None).unwrap();
        {
            let mut snap = read_snapshot_value(&engine, b).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            snap.subvol = 0;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, b), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        let leaf = bch2_snapshot_node_create(&mut engine, b, 0, None).unwrap();
        // 清除 leaf 的 SUBVOL flag（subvol=0 时默认仍带 SUBVOL）
        {
            let mut snap = read_snapshot_value(&engine, leaf).unwrap();
            snap.flags = BchSnapshotFlags::empty();
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        // 标记 b 为已删除
        bch2_snapshot_node_set_deleted(&mut engine, b).unwrap();

        let skipped = bch2_delete_dead_snapshots(&mut engine).unwrap();
        assert!(skipped.is_empty(), "no volume refs, nothing skipped");

        // b 应被删除
        assert!(
            read_snapshot_value(&engine, b).is_none(),
            "b should be deleted"
        );

        // leaf 应存活，且 depth/skip 已调整
        let leaf_snap = read_snapshot_value(&engine, leaf).unwrap();
        assert_eq!(leaf_snap.depth, 3, "leaf depth should reduce from 4 to 3");
        // leaf 的所有非零 skip 不应指向已删节点 b
        for &s in &leaf_snap.skip {
            if s != 0 {
                assert_ne!(s, b, "skip should not reference deleted node b");
            }
        }

        // a 应存活，且不是 deleted
        let a_snap = read_snapshot_value(&engine, a).unwrap();
        assert!(!a_snap.deleted, "a should not be deleted");
    }

    // ─── bch2_delete_dead_interior_snapshots 测试 ───

    #[test]
    fn test_delete_dead_interior_single_child() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let interior = create_interior_snapshot(&mut engine, root, [leaf, 0], 0, 0);
        // 更新 leaf parent 指向 interior
        {
            let mut snap = read_snapshot_value(&engine, leaf).unwrap();
            snap.parent = interior;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        // 设置 NO_KEYS flag
        {
            let mut snap = read_snapshot_value(&engine, interior).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, interior), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut engine).unwrap();

        // interior 应被删除
        assert!(read_snapshot_value(&engine, interior).is_none());
        // leaf 应存活
        assert!(read_snapshot_value(&engine, leaf).is_some());
    }

    #[test]
    fn test_delete_dead_interior_skips_two_children() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf1 = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let leaf2 = bch2_snapshot_node_create(&mut engine, root, 3, None).unwrap();
        let interior = create_interior_snapshot(&mut engine, root, [leaf1, leaf2], 0, 0);
        {
            let mut snap = read_snapshot_value(&engine, interior).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, interior), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut engine).unwrap();

        // 两个孩子时不应删除
        assert!(read_snapshot_value(&engine, interior).is_some());
    }

    #[test]
    fn test_delete_dead_interior_skips_no_no_keys() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let interior = create_interior_snapshot(&mut engine, root, [leaf, 0], 0, 0);
        bch2_delete_dead_interior_snapshots(&mut engine).unwrap();
        assert!(read_snapshot_value(&engine, interior).is_some());
    }

    #[test]
    fn test_delete_dead_interior_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let leaf = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        // root → i1 → i2 → leaf
        let i1 = create_interior_snapshot(&mut engine, root, [leaf, 0], 0, 0);
        let i2 = create_interior_snapshot(&mut engine, i1, [leaf, 0], 0, 0);
        // 更新 leaf parent → i2, i2 parent → i1
        {
            let mut snap = read_snapshot_value(&engine, leaf).unwrap();
            snap.parent = i2;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, leaf), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        for &id in &[i2, i1] {
            let mut snap = read_snapshot_value(&engine, id).unwrap();
            snap.flags.insert(BchSnapshotFlags::NO_KEYS);
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, id), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        bch2_delete_dead_interior_snapshots(&mut engine).unwrap();

        assert!(read_snapshot_value(&engine, i1).is_none());
        assert!(read_snapshot_value(&engine, i2).is_none());
        assert!(read_snapshot_value(&engine, leaf).is_some());
    }

    // ─── check_should_delete_snapshot 测试 ───

    #[test]
    fn test_check_should_delete_deleted_flag() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.deleted = true;
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_will_delete() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags.insert(BchSnapshotFlags::WILL_DELETE);
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_leaf_no_subvol() {
        let snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Leaf)
        );
    }

    #[test]
    fn test_check_should_delete_interior_no_keys() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.children = [3, 0]; // has children, not a leaf
        snap.flags.insert(BchSnapshotFlags::NO_KEYS);
        assert_eq!(
            check_should_delete_snapshot(&snap),
            Some(DeadSnapshotType::Interior)
        );
    }

    #[test]
    fn test_check_should_delete_alive_has_subvol() {
        let mut snap = SnapshotT::new_leaf(1, 1, 0, 2, 0);
        snap.flags = BchSnapshotFlags::SUBVOL;
        assert_eq!(check_should_delete_snapshot(&snap), None);
    }

    // ─── bch2_check_snapshot_needs_deletion 测试 ───

    #[test]
    fn test_check_needs_deletion_will_delete() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags.insert(BchSnapshotFlags::WILL_DELETE);
        assert!(bch2_check_snapshot_needs_deletion(&snap));
    }

    #[test]
    fn test_check_needs_deletion_single_child() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.children = [2, 0];
        assert!(bch2_check_snapshot_needs_deletion(&snap));
    }

    #[test]
    fn test_check_needs_deletion_no_keys() {
        let mut snap = SnapshotT::new_leaf(1, 0, 0, 2, 0);
        snap.flags.insert(BchSnapshotFlags::NO_KEYS);
        assert!(
            !bch2_check_snapshot_needs_deletion(&snap),
            "NO_KEYS handled by interior delete"
        );
    }

    #[test]
    fn test_check_needs_deletion_normal() {
        let snap = SnapshotT::new_leaf(1, 1, 0, 2, 0);
        assert!(!bch2_check_snapshot_needs_deletion(&snap));
    }

    // ─── bch2_snapshot_skiplist_get 测试 ───

    #[test]
    fn test_skiplist_get_root_returns_none() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let result = bch2_snapshot_skiplist_get(&engine, root);
        assert_eq!(result, Some([0, 0, 0]), "root snapshot has no parent");
    }

    #[test]
    fn test_skiplist_get_child_returns_parent() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        let result = bch2_snapshot_skiplist_get(&engine, child);
        let skiplist = result.expect("skiplist should be Some for non-root snapshot");
        // bcachefs 随机深度跳跃：结果可能是 child 或 root 的任意组合，必须已排序
        assert!(
            skiplist[0] <= skiplist[1] && skiplist[1] <= skiplist[2],
            "depth-2 skiplist should be sorted"
        );
        // 所有值必须是 child 或 root（child depth=2 时只有这两层祖先）
        for &entry in &skiplist {
            assert!(entry == child || entry == root || entry == 0,
                "each skip entry must be a valid ancestor (child={child}, root={root}), got {entry}");
        }
    }

    // ─── bch2_snapshot_is_ancestor_btree 测试 ───

    #[test]
    fn test_bch2_is_ancestor_btree_self() {
        let engine = make_engine();
        assert!(bch2_snapshot_is_ancestor_btree(&engine, 42, 42));
    }

    #[test]
    fn test_bch2_is_ancestor_btree_chain() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        assert!(bch2_snapshot_is_ancestor_btree(&engine, child, root));
        assert!(!bch2_snapshot_is_ancestor_btree(&engine, root, child));
    }

    // ─── Layer 3: bch2_check_key_has_snapshot ───

    #[test]
    fn test_check_key_has_snapshot_valid() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let table = SnapshotTable::build(&engine);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, root),
            CheckKeySnapshotResult::Valid,
            "live snapshot should be valid"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_deleted() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 1, None).unwrap();
        bch2_snapshot_node_set_deleted(&mut engine, child).unwrap();
        let table = SnapshotTable::build(&engine);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, child),
            CheckKeySnapshotResult::ShouldDelete,
            "key in deleted snapshot should need deletion"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_missing() {
        let engine = make_engine();
        let table = SnapshotTable::build(&engine);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, 42),
            CheckKeySnapshotResult::Missing,
            "nonexistent ID should be missing"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_zero_id() {
        let engine = make_engine();
        let table = SnapshotTable::build(&engine);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, 0),
            CheckKeySnapshotResult::Valid,
            "snapshot_id=0 should always be valid"
        );
    }

    #[test]
    fn test_check_key_has_snapshot_live_after_rebuild() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // 删除后重建表，确认状态正确
        bch2_snapshot_node_set_deleted(&mut engine, root).unwrap();
        let table = SnapshotTable::build(&engine);
        assert_eq!(
            bch2_check_key_has_snapshot(&table, root),
            CheckKeySnapshotResult::ShouldDelete,
            "root was deleted"
        );
    }

    // ─── Layer 3: bch2_reconstruct_snapshots ───

    #[test]
    fn test_reconstruct_empty_nothing_to_do() {
        let mut engine = make_engine();
        bch2_reconstruct_snapshots(&mut engine).unwrap();
        let table = SnapshotTable::build(&engine);
        assert!(table.get(u32::MAX).is_none(), "no snapshots should exist");
    }

    #[test]
    fn test_reconstruct_missing_snapshot_from_extents() {
        let mut engine = make_engine();
        let snap_id = u32::MAX - 10;
        // 在 Extents btree 中创建一个引用缺失 snapshot 的条目
        let extent_entry = BtreeEntry::raw(
            Bpos::new(1, 100, snap_id),
            KeyType::Normal,
            vec![1, 2, 3, 4],
        );
        engine.insert_entry_raw(BtreeId::Extents, extent_entry, 0);

        // 运行重建
        bch2_reconstruct_snapshots(&mut engine).unwrap();

        // 验证 snapshot 已被创建
        let snap = read_snapshot_value(&engine, snap_id).expect("snapshot should be reconstructed");
        assert_eq!(snap.parent, 0, "reconstructed snapshot should be root");
        assert_eq!(snap.depth, 1, "reconstructed snapshot depth should be 1");
        // 应有一个对应的 SnapshotTree 条目
        let tree_val =
            read_snapshot_tree_value(&engine, 1).expect("SnapshotTree entry 1 should exist");
        assert_eq!(
            tree_val.root_snapshot, snap_id,
            "SnapshotTree root should point to the reconstructed snapshot"
        );
    }

    #[test]
    fn test_reconstruct_multiple_missing_ids() {
        let mut engine = make_engine();
        // 在多个 btree 中引用不同的缺失 snapshot ID
        // 使用接近 u32::MAX 的 ID（bcachefs 分配惯例）
        let ids = [u32::MAX - 10, u32::MAX - 20, u32::MAX - 30];
        let entries = [
            (BtreeId::Extents, Bpos::new(1, 100, ids[0])),
            (BtreeId::Extents, Bpos::new(1, 200, ids[1])),
            (BtreeId::Subvolumes, Bpos::new(2, 0, ids[2])),
        ];
        for (btree_id, pos) in &entries {
            let entry = BtreeEntry::raw(*pos, KeyType::Normal, vec![1, 2, 3]);
            engine.insert_entry_raw(*btree_id, entry, 0);
        }

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        for &snap_id in &ids {
            assert!(
                read_snapshot_value(&engine, snap_id).is_some(),
                "snapshot {} should be reconstructed",
                snap_id
            );
        }
    }

    #[test]
    fn test_reconstruct_with_existing_snapshots() {
        let mut engine = make_engine();
        // 已有部分快照
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        let missing_id = u32::MAX - 50;
        // 外加一个缺失的引用
        let extent_entry = BtreeEntry::raw(
            Bpos::new(1, 100, missing_id),
            KeyType::Normal,
            vec![1, 2, 3],
        );
        engine.insert_entry_raw(BtreeId::Extents, extent_entry, 0);

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        // 已有的仍然存在
        assert!(
            read_snapshot_value(&engine, root).is_some(),
            "existing root should remain"
        );
        assert!(
            read_snapshot_value(&engine, child).is_some(),
            "existing child should remain"
        );
        // 缺失的被重建
        assert!(
            read_snapshot_value(&engine, missing_id).is_some(),
            "missing snapshot should be reconstructed"
        );
    }

    #[test]
    fn test_reconstruct_all_existing_nothing_added() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();

        // 所有被引用的 snapshot ID 都已存在
        let snap_count_before = {
            let btree = engine.get(BtreeId::Snapshots);
            let mut count = 0;
            btree.for_each_entry(|_| count += 1);
            count
        };

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        let snap_count_after = {
            let btree = engine.get(BtreeId::Snapshots);
            let mut count = 0;
            btree.for_each_entry(|_| count += 1);
            count
        };

        assert_eq!(
            snap_count_before, snap_count_after,
            "no new snapshots should be added"
        );
    }

    #[test]
    fn test_reconstruct_creates_tree_entry() {
        let mut engine = make_engine();
        let snap_id = u32::MAX - 50;
        // 只有 extents 引用，没有 SnapshotTrees
        let entry = BtreeEntry::raw(Bpos::new(1, 100, snap_id), KeyType::Normal, vec![1, 2, 3]);
        engine.insert_entry_raw(BtreeId::Extents, entry, 0);

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        // 验证 SnapshotTree 条目被创建
        let tree_val =
            read_snapshot_tree_value(&engine, 1).expect("SnapshotTree entry should be created");
        assert_eq!(
            tree_val.root_snapshot, snap_id,
            "tree root should point to reconstructed snapshot"
        );
    }

    #[test]
    fn test_reconstruct_no_duplicate_trees() {
        let mut engine = make_engine();
        // 两个缺失的快照（使用接近 u32::MAX 的 ID）
        let ids = [u32::MAX - 50, u32::MAX - 100];
        let entries = [
            (BtreeId::Extents, Bpos::new(1, 100, ids[0])),
            (BtreeId::Extents, Bpos::new(2, 200, ids[1])),
        ];
        for (btree_id, pos) in &entries {
            let entry = BtreeEntry::raw(*pos, KeyType::Normal, vec![1, 2, 3]);
            engine.insert_entry_raw(*btree_id, entry, 0);
        }

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        // 应该有两个独立的 tree 条目
        let tree_count = {
            let btree = engine.get(BtreeId::SnapshotTrees);
            let mut count = 0;
            btree.for_each_entry(|_| count += 1);
            count
        };
        assert_eq!(
            tree_count, 2,
            "two missing snapshots should create two trees"
        );
    }

    // ─── Layer 3: 表集成 ───

    #[test]
    fn test_id_state_methods() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let table = SnapshotTable::build(&engine);

        assert_eq!(table.id_state(root), SnapshotIdState::Live);
        assert_eq!(table.id_state(0), SnapshotIdState::Empty);
        assert_eq!(table.id_state(9999), SnapshotIdState::Empty);

        // 删除后重建表
        bch2_snapshot_node_set_deleted(&mut engine, root).unwrap();
        let table2 = SnapshotTable::build(&engine);
        assert_eq!(table2.id_state(root), SnapshotIdState::Deleted);
    }

    #[test]
    fn test_snapshots_read_after_reconstruct() {
        let mut engine = make_engine();
        let snap_id = u32::MAX - 5;
        // 引用缺失的 snapshot
        let entry = BtreeEntry::raw(Bpos::new(1, 100, snap_id), KeyType::Normal, vec![1, 2, 3]);
        engine.insert_entry_raw(BtreeId::Extents, entry, 0);

        bch2_reconstruct_snapshots(&mut engine).unwrap();

        // 验证 bch2_snapshots_read 能正确加载重建后的数据
        let (table, tree_table) = crate::snap::table::bch2_snapshots_read(&engine);
        assert!(
            table.exists(snap_id),
            "reconstructed snapshot should be in table"
        );
        assert!(
            tree_table.get(1).is_some(),
            "reconstructed tree should be in tree table"
        );
    }
}
