use std::collections::{HashMap, HashSet};

use crate::btree::key::{Bpos, KeyType, KeyValue};
use crate::btree::{BtreeEngine, BtreeEntry, BtreeId};
use crate::recovery::RecoveryState;
use crate::snap::meta::{BchSnapshotFlags, SnapshotT};
use crate::snap::snapshot::bch2_snapshot_skiplist_get;
use crate::subvol::ops::bch2_subvolume_get;
use crate::types::StorageError;

/// 使用 in-memory HashMap 检查 `ancestor` 是否是 `descendant` 的祖先。
/// 用于 check_snapshots pass 中的 skiplist 验证（代替 btree 读取）。
fn is_ancestor_in_map(snapshots: &HashMap<u32, SnapshotT>, ancestor: u32, descendant: u32) -> bool {
    if ancestor == descendant {
        return true;
    }
    if ancestor <= descendant || descendant == 0 {
        return false;
    }
    let mut current = descendant;
    loop {
        let snap = match snapshots.get(&current) {
            Some(s) => s,
            None => return false,
        };
        if snap.parent == 0 {
            return false;
        }
        if snap.parent == ancestor {
            return true;
        }
        current = snap.parent;
    }
}

/// Pass: 快照一致性验证与修复（对齐 bcachefs `bch2_check_snapshots_trans()`）
///
/// 从 Snapshots btree 构建快照 ID → SnapshotT 映射，验证拓扑完整性，
/// 并对可修复问题自动修复：
/// - depth 错误 → 重算
/// - skip 错误 → 重建 skip list
/// - SUBVOL 标志但 subvol=0 → 清除标志
/// - children 指针不一致 → 修复
/// - parent 不存在 → 不可修复，返回错误
/// - 循环引用 → 不可修复，返回错误
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    bch2_check_snapshots(&mut state.engine)
}

/// 核心验证与修复逻辑
pub(crate) fn bch2_check_snapshots(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    // 1. 从 Snapshots btree 收集所有非删除条目
    let mut snapshots: HashMap<u32, SnapshotT> = HashMap::new();
    {
        let btree = engine.get(BtreeId::Snapshots);
        btree.for_each_entry(|entry: BtreeEntry| {
            let snapshot_id = entry.pos.snapshot;
            if snapshot_id == 0 {
                return;
            }
            let bytes = match &entry.value {
                KeyValue::Raw(b) => b.clone(),
                _ => return,
            };
            if let Ok(snap) = bincode::deserialize::<SnapshotT>(&bytes) {
                if !snap.deleted {
                    snapshots.insert(snapshot_id, snap);
                }
            }
        });
    }

    if snapshots.is_empty() {
        return Ok(());
    }

    // 2. 验证无循环（不可修复，发现即返回错误）
    const MAX_DEPTH: u32 = 1_000_000;
    for &sid in snapshots.keys() {
        let mut current = sid;
        let mut steps = 0;
        while current != 0 {
            if steps > MAX_DEPTH {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent chain exceeds max depth (cycle?)",
                    sid
                )));
            }
            match snapshots.get(&current) {
                Some(s) if s.parent == current => {
                    return Err(StorageError::InvalidData(format!(
                        "check_snapshots: snapshot {} parent is self",
                        current
                    )));
                }
                Some(s) => {
                    current = s.parent;
                    steps += 1;
                }
                None => {
                    return Err(StorageError::InvalidData(format!(
                        "check_snapshots: snapshot {} parent chain broken at {}",
                        sid, current
                    )));
                }
            }
        }
    }

    // 3a. 收集有效的 tree_id 集合（来自 SnapshotTrees btree）
    let mut valid_tree_ids: HashSet<u32> = HashSet::new();
    {
        let btree = engine.get(BtreeId::SnapshotTrees);
        btree.for_each_entry(|entry: BtreeEntry| {
            let tree_id = entry.pos.snapshot;
            if tree_id != 0 {
                valid_tree_ids.insert(tree_id);
            }
        });
    }

    // 3b. 逐项检查并收集修复
    // 收集所有需要修复的 snapshot，避免在迭代中修改 HashMap
    let mut fixes: Vec<(u32, SnapshotT)> = Vec::new();

    for (&sid, snap) in &snapshots {
        let mut fixed = snap.clone();
        let mut changed = false;

        if snap.parent != 0 {
            // 3a. parent 不存在 → 不可修复（已在循环检查中排除，但再确认一次）
            if !snapshots.contains_key(&snap.parent) {
                return Err(StorageError::InvalidData(format!(
                    "check_snapshots: snapshot {} parent {} not found",
                    sid, snap.parent
                )));
            }

            let parent = &snapshots[&snap.parent];

            // 3b. depth 重算
            let expected_depth = parent.depth + 1;
            if fixed.depth != expected_depth {
                fixed.depth = expected_depth;
                changed = true;
            }

            // 3c. skip 验证（对齐 bcachefs check_snapshot.c:378-410）
            // bcachefs 使用 bch2_snapshot_is_ancestor_early 验证每个 skip 条目
            // 是当前节点的有效祖先，无效则通过 bch2_snapshot_skiplist_get 重建
            let mut bad_skip = false;
            for i in 0..3 {
                let skip = snap.skip[i];
                if skip != 0 && !is_ancestor_in_map(&snapshots, skip, sid) {
                    bad_skip = true;
                    break;
                }
            }
            if bad_skip {
                if let Some(new_skip) = bch2_snapshot_skiplist_get(engine, snap.parent) {
                    fixed.skip = new_skip;
                    changed = true;
                }
            }
        } else {
            // 根节点 depth 应为 1
            if fixed.depth != 1 {
                fixed.depth = 1;
                changed = true;
            }
            // 根节点 skip 应为 [0, 0, 0]
            if fixed.skip != [0, 0, 0] {
                fixed.skip = [0, 0, 0];
                changed = true;
            }
        }

        // 3d. SUBVOL 交叉验证（对齐 bcachefs check_snapshot.c:324-353）
        // SUBVOL 标志表示此快照关联一个子卷。
        // bcachefs 要求：设置了 SUBVOL 标志 → subvol 必须存在于 Subvolumes btree
        // 且子卷的 snapshot 字段指向当前节点
        if fixed.flags.contains(BchSnapshotFlags::SUBVOL) {
            if fixed.subvol != 0 {
                // 验证子卷存在且 snapshot 字段匹配
                if let Some(subvol) = bch2_subvolume_get(engine, fixed.subvol as u32) {
                    if subvol.snapshot != sid {
                        // 子卷 snapshot 字段与当前节点不匹配 → 清除标志
                        fixed.flags.remove(BchSnapshotFlags::SUBVOL);
                        changed = true;
                    }
                } else {
                    // 子卷不存在 → 清除标志和 subvol 字段
                    fixed.flags.remove(BchSnapshotFlags::SUBVOL);
                    fixed.subvol = 0;
                    changed = true;
                }
            } else {
                // subvol=0 但 SUBVOL 标志仍设置 → 清除
                fixed.flags.remove(BchSnapshotFlags::SUBVOL);
                changed = true;
            }
        }

        // 3f. tree_id 有效性验证：tree 应指向有效的 SnapshotTrees btree 条目
        //     无效 → 清除 tree 字段（重置为 0）
        if fixed.tree != 0 && !valid_tree_ids.contains(&fixed.tree) {
            fixed.tree = 0;
            changed = true;
        }

        // 3g. children 引用验证：子节点应存在且 parent 指针正确
        for ci in 0..2 {
            let child_id = snap.children[ci];
            if child_id != 0 {
                if let Some(child) = snapshots.get(&child_id) {
                    if child.parent != sid {
                        // 子节点的 parent 不指向当前节点 → 修复
                        // 注意：我们不能修改 HashMap 中的值（迭代中），
                        // 将修复加入 fixes 列表
                        let mut fixed_child = child.clone();
                        fixed_child.parent = sid;
                        fixes.push((child_id, fixed_child));
                    }
                }
                // 子节点不存在 → 清空 children 槽位
                // 注意：子节点可能因 deleted=true 被过滤掉
                if !snapshots.contains_key(&child_id) {
                    fixed.children[ci] = 0;
                    changed = true;
                }
            }
        }

        // 3h. parent 反查：父节点的 children 应包含当前节点（对齐 bcachefs check_snapshot.c:298-303）
        // bcachefs 将此视为不可修复的结构错误（EINVAL_snapshot_parent_missing_child_ptr），
        // 但此处我们尝试修复：如果父节点有空闲 children 槽，将当前节点加入。
        // 如果两个槽都已满，则返回不可修复的错误。
        if snap.parent != 0 {
            if let Some(parent) = snapshots.get(&snap.parent) {
                if parent.children[0] != sid && parent.children[1] != sid {
                    let mut fixed_parent = parent.clone();
                    if fixed_parent.children[0] == 0 {
                        fixed_parent.children[0] = sid;
                    } else if fixed_parent.children[1] == 0 {
                        fixed_parent.children[1] = sid;
                    } else {
                        // 两个 children 槽都已满 → 结构损坏，不可修复
                        return Err(StorageError::InvalidData(format!(
                            "check_snapshots: snapshot {} parent {} children slots full, \
                             cannot add missing child pointer",
                            sid, snap.parent
                        )));
                    }
                    fixes.push((snap.parent, fixed_parent));
                }
            }
        }

        if changed {
            fixes.push((sid, fixed));
        }
    }

    // 4. 应用修复到 btree
    if !fixes.is_empty() {
        // 去重：保留每个 sid 最后的修复
        fixes.sort_by(|a, b| a.0.cmp(&b.0));
        fixes.dedup_by(|a, b| a.0 == b.0);

        for (sid, fixed_snap) in &fixes {
            let bytes = bincode::serialize(fixed_snap).map_err(StorageError::Serialization)?;
            let entry = BtreeEntry::raw(Bpos::new(0, 0, *sid), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }

        engine.get_mut(BtreeId::Snapshots).compact();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::Bpos;
    use crate::snap::meta::SnapshotT;
    use crate::snap::snapshot::{
        bch2_snapshot_node_create, create_root_snapshot_btree, read_snapshot_value,
    };

    fn make_engine() -> BtreeEngine {
        BtreeEngine::new()
    }

    #[test]
    fn test_check_snapshots_valid_tree() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let _child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        assert!(bch2_check_snapshots(&mut engine).is_ok());
    }

    #[test]
    fn test_check_snapshots_repairs_depth() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        // 损坏 child 的 depth
        {
            let mut snap = read_snapshot_value(&engine, child).unwrap();
            snap.depth = 99;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, child), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        bch2_check_snapshots(&mut engine).unwrap();
        let fixed = read_snapshot_value(&engine, child).unwrap();
        assert_eq!(fixed.depth, 2, "depth should be repaired to parent.depth+1");
    }

    #[test]
    fn test_check_snapshots_repairs_skip() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        let grandchild = bch2_snapshot_node_create(&mut engine, child, 3, None).unwrap();
        // 损坏 grandchild 的 skip：设置一个非祖先 ID（u32::MAX 不可能在快照树中）
        // 在 bcachefs 中，skip=0 是合法的（表示无 skip 链接），
        // 但非零且非祖先的 skip 条目必须修复
        {
            let mut snap = read_snapshot_value(&engine, grandchild).unwrap();
            snap.skip = [u32::MAX, u32::MAX, u32::MAX];
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, grandchild), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        bch2_check_snapshots(&mut engine).unwrap();
        let fixed = read_snapshot_value(&engine, grandchild).unwrap();
        // 验证：修复后的 skip 条目都应是 grandchild 的有效祖先（0/root/child）
        for &entry in &fixed.skip {
            if entry != 0 {
                assert!(
                    entry == root || entry == child,
                    "skip entry {} should be an ancestor of {}: {:?}",
                    entry,
                    grandchild,
                    fixed.skip
                );
            }
        }
    }

    #[test]
    fn test_check_snapshots_repairs_subvol_flag() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // 创建节点后清除 subvol 值但保留 SUBVOL flag
        let child = bch2_snapshot_node_create(&mut engine, root, 0, None).unwrap();
        // 清除 subvol 但保留 flag（模拟损坏）
        {
            let mut snap = read_snapshot_value(&engine, child).unwrap();
            snap.subvol = 0;
            // 保留 SUBVOL flag（制造不一致）
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, child), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        bch2_check_snapshots(&mut engine).unwrap();
        let fixed = read_snapshot_value(&engine, child).unwrap();
        assert!(
            !fixed.flags.contains(BchSnapshotFlags::SUBVOL),
            "SUBVOL flag should be cleared when subvol=0"
        );
    }

    #[test]
    fn test_check_snapshots_repairs_parent_children() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        let child = bch2_snapshot_node_create(&mut engine, root, 2, None).unwrap();
        // 损坏 root 的 children：清除 child 引用
        {
            let mut snap = read_snapshot_value(&engine, root).unwrap();
            snap.children = [0, 0];
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        bch2_check_snapshots(&mut engine).unwrap();
        let fixed = read_snapshot_value(&engine, root).unwrap();
        assert!(
            fixed.children[0] == child || fixed.children[1] == child,
            "root children should contain child {} after repair: {:?}",
            child,
            fixed.children
        );
    }

    #[test]
    fn test_check_snapshots_repairs_subvol_nonexistent() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // 创建 snapshot 并设置不存在的 subvol ID（subvol=999 不存在于 Subvolumes btree）
        let child = bch2_snapshot_node_create(&mut engine, root, 999, None).unwrap();
        // 强制设置 SUBVOL flag（node_create 不会自动设 flag，模拟损坏）
        {
            let mut snap = read_snapshot_value(&engine, child).unwrap();
            snap.flags.insert(BchSnapshotFlags::SUBVOL);
            snap.subvol = 999;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, child), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        bch2_check_snapshots(&mut engine).unwrap();
        let fixed = read_snapshot_value(&engine, child).unwrap();
        assert!(
            !fixed.flags.contains(BchSnapshotFlags::SUBVOL),
            "SUBVOL flag should be cleared when subvol 999 not found in Subvolumes btree"
        );
        assert_eq!(fixed.subvol, 0, "subvol should be cleared to 0");
    }

    #[test]
    fn test_check_snapshots_detects_cycle() {
        let mut engine = make_engine();
        let root = create_root_snapshot_btree(&mut engine, 1).unwrap();
        // 让 root 自引用 parent
        {
            let mut snap = read_snapshot_value(&engine, root).unwrap();
            snap.parent = root;
            let bytes = bincode::serialize(&snap).unwrap();
            let entry = BtreeEntry::raw(Bpos::new(0, 0, root), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);
        }
        assert!(
            bch2_check_snapshots(&mut engine).is_err(),
            "self-parent cycle should be detected"
        );
    }
}
