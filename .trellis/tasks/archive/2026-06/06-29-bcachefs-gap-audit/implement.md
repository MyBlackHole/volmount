# Batch H 执行计划: `_seq` 过渡 API 迁移

## 前置条件

- [ ] prd.md 已定稿
- [ ] design.md 已定稿
- [ ] 用户已确认范围

## 执行顺序

### Step 1: btree/io.rs (3 处 _add_seq → pin_add)

**任务**:
1. 在 IO 栈帧中创建 `JournalEntryPin` 局部变量
2. 用 `bch2_journal_pin_add` 替换 `bch2_journal_pin_add_seq`
3. 栈帧结束自动 drop pin

**验证**:
- `cargo test -p volmount-core --lib btree::io` (29 tests)
- 确认 `_add_seq` 不再被 io.rs 调用

### Step 2: volume/mod.rs (3 处 _set_seq / __bch2_journal_pin_put → pin)

**任务**:
1. Volume 结构新增 `journal_pin: JournalEntryPin`
2. `bch2_journal_pin_set_seq(journal_seq)` → `j.bch2_journal_pin_add(journal_seq, &mut self.journal_pin, noop_flush)`
3. `j.__bch2_journal_pin_put(node.journal_seq)` → `j.bch2_journal_pin_drop(&mut self.journal_pin)`

**风险**: 需要在 `&mut self` 方法和 `&Journal` 之间协调借用。

**验证**:
- `cargo test -p volmount-core --lib` 全量通过

### Step 3: btree/cache.rs (8 处 _drop_seq → pin_drop)

**需要：`BtreeNode` 嵌入 `JournalEntryPin`**

**子步骤**:
1. `BtreeNode` 新增 `journal_pin: Mutex<Option<JournalEntryPin>>` 或类似方案
2. 节点 evict 路径：从 node 获取 pin，`j.bch2_journal_pin_drop(pin)`
3. 节点 drop 路径：从 node 获取 pin 并 drop
4. 确定设置 pin 的时机（节点修改时）

**验证**:
- `cargo test -p volmount-core --lib btree::cache` 全部通过
- `cargo clippy -p volmount-core --all-targets` 无新增 warning

### Step 4: journal 内部清理

**任务**:
- 将 `types.rs` 中 `__bch2_journal_pin_put` 的调用改为直接内部逻辑
- 评估 `bch2_journal_update_last_seq` 的保留/删除

**验证**:
- `cargo test -p volmount-core --lib journal` 全部通过

### Step 5: 删除过渡 API + 更新 quality-guidelines

**任务**:
1. 删除以下公共函数：
   - `bch2_journal_pin_set_seq` (types.rs:1246)
   - `bch2_journal_pin_add_seq` (types.rs:1254)
   - `bch2_journal_pin_drop_seq` (types.rs:1262)
   - `pub fn __bch2_journal_pin_put` (reclaim.rs:1004) — 改为内部非 pub
   - `bch2_journal_update_last_seq` (types.rs:1268)
2. 删除相关 `// Step 4: Transition` 注释块
3. 更新 quality-guidelines.md 记录 Batch H

**验证**:
- `cargo search` 确认所有 `_seq` API 引用已删除
- `cargo test -p volmount-core --lib` 全量通过
- `cargo clippy -p volmount-core --all-targets` 无新增 warning

## 验证命令

```bash
# 每步后运行
cargo test -p volmount-core --lib 2>&1 | tail -5

# 最终
cargo clippy -p volmount-core --all-targets 2>&1 | grep -E "error|warning.*new"
```

## 回滚点

- 每步一个独立 commit，前一步失败不影响后续
- 如果 Step 3（cache.rs Mutex）引入性能问题，退回到 `_drop_seq` 保留但将其他模块迁移的方案
