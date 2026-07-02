use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// AccountingRead pass — 验证 allocator 使用计数一致性
///
/// 对应 bcachefs `bch2_accounting_read()` (PASS_ALWAYS #39)。
/// volmount 中 accounting 简化（Alloc btree 条目直接携带 bucket state），
/// bch2_alloc_read() 已完成状态恢复。此 pass 做完整性验证。
///
/// # 幂等性
/// 只读验证，无副作用。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let used = state.allocator.allocated_blocks();
    let free = state.allocator.free_blocks();
    let total = state.allocator.total_blocks();
    if used + free > total {
        return Err(StorageError::InvalidData(format!(
            "accounting mismatch: used {} + free {} > total {}",
            used, free, total
        )));
    }
    Ok(())
}
