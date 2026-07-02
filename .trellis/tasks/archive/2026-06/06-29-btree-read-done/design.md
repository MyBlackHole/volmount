# Key Cache Journal Flush — 设计文档

## 现状分析

### 当前架构

```
KeyCache::pin_entry()
  └→ Journal::bch2_journal_pin_add_seq(seq, callback)
       └→ 递增 pin_list.count（忽略 callback）
       └→ callback 永不执行（`_callback` 前缀未使用）

脏条目写回路径：
JournalReclaim → journal_flush_pins()
  └→ 遍历 pins → 调用 flush_fn
  └→ key cache 条目没有注册 flush_fn（因为 `_seq` API 忽略 callback）

实际写回：
外部代码 → BtreeEngine::flush_cache_dirty_keys()
  └→ KeyCache::flush_dirty(writer)
       └→ collect_dirty() → writer() → mark_clean()
```

**问题根本原因**: `bch2_journal_pin_add_seq()` (types.rs:1254) 是一个过渡 API，设计目的是在子系统嵌入 `JournalEntryPin` 之前的临时方案。所有使用 `_seq` API 的子系统，其 callback 被完全忽略，journal reclaim 无法驱动 flush。

### bcachefs 架构

```
bch2_btree_insert_key_cached()
  └→ bch2_journal_pin_add(j->seq, &ck->journal, bch2_btree_key_cache_journal_flush)
       └→ 注册 pin + flush callback

Journal reclaim
  └→ journal_flush_pins()
       └→ bch2_btree_key_cache_journal_flush(j, pin, seq)
            └→ 如果 ck 仍脏且 seq 匹配 → btree_key_cache_flush_pos()
```

## 改造方案

### 目标架构

```
CachedEntry 嵌入 JournalEntryPin struct {
    link: Link,       // 侵入式链表（unflushed/flushed）
    seq: AtomicU64,   // journal seq
    flush: Option<JournalPinFlushFn>,   // callback
}

KeyCache::pin_entry()
  └→ Journal::bch2_journal_pin_add(seq, &entry.pin, flush_fn)
       └→ 跟踪 pin 生命周期 + 注册 flush callback

JournalReclaim → journal_flush_pins()
  └→ flush_fn(j, pin, seq)
       └→ 解析 pin → &CachedEntry
            └→ 设置 flush_pending = true

外部代码 → flush_cache_dirty_keys()
  └→ flush_dirty(writer)
       └→ 写回 → mark_clean() → bch2_journal_pin_drop()
```

### 变更步骤

#### Step 1: CachedEntry 嵌入 JournalEntryPin

```rust
struct CachedEntry {
    pin: JournalEntryPin,          // NEW — 替换 journal_seq: AtomicU64
    lock: RwLock<BtreeEntry>,
    valid: AtomicBool,
    dirty: AtomicBool,
    flush_pending: AtomicBool,
}
```

`JournalEntryPin` 在 `CachedEntry::new()` 中构造为未激活 (seq=0, flush=None)。在 `pin_entry()` 中通过 `bch2_journal_pin_add()` 激活。

#### Step 2: 注册 flush callback

`pin_entry()` 从 `bch2_journal_pin_add_seq()` 改为 `bch2_journal_pin_add()`：

```rust
fn pin_entry(&self, entry: &Arc<CachedEntry>, journal_seq: u64) {
    if journal_seq == 0 { return; }
    let Some(j) = self.journal.get().and_then(|w| w.upgrade()) else { return; };
    
    // 创建 weak 引用供 callback 捕获
    let ck_weak = Arc::downgree(entry);
    j.bch2_journal_pin_add(
        journal_seq,
        &entry.pin,
        Some(Box::new(move |_j: &Journal, pin: &JournalEntryPin, _seq: u64| {
            // 从 JournalEntryPin 指针反算 CachedEntry 地址
            let ck = unsafe {
                let pin_offset = memoffset::offset_of!(CachedEntry, pin);
                let ck_ptr = (pin as *const JournalEntryPin as *const u8).sub(pin_offset) as *const CachedEntry;
                &*ck_ptr
            };
            ck.flush_pending.store(true, Ordering::Release);
        })),
    );
}
```

**注意**: 使用 `memoffset::offset_of!()` 或等效方法获取 `pin` 字段在 `CachedEntry` 中的偏移。这对应 bcachefs 的 `container_of()` 宏。

#### Step 3: Cleanup 路径

`drop_journal_pin()` 从 `bch2_journal_pin_drop_seq()` 改为 `bch2_journal_pin_drop()`：

```rust
fn drop_journal_pin(&self, entry: &CachedEntry) {
    if !entry.pin.is_active() { return; }
    if let Some(j) = self.journal.get().and_then(|w| w.upgrade()) {
        // 使用 journal 的 pin_drop 方法
    }
}
```

需要在 `Journal` 上暴露 `bch2_journal_pin_drop()` 方法（或直接访问）。

#### Step 4: 移除 journal_seq 字段

`CachedEntry.journal_seq: AtomicU64` 被 `CachedEntry.pin: JournalEntryPin` 替代（pin.seq 承载相同信息）。清理所有使用 `journal_seq` 的代码路径。

### 安全考虑

#### `container_of` 模式安全性

```rust
// SAFETY: 
// 1. pin 是 CachedEntry 的嵌入字段，生命周期（in pin list）≤ CachedEntry (Arc) 生命周期
// 2. callback 通过 Arc::downgrade 捕获，pin 被提取前不能 drop
// 3. flush callback 中只设 AtomicBool，不访问保活数据
```

但 `container_of` 的裸指针解引用在 Rust 中有 UB 风险。更安全的替代方案：在 callback 的捕获中使用 `Weak<CachedEntry>` 而非指针算术。

但是 `JournalPinFlushFn` 签名是 `fn(&Journal, &JournalEntryPin, u64)`，只给了 `&JournalEntryPin`，没有给用户数据指针。

方案 A（不安全但对齐 bcachefs）: `container_of` 直接从 pin 指针反算 CachedEntry
方案 B（安全但额外分配）: 在 heap 上分配一个小 wrapper 持有 `Weak<CachedEntry>`

**推荐**: 方案 A（与 bcachefs 一致） + `unsafe` 块加详细安全注释。

#### Drop 安全性

`CachedEntry` 的 Drop 实现必须确保 `JournalEntryPin` 已从 pin list 中移除。但 `CachedEntry` 是 `Arc<CachedEntry>`，Drop 在 `Arc` 计数归零时触发。这要求 `mark_clean()` / `invalidate()` 在 `Arc` 释放前已调用 `bch2_journal_pin_drop()`。

当前 `CachedEntry` 没有自定义 `Drop`，但 `pin` 字段会在 `Arc` 释放时自动 drop。`JournalEntryPin` 的 `Link` 如果仍挂在链表上会导致 use-after-free。

**方案**: 确保所有可能 drop `CachedEntry` 的路径（`invalidate`、`mark_clean`、正常释放）都先调用 `drop_journal_pin()`。

### 与 Existing API 的兼容

`bch2_btree_key_cache_journal_flush()` 静态方法（line 363）保持为公开 stub 或改为 callback 函数（如果被外部代码引用）。当前未被调用，可安全移除或保留为转发函数。

## Rollback 方案

如果实现后出现回归：
1. 恢复 `CachedEntry` 的 `journal_seq` 字段
2. 恢复 `pin_entry()` 使用 `bch2_journal_pin_add_seq()`
3. 恢复 `drop_journal_pin()` 使用 `bch2_journal_pin_drop_seq()`

## 测试策略

- 现有 `test_journal_pin_integration()` 和 `test_journal_pin_with_instance()` 需要更新以验证新绑定的 pin
- 新增 `test_journal_flush_callback_fires()` — 验证 journal_flush_pins 触发 key cache 的 flush callback
- 现有 `test_flush_dirty_callback()` / `test_flush_callback_triggers()` 保持不变
