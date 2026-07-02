pub mod meta;
pub mod snapshot;
pub mod table;

pub use meta::{BchSnapshotFlags, SnapshotIdState, SnapshotMeta, SnapshotT, SnapshotTreeT};
pub use snapshot::{
    // 核心 bcachefs 对齐 API
    bch2_check_key_has_snapshot,
    bch2_delete_dead_snapshots,
    bch2_reconstruct_snapshots,
    bch2_snapshot_is_ancestor_btree,
    bch2_snapshot_is_ancestor_subvol,
    bch2_snapshot_lookup,
    bch2_snapshot_node_create,
    bch2_snapshot_node_set_deleted,
    bch2_snapshot_skiplist_get,
    bch2_snapshot_skiplist_good,
    bch2_snapshot_tree_master_subvol,
    // 快照树操作
    create_root_snapshot_btree,
    dfs_descendants,
    dfs_descendants_alive,
    get_next_snapshot_id,
    is_ancestor_from_btree,
    list_snapshots_from_btree,
    read_snapshot_tree_value,
    read_snapshot_value,
    write_snapshot_tree_value,
    // Layer 3: Key snapshot 验证
    CheckKeySnapshotResult,
    // 迭代器
    DfsIter,
    // 位图分配器
    SnapshotIdBitmap,
    // 注册表
    SubtreeRegistry,
};
pub use table::{
    bch2_fs_snapshots_exit, bch2_fs_snapshots_init, bch2_snapshots_read, SnapshotTable,
    SnapshotTreeTable,
};
