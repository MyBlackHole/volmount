# Batch H 设计文档: `_seq` 过渡 API 迁移

## 架构分析

### 当前 `_seq` API 的三个语义

| API | 语义 | 等效新 API | 迁移难度 |
|-----|------|-----------|----------|
| `_set_seq(seq)` | 将 journal pin count 设为指定 seq 的值 (set, not add) | `bch2_journal_pin_add(seq, &pin, cb)` | 中 — 需嵌入 pin |
| `_add_seq(seq, cb)` | 在 seq 位置 +1 | `bch2_journal_pin_add(seq, &pin, cb)` | 低 — 嵌入 pin 并 add |
| `_drop_seq(seq)` / `__bch2_journal_pin_put(seq)` | 在 seq 位置 -1 | `bch2_journal_pin_drop(&pin)` | 中 — 需存储 pin 引用 |

### 关键设计决策: 嵌入 vs 过渡保留

**Volume**: 每个 Volume 已有一个明确的生命周期，嵌入一个 `JournalEntryPin` 即可。
**IO 操作**: 每次 IO 创建临时 `JournalEntryPin`，IO 完成时 drop。
**BtreeNode 缓存**: 每个 `BtreeNode` 嵌入一个 `Option<JournalEntryPin>`。

## 各模块迁移方案

### 1. `volume/mod.rs` (3 处)

**当前模式**:
```
commit → _set_seq(new_seq)    // 设为最新 journal seq
flush  → __bch2_journal_pin_put(old_seq)  // 释放旧 seq 的 pin
```

**迁移方案**:
- `Volume` 结构体新增 `journal_pin: JournalEntryPin`
- flush callback: no-op (volmount 不依赖 callback)
- `bch2_journal_pin_set_seq(seq)` → `j.bch2_journal_pin_add(seq, &mut self.journal_pin, noop_flush)`
- `j.__bch2_journal_pin_put(seq)` → `j.bch2_journal_pin_drop(&mut self.journal_pin)`

**约束**: `Volume.journal_pin` 需要 `&mut` 访问 — 当前 Volume 方法已是 `&mut self`。

### 2. `btree/io.rs` (3 处)

**当前模式**:
```
read IO 开始 → _add_seq(jseq, empty_callback)  // 防止 journal reclaim 正在读取的 seq
read IO 完成 → pin 自动过期（通过 journal seq 推进）
```

**迁移方案**:
- 在 IO 栈帧中创建局部 `let io_pin = JournalEntryPin::new()`
- `j.bch2_journal_pin_add(jseq, &io_pin, noop_flush)`
- io_pin 在栈帧结束时 drop → 自动 drop pin

**风险**: 当前 `_add_seq` 的 callback 就是空的 (`Box::new(move || {})`)，所以 no-op flush 是正确迁移。

### 3. `btree/cache.rs` (8 处 — 最多)

**当前模式**:
```
节点分配时 → jseq 记录在 BtreeNode.journal_seq
节点 evict 时 → j.bch2_journal_pin_drop_seq(evicted_jseq)
node_drop → j.bch2_journal_pin_drop_seq(jseq)
```

**迁移方案**:
- `BtreeNode` 新增字段 `journal_pin: Option<JournalEntryPin>`
- 节点首次修改时: `journal_pin = Some(JournalEntryPin::new())` + `j.bch2_journal_pin_add(seq, pin, flush_fn)`
- 节点 evict/drop 时: `j.bch2_journal_pin_drop(pin)` 并 `take()` 清除

**关键**: `BtreeNode` 被 `Arc` 共享，`journal_pin` 需通过 `Arc::get_mut` 或 `Mutex` 保护。

#### 3.1 锁方案决策

当前 `BtreeNode` 的 `journal_seq: AtomicU64` 可无锁操作。替换为 `JournalEntryPin` 后：
- `JournalEntryPin` 包含 `Link`（非 Send/Sync）— 不能用 `Atomic` 保护
- 选项 A: `Mutex<Option<JournalEntryPin>>` — 简单但引入锁
- 选项 B: 只在 `&mut BtreeNode` 可获取的操作中设置/drop pin — 约束较多

**决策**: 使用 `Mutex<Option<JournalEntryPin>>`。在 cache eviction 路径中 `cache.rs` 有 `drop(inner)` 释放锁的已有模式可参考。

### 4. `journal/types.rs` & `reclaim.rs` (2 处内部使用)

**当前模式**:
```
内部调用 __bch2_journal_pin_put(seq) 处理 pin 到期
bch2_journal_update_last_seq() 推进 last_seq
```

**迁移方案**:
- `journal/types.rs` 内部使用从 `__bch2_journal_pin_put(seq)` 改为直接操作 pin list
- 或保留 `__bch2_journal_pin_put` 的内部逻辑但删除公共 fn 签名

### 5. 删除过渡 API

删除以下函数和标注：
- `bch2_journal_pin_set_seq` (types.rs:1246)
- `bch2_journal_pin_add_seq` (types.rs:1254)
- `bch2_journal_pin_drop_seq` (types.rs:1262)
- `__bch2_journal_pin_put` (reclaim.rs:1004) — 公共签名，内部逻辑保留在 reclaim.rs 中
- `bch2_journal_update_last_seq` (types.rs:1268) — 测试和内部使用需评估

## 执行顺序

```
1. btree/io.rs     ← 最简单，栈上 pin，风险最低
2. volume/mod.rs   ← 中等，单个 pin 嵌入 Volume
3. btree/cache.rs  ← 最复杂，Arc 共享 + 8 处替换
4. journal 内部    ← 清理内部逻辑
5. 删除过渡 API    ← 最后，确保全量测试通过
```

## 风险

- `btree/cache.rs` 中 Mutex 引入可能影响缓存性能
- BtreeNode 的 `journal_seq: AtomicU64` 字段可移除，但需确认无其他读取者
- `_set_seq` 的语义是"set"而非"add" — 迁移到 `pin_add` + `pin_drop` 需确保 pin 计数正确
