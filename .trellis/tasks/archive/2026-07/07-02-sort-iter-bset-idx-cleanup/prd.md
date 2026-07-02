# sort_iter bset_idx cleanup

## Goal

Remove the unused `bset_idx` field from `SortIterEntry` so the Rust `sort_iter` shape matches the bcachefs source more closely.

## Requirements

- `SortIterEntry` must only store the offsets required for key sorting.
- `SortIter::add` and its call sites must no longer pass or store a `bset_idx` value.
- Existing `sort_iter` ordering, deduplication, and `read_done` / write-path behavior must remain unchanged.
- The backend quality guideline entry for `sort_iter bset_idx unused` must be updated to reflect the resolved state.

## Acceptance Criteria

- [ ] `crates/volmount-core/src/btree/io.rs` no longer contains an unused `bset_idx` field in `SortIterEntry`.
- [ ] `sort_iter` tests still pass and preserve the same ordering behavior.
- [ ] `.trellis/spec/backend/quality-guidelines.md` no longer tracks `sort_iter bset_idx unused` as a low-priority gap.
- [ ] `cargo test -p volmount-core --lib btree::io` passes.

## Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
- Lightweight tasks can remain PRD-only.
- For complex tasks, add `design.md` for technical design and `implement.md` for execution planning before `task.py start`.
