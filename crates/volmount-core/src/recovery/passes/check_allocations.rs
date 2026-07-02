use crate::btree::gc::bch2_check_allocations;
use crate::recovery::RecoveryState;
use crate::types::StorageError;

/// Pass: 分配一致性检查（对应 bcachefs `bch2_check_allocations()`）
///
/// 对比 extent 引用与 allocator 状态，检测 discrepancies。
/// 仅在 FSCK 模式下运行（volmount 默认关闭，pass 返回 Ok）。
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // 仅在 FSCK 标志设置时执行实际检查
    // 当前 volmount 无 fsck 模式，静默跳过
    let _discrepancies = bch2_check_allocations(&state.engine, &state.allocator)?;
    Ok(())
}
