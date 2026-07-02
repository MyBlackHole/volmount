# Key Cache Journal Flush — 执行计划

## Overview

将 `KeyCache::pin_entry()` 从 transitional `_seq` API 迁移到 proper `JournalEntryPin` API，使 journal reclaim 能驱动 key cache flush。

## 实施步骤

### Step 1: CachedEntry 嵌入 JournalEntryPin

**文件**: `crates/volmount-core/src/btree/key_cache.rs`

- 在 `CachedEntry` struct 中添加 `pin: JournalEntryPin` 字段（替换 `journal_seq: AtomicU64`）
- 更新 `CachedEntry::new()` 初始化 `pin: JournalEntryPin::new(None)`（未激活）
- 导入 `use crate::journal::reclaim::JournalEntryPin`

**风险**: `JournalEntryPin` 包含侵入式链表 `Link` + `AtomicU64` + `Option<Box<dyn Fn ...>>`。确保不影响 CachedEntry 的 `Send + Sync` 实现。

### Step 2: 替换 pin_entry 使用新 API

**文件**: `crates/volmount-core/src/btree/key_cache.rs`

```rust
// 修改前:
fn pin_entry(&self, entry: &Arc<CachedEntry>, journal_seq: u64) {
    ...
    j.bch2_journal_pin_add_seq(journal_seq, Box::new(move || {
        if let Some(ck) = ck_weak.upgrade() {
            ck.flush_pending.store(true, Ordering::Release);
        }
    }));
}

// 修改后:
fn pin_entry(&self, entry: &Arc<CachedEntry>, journal_seq: u64) {
    ...
    let ck_weak = Arc::downgrade(entry);
    j.bch2_journal_pin_add(
        journal_seq,
        &entry.pin,
        Some(Box::new(move |_j: &Journal, _pin: &JournalEntryPin, _seq: u64| {
            if let Some(ck) = ck_weak.upgrade() {
                ck.flush_pending.store(true, Ordering::Release);
            }
        })),
    );
}
```

### Step 3: 替换 drop_journal_pin 使用新 API

**文件**: `crates/volmount-core/src/btree/key_cache.rs`

```rust
// 修改前:
fn drop_journal_pin(&self, entry: &CachedEntry) {
    let jseq = entry.journal_seq.load(Ordering::Acquire);
    if jseq == 0 { return; }
    if let Some(j) = self.journal.get().and_then(|w| w.upgrade()) {
        j.bch2_journal_pin_drop_seq(jseq);
    }
}

// 修改后:
fn drop_journal_pin(&self, entry: &CachedEntry) {
    if !entry.pin.is_active() { return; }
    if let Some(j) = self.journal.get().and_then(|w| w.upgrade()) {
        j.bch2_journal_pin_drop(&entry.pin);
    }
}
```

### Step 4: 更新 bch2_btree_key_cache_journal_flush

**文件**: `crates/volmount-core/src/btree/key_cache.rs`

更新为可导出的 flush callback 函数（与 bcachefs 对齐命名），可从 `JournalPinFlushFn` 引用。

```rust
/// bcachefs 对齐: bch2_btree_key_cache_journal_flush
/// 由 journal reclaim 调用，触发对应脏条目的 flush_pending 标志
pub fn bch2_btree_key_cache_journal_flush(
    _j: &Journal,
    pin: &JournalEntryPin,
    _seq: u64,
) {
    // 从 JournalEntryPin 反算 CachedEntry
    let offset = std::mem::offset_of!(CachedEntry, pin);
    let ck_ptr = unsafe {
        let pin_addr = pin as *const JournalEntryPin as *const u8;
        (pin_addr.sub(offset)) as *const CachedEntry
    };
    // SAFETY: pin 是 CachedEntry 的嵌入字段，Arc 保持其存活
    // 在 callback 被调用时，pin 在 unflushed 链表中，CachedEntry 必须存活
    unsafe { &*ck_ptr }.flush_pending.store(true, Ordering::Release);
}
```

### Step 5: 移除 journal_seq 字段

**文件**: `crates/volmount-core/src/btree/key_cache.rs`

- 删除 `CachedEntry.journal_seq: AtomicU64` 字段
- `new()` 中移除 `journal_seq: AtomicU64::new(journal_seq)` — pin 在 pin_entry 中被激活
- `insert()` 中移除 `arc.journal_seq.store(0, ...)` — pin 的 seq 由 JournalEntryPin 管理
- `bch2_btree_insert_key_cached()` 中移除 `arc.journal_seq.store(journal_seq, ...)` — 同上
- 新条目 `CachedEntry { journal_seq: AtomicU64::new(journal_seq), ... }` → 移除该行

### Step 6: 更新测试

**文件**: `crates/volmount-core/src/btree/key_cache.rs` (tests section)

- `test_journal_seq()` — 验证 pin.seq 而非 journal_seq
- `test_journal_pin_integration()` — 更新为使用 `pin.is_active()`
- `test_journal_pin_with_instance()` — 更新为使用 `pin.is_active()`

## 验证命令

```bash
# 每次 step 完成后
cargo test -p volmount-core --lib -- btree::key_cache 2>&1
cargo clippy --all-targets 2>&1
```

## 回滚点

- Commit step 1-2 作为 checkpoint
- 如果 `cargo test` 失败且修复未超过 2 次尝试，回滚到上一 checkpoint

## 风险

1. **Arc 生命周期**: `CachedEntry` 在 pin 还在链表中时可能被 drop。
   - 防护: `invalidate()` 和 `mark_clean()` 先调用 `drop_journal_pin()` 再清除有效数据。Arc 不会在 pin 激活时释放。
   - 但若外部代码持有 Arc 并在未调用 invalidate 的情况下 drop，pin 可能挂在链表上。
   - 缓解: 添加 `impl Drop for CachedEntry` 中确保 pin 已被清理。

2. **offset_of! 安全性**: `container_of` 使用的裸指针加减法要求 `CachedEntry: Sized` 且 `pin` 字段未被打乱。
   - `#[repr(C)]` 可确保字段布局确定性，但 CachedEntry 当前无此属性。
   - `memoffset::offset_of!()` 或 `std::mem::offset_of!()` 不受 repr 约束。
