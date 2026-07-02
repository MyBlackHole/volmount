//! Subvolume — 子卷管理器（BtreeEngine 集成）
//!
//! 使用 BtreeEngine::Subvolumes btree 持久化存储 BchSubvolume。
//! 函数名与 bcachefs API 对齐：`bch2_subvolume_*` / `bch2_subvol_*`。

use crate::btree::key::{BtreeEntry, BtreeKey, KeyType, KeyValue};
use crate::btree::{BatchEntry, Bpos, BtreeEngine, BtreeId};
use crate::snap::meta::{SnapshotT, SnapshotTreeT};
use crate::snap::snapshot::{
    bch2_snapshot_node_create, bch2_snapshot_node_set_deleted, create_root_snapshot_btree,
    read_snapshot_value,
};
use crate::types::StorageError;

use super::types::{BchSubvolume, BCACHEFS_ROOT_INO, BCACHEFS_ROOT_SUBVOL};

/// 重置子卷 ID 分配器（用于测试 — 现在仅重置测试内建的计数器）
/// 分配一个新的子卷 ID（使用引擎本地计数器）
fn allocate_subvol_id(engine: &mut BtreeEngine) -> u32 {
    let id = engine.subvol_id_counter;
    engine.subvol_id_counter += 1;
    id
}

// ─── 子卷创建 / 快照 ───

/// 创建新子卷，自动创建根快照节点，返回分配的 subvol_id
///
/// 对齐 bcachefs `bch2_subvolume_create()`。
/// 流程：
/// 1. 在 Snapshots btree 中创建根快照节点（subvol=0 临时）
/// 2. 在 Subvolumes btree 中创建子卷条目
/// 3. 更新快照节点的 subvol 指针到真实的 subvol_id
pub fn bch2_subvolume_create(
    engine: &mut BtreeEngine,
    inode: u64,
    size: u64,
    created_at: i64,
) -> Result<u32, StorageError> {
    // 1. 创建根快照节点（subvol=0 临时，后续更新）
    let root_snap = create_root_snapshot_btree(engine, 0)?;

    // 2. 分配子卷 ID 并创建子卷条目
    let subvol_id = allocate_subvol_id(engine);
    let sv = BchSubvolume::new(root_snap, inode, size, created_at as u64);
    let pos = Bpos::new(0, subvol_id as u64, 0);
    let entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::Raw(sv.to_bytes()));
    engine.insert_entry_raw(BtreeId::Subvolumes, entry, 0);

    // 触发器校验
    bch2_subvolume_trigger(engine, subvol_id, &sv)?;

    // 注册 inode 映射
    engine.register_ino_map(inode, subvol_id);

    // 3. 更新快照节点的 subvol 指针
    if let Some(mut snap_val) = read_snapshot_value(engine, root_snap) {
        snap_val.subvol = subvol_id;
        if let Ok(bytes) = bincode::serialize(&snap_val) {
            let snap_entry = BtreeEntry::raw(Bpos::new(0, 0, root_snap), KeyType::Normal, bytes);
            engine.insert_entry_raw(BtreeId::Snapshots, snap_entry, 0);
        }
    }

    Ok(subvol_id)
}

/// 创建快照子卷，自动创建快照节点
///
/// 对齐 bcachefs `bch2_subvolume_create()` 的 snapshot 模式（src_subvolid != 0）。
/// 在 Snapshots btree 中创建子快照节点，再创建对应的子卷条目。
///
/// `parent_snapshot` 从父子卷加载（`parent_subvol` 的 snapshot 字段）。
pub fn bch2_subvolume_snapshot(
    engine: &mut BtreeEngine,
    parent_subvol: u32,
    inode: u64,
    size: u64,
    created_at: i64,
) -> Result<u32, StorageError> {
    let parent = bch2_subvolume_get(engine, parent_subvol)
        .ok_or_else(|| StorageError::NotFound(format!("parent subvolume {parent_subvol}")))?;
    if parent.is_unlinked() {
        return Err(StorageError::NotFound(format!(
            "parent subvolume {parent_subvol} is deleted"
        )));
    }

    // 1. 从父子卷获取其根快照 ID，作为新快照的父快照
    let parent_snapshot = parent.snapshot;

    // 2. 分配新子卷 ID 并使用 bcachefs "1变2" 语义一次性创建两个子快照节点
    //    new_nodes[0] 分配给新快照子卷，new_nodes[1] 替换源子卷的快照指针
    let subvol_id = allocate_subvol_id(engine);
    let new_snap_id =
        bch2_snapshot_node_create(engine, parent_snapshot, subvol_id, Some(parent_subvol))?;
    let src_new_snap_id = {
        let parent_snap = read_snapshot_value(engine, parent_snapshot).ok_or_else(|| {
            StorageError::NotFound(format!("parent snapshot {} disappeared", parent_snapshot))
        })?;
        parent_snap.children[1]
    };

    // 3. 触发器校验：验证父子卷和新子卷的快照引用
    let sv = BchSubvolume::new_snapshot(parent_subvol, new_snap_id, inode, size, created_at as u64);
    bch2_subvolume_trigger(
        engine,
        parent_subvol,
        &bch2_subvolume_get(engine, parent_subvol).unwrap(),
    )?;
    // 注：新子卷尚未写入 btree，其快照引用由 bch2_snapshot_node_create 保障

    // 4. 在一个 batch_write 中原子性完成父子卷更新和新子卷创建（1变2 原子性保障）
    let new_subvol_pos = Bpos::new(0, subvol_id as u64, 0);
    if let Some(mut src) = bch2_subvolume_get(engine, parent_subvol) {
        src.snapshot = src_new_snap_id;
        let src_pos = Bpos::new(0, parent_subvol as u64, 0);
        let entries = [
            (BatchEntry::Delete { pos: src_pos }, 0),
            (
                BatchEntry::Insert {
                    pos: src_pos,
                    data: src.to_bytes(),
                },
                0,
            ),
            (
                BatchEntry::Insert {
                    pos: new_subvol_pos,
                    data: sv.to_bytes(),
                },
                0,
            ),
        ];
        if !engine.batch_write(BtreeId::Subvolumes, &entries) {
            return Err(StorageError::Transaction(
                "batch_write failed in bch2_subvolume_snapshot".into(),
            ));
        }
    } else {
        // 如果无法获取父子卷，只创建快照子卷
        let entry = BtreeEntry::new(
            new_subvol_pos,
            KeyType::Normal,
            KeyValue::Raw(sv.to_bytes()),
        );
        engine.insert_entry_raw(BtreeId::Subvolumes, entry, 0);
    }

    // 5. 注册 inode 映射
    engine.register_ino_map(inode, subvol_id);

    // 6. 合并去重，保持 btree 一致性
    engine.get_mut(BtreeId::Snapshots).compact();
    engine.get_mut(BtreeId::Subvolumes).compact();

    Ok(subvol_id)
}

// ─── 子卷查询 ───

/// 获取子卷（反序列化返回 owned 值）
///
/// 对齐 bcachefs `bch2_subvolume_get()`。
pub fn bch2_subvolume_get(engine: &BtreeEngine, subvol: u32) -> Option<BchSubvolume> {
    let pos = Bpos::new(0, subvol as u64, 0);
    let entry = engine.get_entry_raw(BtreeId::Subvolumes, pos)?;
    let bytes = entry.value.to_bytes();
    BchSubvolume::from_bytes(&bytes).ok()
}

/// 获取子卷的快照 ID
///
/// 对齐 bcachefs `bch2_subvolume_get_snapshot()`。
pub fn bch2_subvolume_get_snapshot(engine: &BtreeEngine, subvolid: u32) -> Option<u32> {
    let sv = bch2_subvolume_get(engine, subvolid)?;
    Some(sv.snapshot)
}

/// 快照树引用校验触发器（对齐 bcachefs `bch2_subvolume_trigger`）
///
/// 在子卷条目变更时校验以下约束：
/// - 子卷引用的快照 ID 必须存在于 Snapshots btree 中
/// - 快照节点回引的 subvol 必须与当前子卷 ID 一致（双向引用一致性）
/// - 根子卷 (ID 1) 不可执行删除标记
/// - 子卷的 creation_parent 必须指向一个存在的子卷（除非为 0）
///
/// 这是 read-only 校验，不修改任何数据。
/// 返回 `Ok(())` 表示校验通过。
pub fn bch2_subvolume_trigger(
    engine: &BtreeEngine,
    subvolid: u32,
    sv: &BchSubvolume,
) -> Result<(), StorageError> {
    // 1. 根子卷保护
    if sv.is_unlinked() && subvolid == 1 {
        return Err(StorageError::InvalidArgument(
            "cannot mark root subvolume as unlinked".into(),
        ));
    }

    // 2. 快照引用校验
    let snap = read_snapshot_value(engine, sv.snapshot);
    if snap.is_none() {
        return Err(StorageError::NotFound(format!(
            "subvolume {} references non-existent snapshot {}",
            subvolid, sv.snapshot
        )));
    }

    // 3. 双向引用一致性校验
    if let Some(snap) = snap {
        if snap.subvol != 0 && snap.subvol != subvolid {
            return Err(StorageError::InvalidArgument(format!(
                "snapshot {} subvol pointer mismatch: expected {}, got {}",
                sv.snapshot, subvolid, snap.subvol
            )));
        }
    }

    // 4. creation_parent 引用校验
    if sv.creation_parent != 0 && sv.creation_parent != subvolid {
        let parent_exists = bch2_subvolume_get(engine, sv.creation_parent).is_some();
        if !parent_exists {
            return Err(StorageError::NotFound(format!(
                "subvolume {} references non-existent parent {}",
                subvolid, sv.creation_parent
            )));
        }
    }

    Ok(())
}

/// 检查子卷是否为只读
///
/// 对齐 bcachefs `bch2_subvol_is_ro()`。
/// 如果子卷已删除也返回错误（与 bcachefs 语义一致）。
pub fn bch2_subvol_is_ro(engine: &BtreeEngine, subvol: u32) -> Result<bool, StorageError> {
    let sv = bch2_subvolume_get(engine, subvol)
        .ok_or_else(|| StorageError::NotFound(format!("subvolume {subvol}")))?;
    Ok(sv.is_read_only() || sv.is_unlinked())
}

/// 从 snapshot ID 获取对应的子卷
///
/// 对齐 bcachefs `bch2_snapshot_get_subvol()`。
pub fn bch2_snapshot_get_subvol(engine: &BtreeEngine, snapshot: u32) -> Option<BchSubvolume> {
    // 从 Snapshots btree 读取快照节点，获取 subvol 字段
    // 直接使用完整的 SnapshotT 反序列化（原本地 SnapshotRef 结构体与 SnapshotT 字段不匹配）
    let snap: SnapshotT = read_snapshot_value(engine, snapshot)?;
    bch2_subvolume_get(engine, snap.subvol)
}

// ─── 子卷删除 ───

/// 删除子卷（标记为 UNLINKED，写回 btree），同时清理关联快照
///
/// 对齐 bcachefs `bch2_subvolume_unlink()` + `bch2_subvolume_delete()`。
/// 使用 bcachefs 风格的 delete+insert 模式：
/// 1. `delete_entry` 追加 KEY_TYPE_DELETED 墓碑
/// 2. `insert_entry_raw` 追加新的 KEY_TYPE_NORMAL 值（UNLINKED 标志）
/// 3. 调用 `bch2_snapshot_node_set_deleted` 清理关联的快照节点
///
/// 注意：不调用 compact() — compact 目前使用旧 API 读写 entry，
/// 会丢失 Raw value 数据。
pub fn bch2_subvolume_delete(engine: &mut BtreeEngine, subvolid: u32) -> Result<(), StorageError> {
    // 根子卷 (ID 1) 不可删除 — 对齐 bcachefs 语义
    if subvolid == 1 || subvolid == u32::try_from(BCACHEFS_ROOT_INO).unwrap_or(1) {
        return Err(StorageError::InvalidArgument(
            "cannot delete root subvolume".into(),
        ));
    }

    let pos = Bpos::new(0, subvolid as u64, 0);
    let entry = engine
        .get_entry_raw(BtreeId::Subvolumes, pos)
        .ok_or_else(|| StorageError::NotFound(format!("subvolume {subvolid}")))?;
    let mut sv = BchSubvolume::from_bytes(&entry.value.to_bytes())
        .map_err(|_| StorageError::NotFound(format!("subvolume {subvolid} corrupt")))?;
    sv.mark_unlinked();

    // 触发器校验：删除前验证一致性
    bch2_subvolume_trigger(engine, subvolid, &sv)?;

    // 清理 inode 映射（在写标记之前执行，确保映射数据一致性）
    engine.cleanup_ino_map(sv.inode, subvolid);

    let entries = [
        (BatchEntry::Delete { pos }, 0),
        (
            BatchEntry::Insert {
                pos,
                data: sv.to_bytes(),
            },
            0,
        ),
    ];
    if !engine.batch_write(BtreeId::Subvolumes, &entries) {
        return Err(StorageError::Transaction(
            "batch_write failed in bch2_subvolume_delete".into(),
        ));
    }

    // 3. 清理关联快照
    let snap_id = sv.snapshot;
    if snap_id != 0 {
        let _ = bch2_snapshot_node_set_deleted(engine, snap_id);
    }

    Ok(())
}

// ─── 子卷列表 / 计数 ───

/// 列出所有活跃（未删除）子卷，按 ID 排序
///
/// 对齐 bcachefs `bch2_subvolume_list()`。
pub fn bch2_subvolume_list(engine: &BtreeEngine) -> Vec<(u32, BchSubvolume)> {
    let mut result = Vec::new();
    engine.get(BtreeId::Subvolumes).for_each_entry(|entry| {
        let bytes = entry.value.to_bytes();
        if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
            if !sv.is_unlinked() {
                let id = entry.pos.offset as u32;
                result.push((id, sv));
            }
        }
    });
    result.sort_by_key(|(id, _)| *id);
    result
}

/// 活跃子卷数量
pub fn bch2_subvolume_count(engine: &BtreeEngine) -> usize {
    let mut count = 0usize;
    engine.get(BtreeId::Subvolumes).for_each_entry(|entry| {
        let bytes = entry.value.to_bytes();
        if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
            if !sv.is_unlinked() {
                count += 1;
            }
        }
    });
    count
}

// ─── 子卷关系操作 ───

/// 重挂子卷：将 `subvolid` 的所有非删除子卷的 parent 改为 `new_parent`
///
/// 对齐 bcachefs `bch2_subvolumes_reparent()`。
/// 用于删除子卷前，避免 orphan 子卷。
pub fn bch2_subvolumes_reparent(
    engine: &mut BtreeEngine,
    subvolid: u32,
    new_parent: u32,
) -> Result<(), StorageError> {
    // 收集所有 creation_parent == subvolid 的子卷
    let mut children: Vec<(u32, BchSubvolume)> = Vec::new();
    engine.get(BtreeId::Subvolumes).for_each_entry(|entry| {
        let bytes = entry.value.to_bytes();
        if let Ok(sv) = BchSubvolume::from_bytes(&bytes) {
            if sv.creation_parent == subvolid && !sv.is_unlinked() {
                let id = entry.pos.offset as u32;
                children.push((id, sv));
            }
        }
    });

    for (child_id, mut sv) in children {
        sv.creation_parent = new_parent;
        let pos = Bpos::new(0, child_id as u64, 0);
        engine.delete_entry(
            BtreeId::Subvolumes,
            &BtreeKey::from_bpos(pos, KeyType::Normal),
            0,
        );
        let new_entry = BtreeEntry::new(pos, KeyType::Normal, KeyValue::Raw(sv.to_bytes()));
        engine.insert_entry_raw(BtreeId::Subvolumes, new_entry, 0);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════════

/// 创建根快照/子卷结构 — bcachefs 精确对齐
///
/// 对应 bcachefs `bch2_initialize_subvolumes()` (subvolume.c:653-681)。
/// 在全新文件系统上创建三条记录：
///   1. SnapshotTrees btree: 树 ID=1 → SnapshotTreeT
///   2. Snapshots btree:     snapshot_id=U32_MAX → 根快照节点 (SUBVOL leaf)
///   3. Subvolumes btree:    subvol_id=BCACHEFS_ROOT_SUBVOL(1) → BchSubvolume
///
/// 本函数仅在 bch2_fs_initialize() 中调用，不参与 recovery pass 调度。
/// 每次调用从 vollen 新建，不存在幂等问题。
pub fn bch2_initialize_subvolumes(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    // 1. SnapshotTrees btree: tree_id=1
    //    对应 bcachefs: bkey_snapshot_tree_init + offset=1 + master_subvol=1 + root_snapshot=U32_MAX
    let tree_val = SnapshotTreeT::new(u32::MAX, BCACHEFS_ROOT_SUBVOL as u32);
    let raw = bincode::serialize(&tree_val).map_err(|e| StorageError::Serialization(e))?;
    let entry = BtreeEntry::raw(Bpos::new(0, 0, 1), KeyType::Normal, raw);
    engine.insert_entry_raw(BtreeId::SnapshotTrees, entry, 0);

    // 2. Snapshots btree: snapshot_id=U32_MAX
    //    对应 bcachefs: bkey_snapshot_init + offset=U32_MAX + parent=0
    //    + subvol=BCACHEFS_ROOT_SUBVOL + tree=1 + flags: SUBVOL
    let snap_val = SnapshotT::new_leaf(0, BCACHEFS_ROOT_SUBVOL as u32, 1, 1, 0);
    let raw = bincode::serialize(&snap_val).map_err(|e| StorageError::Serialization(e))?;
    let entry = BtreeEntry::raw(Bpos::new(0, 0, u32::MAX), KeyType::Normal, raw);
    engine.insert_entry_raw(BtreeId::Snapshots, entry, 0);

    // 3. Subvolumes btree: subvol_id=BCACHEFS_ROOT_SUBVOL (1)
    //    对应 bcachefs: bkey_subvolume_init + offset=BCACHEFS_ROOT_SUBVOL
    let subvol_val = BchSubvolume::new(u32::MAX, BCACHEFS_ROOT_INO, 0, 0);
    let raw = bincode::serialize(&subvol_val).map_err(|e| StorageError::Serialization(e))?;
    let entry = BtreeEntry::raw(Bpos::new(0, BCACHEFS_ROOT_SUBVOL, 0), KeyType::Normal, raw);
    engine.insert_entry_raw(BtreeId::Subvolumes, entry, 0);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::BtreeEngine;

    // ─── bch2_subvolume_create / list ───

    #[test]
    fn test_create_and_list() {
        let mut engine = BtreeEngine::new();
        let id = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        assert!(id > 0);
        assert_eq!(bch2_subvolume_list(&engine).len(), 1);
    }

    #[test]
    fn test_create_multiple() {
        let mut engine = BtreeEngine::new();
        let id1 = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let id2 = bch2_subvolume_create(&mut engine, 0, 8192, 2000).unwrap();
        assert!(id2 > id1);
        assert_eq!(bch2_subvolume_list(&engine).len(), 2);
        assert_eq!(bch2_subvolume_count(&engine), 2);
    }

    // ─── bch2_subvolume_get ───

    #[test]
    fn test_load() {
        let mut engine = BtreeEngine::new();
        let id = bch2_subvolume_create(&mut engine, 0, 65536, 500).unwrap();
        let loaded = bch2_subvolume_get(&engine, id).unwrap();
        assert!(loaded.snapshot > 0);
        assert_eq!(loaded.size, 65536);
    }

    #[test]
    fn test_load_nonexistent() {
        let engine = BtreeEngine::new();
        assert!(bch2_subvolume_get(&engine, 999).is_none());
    }

    // ─── bch2_subvolume_delete ───

    #[test]
    fn test_delete() {
        let mut engine = BtreeEngine::new();
        // 创建根子卷 (ID 1) 和可删除子卷 (ID 2)
        let _root = bch2_subvolume_create(&mut engine, 0, 4096, 100).unwrap();
        let target = bch2_subvolume_create(&mut engine, 0, 4096, 100).unwrap();
        assert!(bch2_subvolume_get(&engine, target).is_some());
        bch2_subvolume_delete(&mut engine, target).unwrap();
        assert!(bch2_subvolume_get(&engine, target).unwrap().is_unlinked());
        assert!(bch2_subvolume_list(&engine).len() == 1);
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut engine = BtreeEngine::new();
        assert!(bch2_subvolume_delete(&mut engine, 999).is_err());
    }

    // ─── bch2_subvolume_snapshot ───

    #[test]
    fn test_create_snapshot_subvolume() {
        let mut engine = BtreeEngine::new();
        let parent = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let snap = bch2_subvolume_snapshot(&mut engine, parent, 0, 4096, 2000).unwrap();
        assert!(snap > parent);
        let loaded = bch2_subvolume_get(&engine, snap).unwrap();
        assert!(loaded.is_snapshot());
        assert!(loaded.is_read_only());
        assert_eq!(loaded.creation_parent, parent);
    }

    #[test]
    fn test_create_snapshot_invalid_parent() {
        let mut engine = BtreeEngine::new();
        assert!(bch2_subvolume_snapshot(&mut engine, 999, 0, 4096, 1000).is_err());
    }

    #[test]
    fn test_create_snapshot_unlinked_parent() {
        let mut engine = BtreeEngine::new();
        // 创建根子卷 (ID 1) 和可删除的父子卷 (ID 2)
        let _root = bch2_subvolume_create(&mut engine, 0, 4096, 100).unwrap();
        let parent = bch2_subvolume_create(&mut engine, 0, 4096, 100).unwrap();
        bch2_subvolume_delete(&mut engine, parent).unwrap();
        assert!(bch2_subvolume_snapshot(&mut engine, parent, 0, 4096, 200).is_err());
    }

    // ─── bch2_subvolume_get_snapshot ───

    #[test]
    fn test_get_snapshot() {
        let mut engine = BtreeEngine::new();
        let id = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let snap = bch2_subvolume_get_snapshot(&engine, id);
        assert!(snap.is_some());
        assert!(snap.unwrap() > 0);
    }

    #[test]
    fn test_get_snapshot_nonexistent() {
        let engine = BtreeEngine::new();
        assert!(bch2_subvolume_get_snapshot(&engine, 999).is_none());
    }

    // ─── bch2_subvol_is_ro ───

    #[test]
    fn test_is_ro_normal() {
        let mut engine = BtreeEngine::new();
        let id = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        assert!(!bch2_subvol_is_ro(&engine, id).unwrap());
    }

    #[test]
    fn test_is_ro_snapshot() {
        let mut engine = BtreeEngine::new();
        let parent = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let snap = bch2_subvolume_snapshot(&mut engine, parent, 0, 4096, 2000).unwrap();
        assert!(bch2_subvol_is_ro(&engine, snap).unwrap());
    }

    #[test]
    fn test_is_ro_deleted() {
        let mut engine = BtreeEngine::new();
        // 创建根子卷 (ID 1) 和可删除子卷 (ID 2)
        let _root = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let target = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        bch2_subvolume_delete(&mut engine, target).unwrap();
        assert!(bch2_subvol_is_ro(&engine, target).unwrap());
    }

    // ─── 持久化和一致性 ───

    #[test]
    fn test_btree_persistence_across_operations() {
        let mut engine = BtreeEngine::new();

        // 创建根子卷 (ID 1)、可删除子卷 (ID 2) 和子卷 (ID 3)
        let _root = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let id2 = bch2_subvolume_create(&mut engine, 0, 4096, 1000).unwrap();
        let id3 = bch2_subvolume_create(&mut engine, 0, 8192, 2000).unwrap();
        assert_eq!(bch2_subvolume_count(&engine), 3);

        let sv2 = bch2_subvolume_get(&engine, id2).unwrap();
        assert_eq!(sv2.size, 4096);
        let sv3 = bch2_subvolume_get(&engine, id3).unwrap();
        assert!(sv3.snapshot > 0);

        // 删除第二个子卷（ID 2，不是根子卷）
        bch2_subvolume_delete(&mut engine, id2).unwrap();
        assert_eq!(bch2_subvolume_count(&engine), 2);

        let list = bch2_subvolume_list(&engine);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_roundtrip_matches_hashmap_behavior() {
        let mut engine = BtreeEngine::new();

        let id = bch2_subvolume_create(&mut engine, 0, 65536, 500).unwrap();
        let sv = bch2_subvolume_get(&engine, id).unwrap();
        assert!(sv.snapshot > 0);
        assert_eq!(sv.size, 65536);
        assert!(!sv.is_unlinked());
        assert!(!sv.is_snapshot());
        assert!(!sv.is_read_only());
    }
}
