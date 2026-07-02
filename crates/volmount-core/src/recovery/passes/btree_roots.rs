use crate::btree::BTREE_ID_NR;
use crate::journal::JournalReplayer;
use crate::recovery::{effective_root_source, EffectiveRoot, RecoveryState};
use crate::types::StorageError;

/// Pass: 从 journal 中提取 btree root 信息并加载 root 节点
///
/// 对应 bcachefs 的 btree root recovery 阶段：
/// 1. 从 journal 的 BtreeRoot 条目中获取 root 指针
/// 2. 与 superblock 的 root_addrs/root_levels 合并（journal 覆盖 superblock）
/// 3. 调用 load_root_from_ptr() 加载根节点
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    let replayer = JournalReplayer::new(&state.journal, &*state.backend);
    let journal_roots = replayer.read_btree_roots().await?;

    // 合并 superblock roots + journal roots（journal 覆盖 superblock）
    for ty in BTREE_ID_NR {
        if let Some(source) = effective_root_source(&state.superblock, &journal_roots, ty) {
            match source {
                EffectiveRoot::Ptr(root_ptr) => {
                    state
                        .engine
                        .load_root_from_ptr(ty, &*state.backend, root_ptr)
                        .await?;
                }
                EffectiveRoot::Addr(root_addr) => {
                    state.engine.load_root(ty, &*state.backend, root_addr).await?;
                }
            }
        }
    }

    state.recovered_roots = journal_roots;
    Ok(())
}
