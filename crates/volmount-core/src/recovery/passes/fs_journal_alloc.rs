use crate::alloc::{AllocRequest, BchDataType};
use crate::recovery::RecoveryState;
use crate::types::{StorageError, Watermark};

/// FsJournalAlloc pass — 确保 journal 至少有一个已分配 bucket
///
/// 对应 bcachefs `bch2_fs_journal_alloc()` (PASS_ALWAYS #7)。
/// 仅在 journal bucket 为零时从 allocator 分配一个（对齐 bcachefs 逻辑）。
/// Journal::create() 在 init 路径中已分配 bucket，此处仅作安全补充。
///
/// # 幂等性
/// 当 journal 已有 bucket 时不做分配，后续执行直接通过。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let js = state.journal.to_superblock_state();
    let current_count = js.bucket_addrs.len();

    // 对齐 bcachefs: 仅当 journal 无 bucket 时分配
    if current_count == 0 && state.allocator.total_blocks() > 0 {
        let _new_addrs = state.allocator.bch2_alloc_buckets(
            1,
            &mut state.engine,
            &AllocRequest::new(Watermark::Normal, BchDataType::Reserved),
            None,
        )?;
    }

    Ok(())
}
