use crate::btree::BTREE_ID_NR;
use crate::journal::{extract_blacklist_entries, BlacklistEntry, JournalReplayer, Jset};
use crate::recovery::{effective_root_source, EffectiveRoot, RecoveryState};
use crate::types::StorageError;

/// 判断 jset 的 seq 是否被任何 blacklist entry 覆盖
fn in_blacklist(jset: &Jset, blacklist: &[BlacklistEntry]) -> bool {
    blacklist
        .iter()
        .any(|bl| jset.header.seq >= bl.start_seq && jset.header.seq < bl.end_seq)
}

/// R5: Unclean shutdown seq skip 窗口大小 — 对应 bcachefs 的 +64 skip
///
/// unclean 关闭时，最后 64 个 seq 可能包含部分写入的 journal entry，
/// 跳过它们以防止恢复不完整数据。
const UNCLEAN_SEQ_SKIP: u64 = 64;

/// Pass: 读取所有 journal buckets + 加载 btree roots（合并 journal & superblock）
///
/// 对应 bcachefs 的 journal read + btree root loading 阶段。
/// 两个步骤合并为一个 pass，与 bcachefs 语义一致（journal 读取和 root 提取一起完成）。
///
/// 操作：
/// 1. 读取所有 Jset → 提取 blacklist entries → 过滤已落盘的 jsets
/// 2. 从 journal 的 BtreeRoot 条目中获取 root 指针
/// 3. 与 superblock 的 root_addrs/root_levels 合并（journal 覆盖 superblock）
/// 4. 调用 load_root_from_ptr() 加载根节点
pub async fn run(state: &mut RecoveryState) -> Result<(), StorageError> {
    // Phase 1: 读取并过滤 journal entries
    let all_jsets = state
        .journal
        .bch2_journal_entries_read(&*state.backend)
        .await
        .map_err(|e| StorageError::NotFound(format!("journal_read: {}", e)))?;

    // 按 seq 排序确保顺序
    let mut jsets: Vec<Jset> = all_jsets.into_iter().map(|(_, jset)| jset).collect();
    jsets.sort_by_key(|j| j.header.seq);

    // 提取 blacklist entries（跳过已落盘范围覆盖的 seq）
    let blacklist = extract_blacklist_entries(&jsets);
    if !blacklist.is_empty() {
        jsets.retain(|jset| !in_blacklist(jset, &blacklist));
    }

    // R5: unclean shutdown 时跳过最后 64 个 seq（可能含部分写入数据）
    if !state.superblock.clean_shutdown && !jsets.is_empty() {
        let max_seq = jsets.last().unwrap().header.seq;
        if max_seq > UNCLEAN_SEQ_SKIP {
            // 只跳过尾部窗口内的 seq；小日志直接保留，避免把全部 entries 过滤掉。
            let keep_before = max_seq - UNCLEAN_SEQ_SKIP + 1;
            jsets.retain(|jset| jset.header.seq < keep_before);
        }
    }

    state.jsets = jsets;

    // Phase 2: 从 journal + superblock 加载 btree roots
    // 合并 journal roots 与 superblock roots（journal 覆盖 superblock）
    let replayer = JournalReplayer::new(&state.journal, &*state.backend);
    let journal_roots = replayer.read_btree_roots().await?;

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
                    state
                        .engine
                        .load_root(ty, &*state.backend, root_addr)
                        .await?;
                }
            }
        }
    }

    state.recovered_roots = journal_roots;
    Ok(())
}
