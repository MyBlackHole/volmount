# Phase 1: Journal P0 修复

## Goal

确认 Journal 层 P0 bcachefs 不一致问题的当前状态，处理剩余问题。

## 现状调查

### P0-1 (find_free_buf idx++) — ✅ 已由前期工作修复
`types.rs:1334` 已使用 `idx = ((old_idx + 1) & (BUF_NR - 1))` 轮转模式，并有 `debug_assert_eq!` 验证 `idx == new_seq & BUF_MASK`。

### P0-2 (CLOSED_VAL 初始化) — ✅ 已由前期工作修复
`JournalResState::new()` 初始化为 `JOURNAL_ENTRY_CLOSED_VAL`，有完整的 open/close 状态转换。

### 其他潜在 Journal P0
- `JOURNAL_ENTRY_BLOCKED_VAL` / `JOURNAL_ENTRY_ERROR_VAL` sentinel 是否缺失？
- journal_reclaim / journal_flush 是否有其他 P0 差距？
- `bch2_journal_seq_verify` 是否已实现？

## Out of Scope
- 非 Journal 层的 P0 修复（Phase 2-5）

## Acceptance Criteria

- [ ] 确认所有 Journal 层 P0 问题已修复，或记录为已知差距
