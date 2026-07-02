# Journal Reclaim (Pin API) — 代码规范

## 模式：UnsafeCell 实现 `&self` 的内变性

**问题**：`PinListFifo`（ring buffer）需要在 `journal_entry_open(&self)` 中 `push_back`，但 retry loop 中 `entry_for_seq` 是只读的。对方存取 `&mut self` 意味着所有 `&self` 方法无法调用 `journal_entry_open`。

**方案**：将 `pin_fifo` 字段声明为 `UnsafeCell<PinListFifo>`。

```rust
pub(crate) pin_fifo: UnsafeCell<PinListFifo>,
```

**安全前提**：
- `push_back` 只在 `journal_entry_open()` 中发生（被 journal 生命周期序列化——entry open/close 不并发）。
- 所有其他路径（retry loop、`__bch2_journal_pin_put`）均为只读。

**读访问模式**（仅 reclaim.rs）：

```rust
fn pin_fifo_ref(&self) -> &PinListFifo {
    unsafe { &*self.pin_fifo.get() }
}
```

**写访问模式**（仅 types.rs）：

```rust
unsafe {
    (*self.pin_fifo.get()).push_back(JournalEntryPinList::new(1)).unwrap();
}
```

> **Gotcha**：pin_fifo 不是 `Mutex` 也不是 `RwLock`。不要试图在 retry loop 中持有 pin_fifo 引用的同时再去拿 pin_list 的锁——这会导致编译期借用链错误。用 `UnsafeCell` + `pin_fifo_ref()` 避免此问题。

---

## 模式：`_seq` 过渡 API

**问题**：外部调用者（`btree/*.rs`、`volume/mod.rs`）尚未嵌入 `JournalEntryPin` 对象，只能传递 `u64 seq`。新的 pin API 接受 `&JournalEntryPin`。

**方案**：在 `types.rs` 中保留 `_seq` 后缀的过渡方法。

```rust
// types.rs — 过渡方法
pub fn bch2_journal_pin_set_seq(&self, seq: u64) {
    if let Some(pl) = self.pin_fifo_ref().entry_for_seq(seq) {
        pl.count.fetch_add(1, Ordering::Release);
    }
}

pub fn bch2_journal_pin_drop_seq(&self, seq: u64) {
    self.__bch2_journal_pin_put(seq);
}
```

**过渡方法 vs 新 API**：

| 操作 | 过渡（_seq） | 新 API |
|------|--------------|--------|
| pin_set | `pin_set_seq(u64)` 只递增 count | `pin_set(seq, &pin, fn)` 完整语义 |
| pin_drop | `pin_drop_seq(u64)` 递减 count | `pin_drop(&pin)` 移除 pin |
| pin_add | `pin_add_seq(u64, Box<Fn>)` 回调忽略 | `pin_add(seq, &pin, fn)` 完整语义 |

**迁移路径**：当 `btree/node.rs` 中 `BtreeNode` 嵌入 `JournalEntryPin` 字段后，调用者改为 `journal.bch2_journal_pin_set(seq, &node.pin, flush_fn)`，删除 `_seq` 方法。

---

## 模式：方法重命名（`_aligned` 后缀 → 正式名称）

**问题**：过渡期新 API 方法带 `_aligned` 后缀（如 `bch2_journal_pin_set_aligned`）以避开旧 stub 的方法名冲突。Step 4 删除旧 stub 后，重命名为正式名称。

```rust
// Step 3（过渡）:
pub fn bch2_journal_pin_set_aligned(&self, new_seq: u64, pin: &JournalEntryPin, flush_fn: Option<JournalPinFlushFn>)

// Step 4（最终）:
pub fn bch2_journal_pin_set(&self, new_seq: u64, pin: &JournalEntryPin, flush_fn: Option<JournalPinFlushFn>)
```

**重命名清单**：

| 过渡名 | 正式名 |
|--------|--------|
| `bch2_journal_pin_set_aligned` | `bch2_journal_pin_set` |
| `bch2_journal_pin_drop_aligned` | `bch2_journal_pin_drop` |
| `bch2_journal_pin_add_aligned` | `bch2_journal_pin_add` |
| `bch2_journal_pin_update_aligned` | `bch2_journal_pin_update` |
| `bch2_journal_pin_copy_aligned` | `bch2_journal_pin_copy` |
| `bch2_journal_pin_flush_aligned` | `bch2_journal_pin_flush` |

---

## 关键 API 签名

### JournalPinFlushFn — callback 错误通道

**签名变更 (2026-07-01)**：从 `()` → `Result<(), StorageError>`。

```rust
pub type JournalPinFlushFn = Box<dyn Fn(&Journal, &JournalEntryPin, u64) -> Result<(), StorageError> + Send>;
```

**理由**：对齐 bcachefs C 的 `journal_pin_flush_fn`（void 函数）。volmount 扩展为 `Result` 以传播 flush callback 中的存储错误（如 btree 写入失败）。

### JournalEntryPin.flush — callback 必须可持久保存

**问题**：`bch2_journal_pin_add()` 和 `bch2_journal_pin_set()` 都可能通过参数传入 flush callback；如果 callback 只停留在局部变量里，后续 `journal_flush_pins()` 就拿不到它。

**方案**：`JournalEntryPin.flush` 使用 `Mutex<Option<JournalPinFlushFn>>`，并在 `bch2_journal_pin_set_locked()` 中只在收到 `Some(flush_fn)` 时覆盖现有值。

**结果**：
- `bch2_journal_pin_add(..., Some(callback))` 会把 callback 挂到 pin 上
- `bch2_journal_pin_set(..., None)` 不会清掉已有 callback
- `journal_flush_pins()` 通过锁读取 callback，再调用

### journal_flush_pins — 类型优先级顺序

**问题**：同一 `seq` 下可能同时存在多个 pin type。若直接按插入顺序 flush，reclaim 顺序会偏离 bcachefs。

**方案**：按 bcachefs `journal_flush_done()` 的类型优先级遍历：
`Other → KeyCache → Btree0 → Btree1 → Btree2 → Btree3`

**结果**：
- 同一 `seq` 的多个 pin 会按该顺序依次 flush
- btree 类 pin 保持 leaf-to-root 的相对顺序
- key cache pin 不再依赖插入先后

### bch2_journal_pin_set

```rust
pub fn bch2_journal_pin_set(&self, new_seq: u64, pin: &JournalEntryPin, flush_fn: Option<JournalPinFlushFn>)
```

- 多步 retry loop：read old_seq → lock pin_lists（序化）→ recheck → mutate → unlock
- Lock order：按 `min(old_seq, new_seq)` → `max(old_seq, new_seq)`（防死锁）
- 如果 old_seq == new_seq，只锁一次

### bch2_journal_pin_drop

```rust
pub fn bch2_journal_pin_drop(&self, pin: &JournalEntryPin)
```

- 读取 `pin.seq` → 锁 pin_list → recheck → remove → `fetch_sub(1)` → `pin.seq = 0`
- 如果 `flush_in_progress == pin`，设置 `flush_in_progress_dropped = true`

### bch2_journal_pin_copy

```rust
pub fn bch2_journal_pin_copy(&self, dst: &JournalEntryPin, src: &JournalEntryPin, flush_fn: Option<JournalPinFlushFn>)
```

- `dst` 继承 `src` 的 seq，不分配到新 seq
- 内部调用 `journal_pin_set_locked(dst_l=None, new_l=src_l, pin=dst, seq=src_seq)`

---

## 测试

新 pin API 的 12 个单元测试在 `reclaim.rs` 的 `#[cfg(test)] mod tests` 中：

1. `test_pin_set_drop` — 基本 set + drop
2. `test_pin_set_put` — set 后 `__bch2_journal_pin_put` 递减
3. `test_multi_pin_same_seq` — 多 pin 共享 seq
4. `test_pin_update_seq_forward` — 迁移到新 seq
5. `test_pin_copy` — 复制 pin
6. `test_pin_add_conditional` — 条件设置（inactive 或 seq 后退）
7. `test_pin_drop_inactive` — 无操作处理
8. `test_flush_in_progress_dropped` — 竞争标记
9. `test_pin_update_noop` — 相同 seq 不操作
10. `test_btree_level_pin_type_mapping` — btree 层级到 pin type 的映射
11. `test_journal_pin_type_reads_stored_metadata` — 从 pin 元数据读取分类
12. `test_flush_pins_prefers_bcachefs_type_order_within_seq` — 同 seq 下按 bcachefs 顺序 flush
13. `btree::io::tests::test_btree_node_write_sets_level_pin_type` — btree 写路径覆盖 leaf 与高层 interior 的 pin type
14. `test_flush_pins_orders_key_cache_before_btree_bucket` — 同 seq 下 key cache 先于 btree bucket flush

**辅助函数**：`create_test_journal(entry_count)` 通过 UnsafeCell 直接向 pin_fifo 推入条目。

---

## 模式：`journal_flush_pins` — cleanup-first 错误传播

**问题**：`journal_flush_pins()` 的 callback 现返回 `Result<(), StorageError>`。若 callback 返回 `Err`，直接使用 `?` 会让函数立即退出，跳过 cleanup（move pin to flushed、clear flush_in_progress、notify）。

**后果**：
- `flush_in_progress` 永远留在 pin 地址 → `bch2_journal_pin_flush()` **永久死循环**
- pin 不从未移 flushed 链表 → 下次 `journal_get_next_pin` 重入同一失败 pin
- `flush_in_progress_dropped` 不重置

**修复方案**：callback 结果先存临时变量 → cleanup 始终执行 → 然后传播错误。

```rust
// ✅ 正确：cleanup-first 错误传播
let cb_result = if let Some(ref flush_fn) = pin.flush {
    flush_fn(self, pin, seq)
} else {
    Ok(())
};
// ALWAYS 执行 cleanup（move pin, clear flush_in_progress, notify）
// ...
// 传播 callback 错误（cleanup 完成后才传播，防止 leak）
cb_result?;

// ❌ 错误：尽早传播，跳过 cleanup
flush_fn(self, pin, seq)?;  // 返回 Err → 跳过所有 cleanup
```

### journal_flush_pins

```rust
pub fn journal_flush_pins(&self, seq_to_flush: u64) -> Result<u32, StorageError>
```

- 返回值：`Ok(u32)` — flush 的 pin 数量
- callback 错误：cleanup 后传播 `Err(StorageError)`
- 对应 bcachefs `journal_flush_pins()` (reclaim.c:774-849)，但 C 中 callback 为 void 无错误

### bch2_journal_flush_pins

```rust
pub fn bch2_journal_flush_pins(&self, seq_to_flush: u64) -> Result<bool, StorageError>
```

- 阻塞重试循环直到 `journal_flush_pins` 返回 0
- `Ok(true)` 表示至少执行了一次 flush 操作
- 对应 bcachefs `bch2_journal_flush_pins()` (reclaim.c:1399-1411)
