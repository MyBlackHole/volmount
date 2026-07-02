# Alloc Coverage

> Alloc module coverage map for bucket state, freespace, and alloc-info work.

## Overview

This map tracks the alloc/freespace consistency helpers that recovery depends on.
The new alloc-info helper cross-checks allocator state against the Alloc and
Freespace btrees. The bucket generation index is modeled as a separate
auxiliary btree with 256 generation slots per key.
Recent follow-up work also aligned the alloc entry payload with bcachefs-style
metadata used by read-LRU and backpointer accounting.

## Function Coverage Map

| Our Function | bcachefs Counterpart | Reference | Status |
|---|---|---|---|
| `bch2_check_allocations()` | `bch2_check_allocations()` | `fs/alloc/check.c:630-684` | ✅ |
| `bch2_check_alloc_info()` | `bch2_check_alloc_info()` | `fs/alloc/check.c:630-684` | ✅ |
| `bch2_fs_freespace_init()` | `bch2_fs_freespace_init()` | `fs/alloc/check.c:754-760` / freespace init path | ✅ |
| `bch2_rebuild_freespace()` | `bch2_recalc_freespace()` | `fs/alloc/check.c:754-760` + freespace rebuild path | ✅ |
| `bch2_trigger_alloc_freespace()` | alloc-trigger freespace sync | `fs/data/ec/trigger.c` / alloc trigger path | ✅ |
| `alloc_lru_idx_read()` | alloc read-LRU accessor | `fs/alloc/background.h` / `fs/alloc/lru.c` | ✅ |
| `alloc_lru_idx_fragmentation()` | alloc fragmentation-LRU accessor | `fs/alloc/background.h` / `fs/alloc/lru.c` | ✅ |
| `alloc_nr_external_backpointers()` | alloc backpointer count accessor | `fs/alloc/background.h` / `fs/alloc/backpointers.c` | ✅ |
| `serialize_alloc_entry()` | alloc entry write path helper | `fs/alloc/foreground.c` / `fs/alloc/check.c` | ✅ |
| `deserialize_alloc_entry()` | alloc entry read path helper with legacy fallback | `fs/alloc/foreground.c` / `fs/alloc/check.c` | ✅ |

## Notes

- The alloc-info helper treats free buckets as requiring an exact matching
  freespace generation and treats any freespace entry for an allocated bucket as
  stale.
- `bucket_gens` keys are chunked in groups of 256 buckets. The value payload
  stores one `u8` generation per bucket slot, matching bcachefs
  `KEY_TYPE_BUCKET_GENS_BITS = 8`.
- `BchAllocEntry` now derives `PartialEq/Eq` so alloc-btree snapshots can be
  compared directly against the in-memory allocator state.
- `BchAllocEntry` carries the extra `io_time_read` and
  `nr_external_backpointers` fields in addition to the existing bucket state
  and `group` tail field. New writes must serialize through
  `serialize_alloc_entry()`; legacy 7-field payloads must read through
  `deserialize_alloc_entry()` so old data defaults the new fields to zero.

## Scenario: alloc entry compatibility and read-LRU helpers

### 1. Scope / Trigger
- Trigger: alloc metadata shape changed to carry read-LRU and backpointer count
  fields, and the same payload is used by alloc write/read paths across
  foreground, recovery, and GC code.

### 2. Signatures
- `BchAllocEntry { io_time_read: u64, nr_external_backpointers: u32, group: u32, ... }`
- `serialize_alloc_entry(entry: &BchAllocEntry) -> Result<Vec<u8>, bincode::Error>`
- `deserialize_alloc_entry(bytes: &[u8]) -> Result<BchAllocEntry, StorageError>`
- `alloc_lru_idx_read(entry: &BchAllocEntry) -> u64`
- `alloc_lru_idx_fragmentation(entry: &BchAllocEntry, bucket_size: u64) -> u64`
- `alloc_nr_external_backpointers(entry: &BchAllocEntry) -> u32`

### 3. Contracts
- New alloc entries must preserve the new fields in every write path that emits
  Alloc-btree values.
- Read paths must accept both the current 9-field layout and the legacy
  7-field layout.
- Legacy payloads default `io_time_read` and `nr_external_backpointers` to zero.
- `alloc_lru_idx_read()` is only meaningful for cached data buckets.
- `alloc_lru_idx_fragmentation()` is only meaningful for movable data buckets.

### 4. Validation & Error Matrix
- Missing trailing fields in legacy alloc payload -> decode via compatibility
  fallback, not a hard failure.
- Non-movable or non-cached bucket -> helper returns zero and does not invent
  an LRU score.
- Serialize failure -> caller bubbles `AllocError::Serialization` or the
  equivalent storage error.

### 5. Good/Base/Bad Cases
- Good: new alloc entry is written with helper serialization and later decoded
  with the same helper.
- Base: old alloc entry bytes from before the field expansion still load and
  fill the new fields with zero.
- Bad: calling bare `bincode::deserialize::<BchAllocEntry>()` on persisted
  alloc bytes after the field expansion.

### 6. Tests Required
- Unit test for round-tripping a current alloc entry through
  `serialize_alloc_entry()` / `deserialize_alloc_entry()`.
- Unit test for decoding a legacy 7-field payload and asserting new fields are
  zero.
- Unit tests for read-LRU, fragmentation-LRU, and backpointer accessor return
  values.

### 7. Wrong vs Correct
#### Wrong
```rust
let entry: BchAllocEntry = bincode::deserialize(bytes)?;
```

#### Correct
```rust
let entry = deserialize_alloc_entry(bytes)?;
let raw = serialize_alloc_entry(&entry)?;
```
