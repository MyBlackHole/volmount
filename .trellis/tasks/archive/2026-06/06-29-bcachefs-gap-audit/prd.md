# Batch H: `_seq` 过渡 API 迁移

## Goal

将 volmount 中仍在使用 `_seq` 过渡 API 的模块迁移到 `JournalEntryPin` 新 API，然后删除全部过渡期 `_seq` 函数。

迁移完成后，所有 journal pin 操作统一通过 `bch2_journal_pin_add` / `bch2_journal_pin_drop` 管理，不再使用 seq 号做粗糙的 pin 计数。

## 范围

### 迁移目标（5 个文件，25 处）

| 文件 | 替换数 | 当前 API | 迁移方案 |
|------|--------|----------|----------|
| `btree/cache.rs` | 8 | `bch2_journal_pin_drop_seq` | 调用者嵌入 `JournalEntryPin`，用 `bch2_journal_pin_drop` 替换 |
| `btree/io.rs` | 3 | `bch2_journal_pin_add_seq` | IO 请求生命周期内用 `JournalEntryPin` 代替 seq pin |
| `volume/mod.rs` | 3 | `bch2_journal_pin_set_seq`, `__bch2_journal_pin_put` | Volume 结构嵌入 `JournalEntryPin` |
| `journal/types.rs` | 1 | `__bch2_journal_pin_put`（内部使用） | 使用新 pin drop 路径 |
| `journal/reclaim.rs` | 1 | `__bch2_journal_pin_put`（内部使用） | 使用新 pin drop 路径 |

### 清理目标

删除以下过渡 API 函数：
- `bch2_journal_pin_set_seq`
- `bch2_journal_pin_add_seq`
- `bch2_journal_pin_drop_seq`
- `__bch2_journal_pin_put`
- `bch2_journal_update_last_seq`（_seq 别名）
- 相关 `// Step 4: Transition` 注释块

## 非范围

- write_buffer worker 线程实现（需异步架构设计）
- gc 存根补完（需事务 + trigger）
- trans_get 实现
- cache/mod.rs page cache 实现
- other gaps from audit

## Acceptance Criteria

- [ ] `btree/cache.rs` 不再调用 `bch2_journal_pin_drop_seq`
- [ ] `btree/io.rs` 不再调用 `bch2_journal_pin_add_seq`
- [ ] `volume/mod.rs` 不再调用 `bch2_journal_pin_set_seq` / `__bch2_journal_pin_put`
- [ ] 过渡 API 函数全部删除
- [ ] 全量 `cargo test -p volmount-core --lib` 通过（已知 expected fails 不变）
- [ ] clippy 无新增 warning
- [ ] 无 `#[allow(dead_code)]` 因本变更而新增
