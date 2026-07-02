//! Journal entry 校验 — bcachefs `fs/journal/validate.c` 对齐
//!
//! # bcachefs 对应关系
//!
//! | bcachefs 函数 | 位置 | volmount 对应 |
//! |--------------|------|--------------|
//! | `bch2_jset_validate_early` | validate.c:748 | `Jset::verify()` (magic + CRC) |
//! | `bch2_jset_validate` | validate.c:694 | `jset_validate()` (完整校验) |
//! | `jset_validate_entries` | validate.c:662 | `jset_validate()` 中的 entry 循环 |
//! | `bch2_journal_entry_validate` | validate.c:639 | `journal_entry_validate()` |
//! | `journal_entry_{type}_validate` | validate.c:115-564 | 各 entry type 的 validate 函数 |
//!
//! # 架构差异
//!
//! bcachefs 的 validate.c 验证 C 结构体级别的 jset_entry（变长，含内联 bkey）。
//! volmount 使用固定布局 `JsetEntryHeader` + bincode payload，结构边界由 deserialize 保证。
//! 此模块在 CRC 校验之上增加语义验证：版本兼容性、seq 顺序、entry_type 字段合法性、
//! 以及各 entry type 特定的 payload 可反序列化验证。

use crate::btree::key::BtreeEntry;
use crate::journal::jset::{
    BlacklistEntry, Jset, JsetEntryType, RawJsetEntry, CSUM_TYPE_CRC32C, CSUM_TYPE_NONE,
    JSET_ENTRY_VERSION, JSET_VERSION,
};

/// 完整 Jset 校验（对应 bcachefs `bch2_jset_validate`, validate.c:694）。
///
/// 在 `Jset::verify()`（magic + CRC）通过后调用。检查：
///
/// 1. **Version 兼容** — 当前版本必须 ≤ JSET_VERSION（对应 bcachefs `!bch2_version_compatible`）
/// 2. **csum_type 合法性** — 必须是 NONE 或 CRC32C（对应 bcachefs `!bch2_checksum_type_valid`）
/// 3. **Seq 顺序** — last_seq 不能大于 seq（对应 bcachefs `jset_last_seq_newer_than_seq`）
/// 4. **entry_count 一致性** — 需与实际 entries.len() 匹配
/// 5. **逐 entry type 校验** — 委托给 `journal_entry_validate()`
pub fn jset_validate(jset: &Jset) -> bool {
    // 1. Version 兼容性（bcachefs: bch2_version_compatible）
    if jset.header.version > JSET_VERSION {
        return false;
    }

    // 2. csum_type 合法性（bcachefs: bch2_checksum_type_valid）
    if jset.header.csum_type != CSUM_TYPE_NONE && jset.header.csum_type != CSUM_TYPE_CRC32C {
        return false;
    }

    // 3. last_seq ≤ seq（bcachefs: jset_last_seq_newer_than_seq）
    if jset.header.last_seq > jset.header.seq {
        return false;
    }

    // 4. entry_count 与实际 entries 长度一致
    if jset.header.entry_count as usize != jset.entries.len() {
        return false;
    }

    // 5. 逐 entry type 校验
    for entry in &jset.entries {
        if entry.hdr.version > JSET_ENTRY_VERSION {
            return false;
        }
        if !journal_entry_validate(entry) {
            return false;
        }
    }

    true
}

/// 校验单个 JsetEntry（对应 bcachefs `bch2_journal_entry_validate`, validate.c:639）。
///
/// bcachefs 使用 ops 表（`bch2_jset_entry_ops[]`）调度到各 type 的 validate 函数。
/// volmount 使用 match 调度。未知 entry type 返回 true（向前兼容）。
fn journal_entry_validate(entry: &RawJsetEntry) -> bool {
    match JsetEntryType::from_u8(entry.hdr.entry_type) {
        Some(JsetEntryType::BtreeKeys) => btree_keys_validate(entry),
        Some(JsetEntryType::BtreeRoot) => btree_root_validate(entry),
        Some(JsetEntryType::Blacklist) => blacklist_validate(entry),
        Some(JsetEntryType::Overwrite) => overwrite_validate(entry),
        Some(JsetEntryType::BtreeNodeRewrite) => btree_node_rewrite_validate(entry),
        None => true,
    }
}

/// 校验 BtreeKeys entry（对应 bcachefs `journal_entry_btree_keys_validate`, validate.c:115）。
///
/// bcachefs 验证：各 bkey 的 fields 合法性、不超出 entry 边界、格式兼容性。
/// volmount 验证：payload 字节可反序列化为 `Vec<BtreeEntry>`。
fn btree_keys_validate(entry: &RawJsetEntry) -> bool {
    bincode::deserialize::<Vec<BtreeEntry>>(&entry.payload).is_ok()
}

/// 校验 BtreeRoot entry（对应 bcachefs `journal_entry_btree_root_validate`, validate.c:168）。
///
/// bcachefs 额外验证 root node 的 level/fields。
/// volmount 验证：payload 可反序列化且恰好包含一个 BtreeEntry。
fn btree_root_validate(entry: &RawJsetEntry) -> bool {
    match bincode::deserialize::<Vec<BtreeEntry>>(&entry.payload) {
        Ok(keys) => keys.len() == 1,
        Err(_) => false,
    }
}

/// 校验 Blacklist entry（对应 bcachefs `journal_entry_blacklist_validate`, validate.c:225）。
///
/// bcachefs 验证 seq 黑名单中的 seq 值。
/// volmount 验证：可反序列化为 `Vec<BlacklistEntry>` 且每个条目 start_seq < end_seq。
fn blacklist_validate(entry: &RawJsetEntry) -> bool {
    match bincode::deserialize::<Vec<BlacklistEntry>>(&entry.payload) {
        Ok(entries) => entries.iter().all(|bl| bl.start_seq < bl.end_seq),
        Err(_) => false,
    }
}

/// 校验 Overwrite entry（对应 bcachefs `journal_entry_overwrite_validate`, validate.c:483）。
///
/// bcachefs 验证 overwrite 范围。volmount 当前仅检查 payload 非空。
fn overwrite_validate(entry: &RawJsetEntry) -> bool {
    !entry.payload.is_empty()
}

/// 校验 BtreeNodeRewrite entry（对应 bcachefs `journal_entry_write_buffer_keys_validate`，volmount 独立）。
///
/// bcachefs write_buffer_keys validate 验证 key 指针合法性。
/// volmount 的 BtreeNodeRewrite 记录重写的 node 信息，当前仅检查 payload 非空。
fn btree_node_rewrite_validate(entry: &RawJsetEntry) -> bool {
    !entry.payload.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};
    use crate::journal::jset::{Jset, JsetEntryHeader, JsetHeader, RawJsetEntry, CSUM_TYPE_CRC32C};

    fn make_valid_jset() -> Jset {
        let mut jset = Jset::new(1, 0);
        jset.header.csum_type = CSUM_TYPE_CRC32C;
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1),
        )])
        .unwrap();
        let entry = RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload).unwrap();
        jset.entries.push(entry);
        jset.header.entry_count = 1;
        jset
    }

    #[test]
    fn test_jset_validate_ok() {
        let jset = make_valid_jset();
        assert!(jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_version_too_high() {
        let mut jset = make_valid_jset();
        jset.header.version = JSET_VERSION + 1;
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_unknown_csum_type() {
        let mut jset = make_valid_jset();
        jset.header.csum_type = 99;
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_last_seq_greater_than_seq() {
        let mut jset = make_valid_jset();
        jset.header.last_seq = 5;
        jset.header.seq = 3;
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_entry_count_mismatch() {
        let mut jset = make_valid_jset();
        jset.header.entry_count = 2; // but only 1 entry
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_btree_keys_corrupt() {
        let mut jset = make_valid_jset();
        jset.entries[0].payload = vec![0xDE, 0xAD]; // garbage bytes
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_btree_root_ok() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::BtreeRoot as u8;
        // Single entry is valid for BtreeRoot
        assert!(jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_btree_root_empty() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::BtreeRoot as u8;
        jset.entries[0].payload = bincode::serialize::<Vec<BtreeEntry>>(&vec![]).unwrap();
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_blacklist_ok() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::Blacklist as u8;
        jset.entries[0].payload = bincode::serialize(&vec![
            BlacklistEntry {
                start_seq: 1,
                end_seq: 10,
            },
            BlacklistEntry {
                start_seq: 10,
                end_seq: 20,
            },
        ])
        .unwrap();
        assert!(jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_blacklist_bad_range() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::Blacklist as u8;
        jset.entries[0].payload = bincode::serialize(&vec![
            BlacklistEntry {
                start_seq: 10,
                end_seq: 5,
            }, // start > end
        ])
        .unwrap();
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_overwrite_ok() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::Overwrite as u8;
        jset.entries[0].payload = vec![0u8; 8]; // non-empty
        assert!(jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_overwrite_empty() {
        let mut jset = make_valid_jset();
        jset.entries[0].hdr.entry_type = JsetEntryType::Overwrite as u8;
        jset.entries[0].payload = vec![]; // empty
        assert!(!jset_validate(&jset));
    }

    #[test]
    fn test_jset_validate_empty_entries() {
        let jset = Jset::new(1, 0);
        assert!(jset_validate(&jset)); // empty entries is valid
    }

    #[test]
    fn test_jset_validate_verify_then_validate() {
        // Integration: verify() passes + validate() passes for valid jset
        let mut jset = make_valid_jset();
        use crate::journal::jset::crc32c;
        jset.header.crc32 = 0;
        let mut header_zero = jset.header;
        header_zero.crc32 = 0;
        let mut crc = crc32c(
            unsafe {
                std::slice::from_raw_parts(
                    &header_zero as *const JsetHeader as *const u8,
                    std::mem::size_of::<JsetHeader>(),
                )
            },
            0,
        );
        for entry in &jset.entries {
            crc = crc32c(
                unsafe {
                    std::slice::from_raw_parts(
                        &entry.hdr as *const JsetEntryHeader as *const u8,
                        std::mem::size_of::<JsetEntryHeader>(),
                    )
                },
                crc,
            );
            if !entry.payload.is_empty() {
                crc = crc32c(&entry.payload, crc);
            }
        }
        jset.header.crc32 = crc;

        assert!(jset.verify());
        assert!(jset_validate(&jset));
    }
}
