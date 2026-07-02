use std::collections::VecDeque;

use crate::btree::key::{Bpos, BtreeEntry};
use crate::btree::BtreeEngine;
use crate::btree::BtreeId;

/// Journal key — 对应 bcachefs `struct journal_key`
#[derive(Debug, Clone)]
pub struct JournalKey {
    pub journal_seq: u64,
    pub btree_type: BtreeId,
    pub entry: BtreeEntry,
    pub overwritten: bool,
}

/// Journal overlay — bcachefs journal_keys buffer 对齐实现
///
/// set_may_go_rw pass 激活，journal_replay pass 完成时 drain 到 btree。
/// active 但 replay 未完成时，新写入 push 到 overlay buffer 而非直写 btree。
/// 创建时 active = false（默认不拦截），由 set_may_go_rw pass 启用。
#[derive(Debug)]
pub struct JournalKeys {
    /// 排序的 overlay entries
    entries: VecDeque<JournalKey>,
    /// 是否激活（= 拦截 insert_guarded 调用）
    pub active: bool,
    /// 是否正在 drain（不拦截新写入）
    pub draining: bool,
}

impl JournalKeys {
    /// 创建新的 overlay，初始为 inactive
    /// set_may_go_rw pass 调用 set_active(true) 启用
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            active: false,
            draining: false,
        }
    }

    /// 插入一条 journal key
    ///
    /// bcachefs 对齐：按 (btree_type, entry key) 顺序排列，
    /// 若已有相同 key 则标记旧条目为 overwritten。
    /// 使用 VecDeque 尾部追加 + 去重（在 volmount 单线程模型下 O(n) 足够）。
    pub fn push(&mut self, journal_seq: u64, btree_type: BtreeId, entry: BtreeEntry) {
        // 查找并标记已存在的相同 key 为 overwritten
        for existing in self.entries.iter_mut() {
            if existing.btree_type == btree_type && existing.entry.pos == entry.pos {
                existing.overwritten = true;
            }
        }
        self.entries.push_back(JournalKey {
            journal_seq,
            btree_type,
            entry,
            overwritten: false,
        });
    }

    /// 在 overlay 中查找指定 (btree_type, offset, snapshot) 的最新非 overwritten key
    ///
    /// bcachefs 对齐：对应 journal_keys 读穿透查找。
    /// 遍历时从尾部（最新）开始，返回第一个匹配的非 overwritten 条目。
    /// 若未找到，返回 None。
    pub fn lookup_entry(&self, btree_type: BtreeId, pos: Bpos) -> Option<&JournalKey> {
        self.entries.iter().rev().find(|e| {
            e.btree_type == btree_type
                && e.entry.pos.offset == pos.offset
                && e.entry.pos.snapshot == pos.snapshot
                && !e.overwritten
        })
    }

    /// 在 overlay 中查找指定 btree_type 中 ≥ pos 的下一个非 overwritten entry
    ///
    /// bcachefs 对齐：对应 `bch2_journal_keys_peek_max()` 语义 ——
    /// 遍历找到 btree_type 匹配且 entry.pos >= pos 的最小 entry。
    /// 若未找到，返回 None。
    pub fn find_next_entry(&self, btree_type: BtreeId, pos: Bpos) -> Option<&JournalKey> {
        self.entries
            .iter()
            .filter(|e| e.btree_type == btree_type && !e.overwritten && e.entry.pos >= pos)
            .min_by(|a, b| a.entry.pos.cmp(&b.entry.pos))
    }

    /// Drain 所有 entries 到 engine（按插入顺序应用）
    ///
    /// 跳过 overwritten 标记的条目（被后续同 key 写入覆盖）。
    /// 使用 insert_entry_raw 绕过 overlay（直写 btree）。
    pub fn drain_all(&mut self, engine: &mut BtreeEngine) {
        self.draining = true;
        while let Some(entry) = self.entries.pop_front() {
            if entry.overwritten {
                continue;
            }
            engine.insert_entry_raw(entry.btree_type, entry.entry, 0);
        }
        self.active = false;
        self.draining = false;
    }
}

impl Default for JournalKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{Bpos, BtreeKey, KeyType, KeyValue};

    fn make_extent_entry(lba: u64, paddr: u64) -> BtreeEntry {
        BtreeEntry::new(
            Bpos::new(1, lba, 0),
            KeyType::Normal,
            KeyValue::extent(paddr, 1),
        )
    }

    #[test]
    fn test_overlay_new_is_inactive() {
        let overlay = JournalKeys::new();
        assert!(!overlay.active);
        assert!(!overlay.draining);
        assert!(overlay.entries.is_empty());
    }

    #[test]
    fn test_overlay_push_and_drain() {
        let mut overlay = JournalKeys::new();
        let mut engine = BtreeEngine::new();

        let entry = make_extent_entry(10, 0x100);
        overlay.push(1, BtreeId::Extents, entry.clone());

        assert_eq!(overlay.entries.len(), 1);

        overlay.drain_all(&mut engine);

        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        assert!(engine.get_entry(BtreeId::Extents, &key).is_some());
        assert!(!overlay.active);
    }

    #[test]
    fn test_overlay_overwritten_skipped_on_drain() {
        let mut overlay = JournalKeys::new();
        let mut engine = BtreeEngine::new();

        let entry1 = make_extent_entry(10, 0x100);
        let entry2 = make_extent_entry(10, 0x200); // same key

        overlay.push(1, BtreeId::Extents, entry1);
        overlay.push(2, BtreeId::Extents, entry2);

        // First entry should be marked overwritten
        assert!(overlay.entries[0].overwritten);
        assert!(!overlay.entries[1].overwritten);

        overlay.drain_all(&mut engine);

        // Only the second (non-overwritten) entry should have been applied
        let key = BtreeKey::from_bpos(Bpos::new(1, 10, 0), KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &key);
        assert!(got.is_some());
        // Value should be 0x200 (the last write wins), not 0x100
        assert_eq!(got.unwrap().1.paddr.get(), 0x200);
    }

    #[test]
    fn test_overlay_push_multiple_types() {
        let mut overlay = JournalKeys::new();

        let ext_entry = make_extent_entry(10, 0x100);
        let snap_entry = BtreeEntry::new(
            Bpos::new(2, 0, 0),
            KeyType::Normal,
            KeyValue::extent(0x500, 8),
        );

        overlay.push(1, BtreeId::Extents, ext_entry);
        overlay.push(2, BtreeId::Snapshots, snap_entry);

        assert_eq!(overlay.entries.len(), 2);
        // Different types, different keys — neither overwritten
        assert!(!overlay.entries[0].overwritten);
        assert!(!overlay.entries[1].overwritten);
    }

    #[test]
    fn test_overlay_drain_empty_noop() {
        let mut overlay = JournalKeys::new();
        let mut engine = BtreeEngine::new();
        overlay.drain_all(&mut engine);
        // No panic
    }

    // ── lookup_entry tests ──────────────────────────────────────────────

    #[test]
    fn test_overlay_lookup_entry_found() {
        let mut overlay = JournalKeys::new();
        let entry = make_extent_entry(10, 0x100);
        overlay.push(1, BtreeId::Extents, entry.clone());

        let found = overlay.lookup_entry(BtreeId::Extents, Bpos::new(1, 10, 0));
        assert!(found.is_some());
        assert_eq!(found.unwrap().entry.pos.offset, 10);
    }

    #[test]
    fn test_overlay_lookup_entry_not_found() {
        let overlay = JournalKeys::new();

        let found = overlay.lookup_entry(BtreeId::Extents, Bpos::new(1, 99, 0));
        assert!(found.is_none());

        let entry = make_extent_entry(10, 0x100);
        let mut overlay = JournalKeys::new();
        overlay.push(1, BtreeId::Extents, entry);

        // wrong type
        assert!(overlay
            .lookup_entry(BtreeId::Alloc, Bpos::new(1, 10, 0))
            .is_none());
        // wrong pos
        assert!(overlay
            .lookup_entry(BtreeId::Extents, Bpos::new(1, 99, 0))
            .is_none());
    }

    #[test]
    fn test_overlay_lookup_entry_overwritten_returns_latest() {
        let mut overlay = JournalKeys::new();

        let v1 = make_extent_entry(10, 0x100);
        let v2 = make_extent_entry(10, 0x200);

        overlay.push(1, BtreeId::Extents, v1);
        overlay.push(2, BtreeId::Extents, v2);

        let found = overlay.lookup_entry(BtreeId::Extents, Bpos::new(1, 10, 0));
        assert!(found.is_some());
        // Should return the latest (non-overwritten) entry
        assert_eq!(
            found.unwrap().entry.value.as_extent().unwrap().paddr.get(),
            0x200
        );
    }

    #[test]
    fn test_overlay_lookup_entry_empty_overlay() {
        let overlay = JournalKeys::new();

        assert!(overlay
            .lookup_entry(BtreeId::Extents, Bpos::new(1, 10, 0))
            .is_none());
    }

    #[test]
    fn test_overlay_lookup_entry_different_btree_types() {
        let mut overlay = JournalKeys::new();

        let ext_entry = make_extent_entry(10, 0x100);
        let snap_entry = BtreeEntry::new(
            Bpos::new(2, 0, 0),
            KeyType::Normal,
            KeyValue::extent(0x500, 8),
        );

        overlay.push(1, BtreeId::Extents, ext_entry);
        overlay.push(2, BtreeId::Snapshots, snap_entry);

        let found_ext = overlay.lookup_entry(BtreeId::Extents, Bpos::new(1, 10, 0));
        assert!(found_ext.is_some());
        assert_eq!(
            found_ext
                .unwrap()
                .entry
                .value
                .as_extent()
                .unwrap()
                .paddr
                .get(),
            0x100
        );

        let found_snap = overlay.lookup_entry(BtreeId::Snapshots, Bpos::new(2, 0, 0));
        assert!(found_snap.is_some());
        assert_eq!(
            found_snap
                .unwrap()
                .entry
                .value
                .as_extent()
                .unwrap()
                .paddr
                .get(),
            0x500
        );

        // Non-existent type
        assert!(overlay
            .lookup_entry(BtreeId::Alloc, Bpos::new(1, 10, 0))
            .is_none());
    }

    #[test]
    fn test_overlay_lookup_entry_all_entries_overwritten() {
        let mut overlay = JournalKeys::new();

        let v1 = make_extent_entry(10, 0x100);
        let v2 = make_extent_entry(10, 0x200);
        let v3 = make_extent_entry(10, 0x300);

        overlay.push(1, BtreeId::Extents, v1);
        overlay.push(2, BtreeId::Extents, v2);
        overlay.push(3, BtreeId::Extents, v3);

        // v1 and v2 should be overwritten, v3 is the latest
        let found = overlay.lookup_entry(BtreeId::Extents, Bpos::new(1, 10, 0));
        assert!(found.is_some());
        assert_eq!(
            found.unwrap().entry.value.as_extent().unwrap().paddr.get(),
            0x300
        );
        // v3 should NOT be overwritten
        assert!(!found.unwrap().overwritten);
    }
}
