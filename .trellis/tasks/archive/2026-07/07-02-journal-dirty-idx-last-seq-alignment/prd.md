# journal dirty idx last_seq alignment

## Goal

Align journal dirty bucket advancement with bcachefs by using `last_seq` as the reclaim boundary instead of the current in-memory `seq`.

## Requirements

- `advance_dirty_idx()` must use the journal's last flushed sequence boundary, not the current open sequence, when deciding whether a bucket can become clean.
- The existing `dirty_idx_ondisk` logic must remain keyed off `last_seq_ondisk`.
- The cleanup must preserve the current wraparound behavior and the `discard_idx <= dirty_idx_ondisk <= dirty_idx <= cur_idx` invariant.
- Update the backend quality guideline entry that currently describes this as an acknowledged divergence.
- Add or adjust tests to demonstrate the difference between `last_seq` and `cur_seq` behavior.

## Acceptance Criteria

- [ ] `advance_dirty_idx()` compares against `last_seq` and not `journal_cur_seq()`.
- [ ] Existing dirty-index tests still pass, and there is a regression test covering the new boundary behavior.
- [ ] `.trellis/spec/backend/quality-guidelines.md` no longer marks dirty-index advancement as an acknowledged divergence.
- [ ] `cargo test -p volmount-core --lib journal::types` passes.

## Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
- Lightweight tasks can remain PRD-only.
- For complex tasks, add `design.md` for technical design and `implement.md` for execution planning before `task.py start`.
