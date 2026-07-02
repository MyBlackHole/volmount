# Snapshot Block Device

## Execution Plan

1. Rename the public block-device surface
   - Replace `/api/v1/volumes/...` routes with `/api/v1/blocks/...`
   - Update request/response names in the daemon handlers
   - Update daemon status payloads and tests to use blocks terminology

2. Rename disk/config layout
   - Replace `volumes_dir` / `volume_backend_path` with blocks naming
   - Update config defaults and path helpers
   - Rename any persisted directory assumptions in daemon tests

3. Keep sparse backend as the only supported backend for this task
   - Gate the new flow to sparse backend creation/opening
   - Leave existing S3/NFS implementation code untouched unless it blocks compilation
   - Update validation to reject backends outside the supported first-release scope

4. Add COW snapshot/clone semantics
   - Reuse existing btree snapshot machinery as the metadata authority
   - Add clone-from-snapshot flow that creates a new block resource from a snapshot root
   - Ensure write paths on the clone diverge without mutating the source block
   - Verify the shared block pool / refcount path does not free data still reachable from any clone

5. Update lifecycle operations
   - Ensure create / delete / mount / umount / snapshot / rollback all use block naming
   - Ensure cloned blocks behave like independent resources in the daemon registry
   - Keep NBD export semantics unchanged aside from resource naming

6. Update tests
   - Rewrite daemon integration tests to call `/api/v1/blocks/...`
   - Add clone + COW verification tests
   - Add persistence tests for reopen after snapshot and clone
   - Add multi-block isolation tests

## Validation

- `cargo fmt --all`
- `cargo test -p volmountd --tests`
- `cargo test -p volmount-core --lib`
- `cargo test -p volmountd --tests -- --nocapture`

## Risky Areas

- `crates/volmountd/src/server.rs`
- `crates/volmountd/src/volume.rs`
- `crates/volmountd/tests/{functional,integration}.rs`
- `crates/volmount-core/src/block_device/*`
- block-layout config / path helpers

## Acceptance Checks

- Only `/api/v1/blocks/...` remains on the public HTTP surface
- Block resources remain independent at the API level
- Snapshot and clone behavior is COW, not full-copy
- Sparse backend persists state across reopen
- No old `volumes` compatibility alias remains
