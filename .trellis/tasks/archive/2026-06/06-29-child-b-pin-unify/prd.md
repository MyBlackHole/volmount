# Child-B: 统一 pin 管理 (TC3)

## Goal

消除 Volume 级显式 pin 管理，移除 `Volume.journal_pin` 字段及相关所有调用，仅保留节点级 embedded pin（`BtreeNode.journal_pin`）完成全部 journal pin 语义。

## 背景

当前 Volume 级 pin（`write_extent`/`delete_extent` 中 `bch2_journal_pin_add`，`flush_dirty_nodes` 末尾 `bch2_journal_pin_drop`）与节点级 pin（`bch2_btree_node_write*` 时注册，cache eviction 时释放）两条路径并存：

| 机制 | 注册时机 | 释放时机 |
|------|----------|----------|
| **Volume 级 pin** | `write_extent()` / `delete_extent()` 末尾 | `flush_dirty_nodes()` 末尾 |
| **节点级 pin** | `bch2_btree_node_write*()` (`io.rs:762-861`) | cache eviction (`cache.rs:203-807`) 等各种释放点 |

Volume 级 pin 是 volmount 特有的（bcachefs 无对应），粒度为整个 flush 批次而非具体节点。两者并存造成 **pin 计数加倍**，可能导致 `last_seq` 推进延迟。

## 安全性论证

节点级 pin 已覆盖全部语义：

1. **每个脏节点** 在首次写回后端时，通过 `bch2_btree_node_write*()` 注册 pin（`io.rs:784,825,861`）
2. **每个 pin 在节点被 cache evict 时释放**（`cache.rs` 中 8 处 pin drop）
3. Volume 级 pin 是冗余：它 pin 住整个 journal entry（可能包含多个 btree 修改），而非具体节点。当该 entry 中所有节点都已刷回时，节点级 pin 已全部释放
4. `last_seq` 推进不受影响：`bch2_journal_maybe_update_last_seq()` 检查 pin FIFO 前端 `count==0` 推进。消除冗余 pin 后推进更快

Child-A (TC1+TC2) 已将 `trans_commit()` 内的 btree 修改与 journal 绑定，Volume 不再需要持有自己的 pin 来确保 journal entry 存活。

## Requirements

1. 移除 `Volume` 结构体中的 `journal_pin: JournalEntryPin` 字段
2. 移除构造器 `Volume::new()` 中的 `journal_pin: JournalEntryPin::new(None)` 初始化
3. 移除 `write_extent()` 末尾的 `journal.bch2_journal_pin_add(journal_seq, &self.journal_pin, None)`
4. 移除 `delete_extent()` 末尾的 `journal.bch2_journal_pin_add(journal_seq, &self.journal_pin, None)`
5. 移除 `flush_dirty_nodes()` 末尾的 `journal.bch2_journal_pin_drop(&self.journal_pin)`
6. 移除 `use crate::journal::reclaim::JournalEntryPin` import（如无其他引用）
7. `flush_dirty_nodes()` 中保留节点写回逻辑，移除 pin 管理注释
8. 节点级 pin（`io.rs` 中 3 处 add + `cache.rs` 中 8 处 drop）保持不变

## Acceptance Criteria

### 功能验收

- [ ] `Volume` 结构体中不再有 `journal_pin` 字段
- [ ] `write_extent()` 中无 `bch2_journal_pin_add` 调用
- [ ] `delete_extent()` 中无 `bch2_journal_pin_add` 调用
- [ ] `flush_dirty_nodes()` 中无 `bch2_journal_pin_drop` 调用（只用 `journal` 参数 flush 节点本身）
- [ ] `Volume::new()` 构造器中无 `JournalEntryPin::new()` 初始化
- [ ] `journal::reclaim::JournalEntryPin` import 从 `volume/mod.rs` 中移除（如果无其他引用）
- [ ] 节点级 pin 注册/释放不受影响（`io.rs`、`cache.rs`、`key_cache.rs`、`node.rs` 不修改）

### 质量验收

- [ ] `cargo build` 0 errors
- [ ] `cargo test -p volmount-core --lib` 763 passed, 5 pre-existing（0 新增失败）
- [ ] `cargo clippy --all-targets` 无新增 warning
- [ ] `cargo fmt --check` clean

### 集成验证

- [ ] `write_extent` → `trans_commit`（Phase 2 btree 修改 + 节点级 pin）→ 数据正确
- [ ] `delete_extent` → `trans_commit`（Phase 2 btree 修改 + 节点级 pin）→ 数据正确
- [ ] `flush_dirty_nodes` → 节点写回后节点级 pin 正常释放 → `last_seq` 正常推进

## 排除范围

- 不修改节点级 pin 逻辑（`io.rs`、`cache.rs`、`node.rs`、`key_cache.rs`）
- 不修改 `Flush` 结构体或 journal reclaim 路径
- 不涉及 `flush_dirty_nodes()` 中节点写回的核心逻辑（保留 `bch2_btree_post_write_cleanup`、`clear_will_make_reachable` 等）

## 波及文件

| 文件 | 改动 |
|------|------|
| `volume/mod.rs` | 移除 `journal_pin` 字段 + 构造器初始化 + 3 处 pin 调用 + 对应 import |

仅此一个文件。其他文件（`io.rs`、`cache.rs`、`node.rs`、`key_cache.rs`、`reclaim.rs`、`types.rs`）不变。
