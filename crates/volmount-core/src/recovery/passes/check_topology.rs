use std::sync::atomic::Ordering;

use crate::alloc::BchAllocator;
use crate::btree::gc::BtreeGc;
use crate::btree::gc::{bch2_check_topology, bch2_gc_gens};
use crate::btree::BtreeEngine;
use crate::journal::Journal;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: btree 拓扑完整性检查 + GC generation 传递（对应 bcachefs `bch2_check_topology()`）
///
/// 验证所有 btree 的条目顺序和唯一性。
/// 同时执行 gc_gens（原 GcScan pass 的 GC generation 传递）— 遍历 Extents btree，
/// 标记被引用的 bucket，更新 gc_pos。
/// 仅在非干净关闭时运行（UNCLEAN）。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let journal_seq = state.journal.last_seq_ondisk.load(Ordering::Relaxed);

    // GC generation 传递：标记被引用的 bucket
    bch2_gc_gens(
        &state.engine,
        &mut state.allocator,
        &mut state.gc,
        journal_seq,
    )?;

    // 拓扑检查：验证所有 btree 的条目顺序和唯一性
    bch2_check_topology(&state.engine)?;

    Ok(())
}
