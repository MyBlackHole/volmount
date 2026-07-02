# read_done checksum alignment

## Goal

Align the btree read path with bcachefs checksum handling by making the checksum gate explicit in the read pipeline and documenting where the validation happens in volmount.

## Requirements

- Read-side node loading must reject corrupted initial or append records before the node reaches the post-read validation pipeline.
- The checksum validation path must be shared and traceable from the load boundary, not only implied by lower-level parsing.
- Existing `read_done` structural validation and key sorting behavior must remain unchanged.
- The bcachefs comparison note in `quality-guidelines.md` must reflect the actual placement of checksum validation and cite the source reference.
- Coverage must include a corruption test for the read path, not only the serializer/deserializer boundary.

## Acceptance Criteria

- [ ] Corrupting the initial btree record causes the read path to fail with a checksum-related error.
- [ ] Corrupting an appended btree record causes the read path to fail with a checksum-related error.
- [ ] `read_done` still passes on valid round-trip data and keeps the existing bset validation + sort behavior.
- [ ] `.trellis/spec/backend/quality-guidelines.md` no longer tracks `bset checksum` as an unresolved low-priority gap, and the note points to the verified implementation location.
- [ ] `cargo test -p volmount-core --lib btree::io` passes.

## Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
- Lightweight tasks can remain PRD-only.
- For complex tasks, add `design.md` for technical design and `implement.md` for execution planning before `task.py start`.
