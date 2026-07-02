# Child-A: trans_commit 集成 (TC1+TC2)

## 目标

将 `trans_commit()` 从 journal-WAL 先于 btree 的旧顺序重构为 bcachefs 完全对齐的 reserve→modify→fill→release 顺序。

## 需求

1. `trans_commit()` 改为 4 阶段：
   - Phase 1: `calc_journal_u64s()` → `journal_res_get(&res)` → `self.journal_seq = res.seq`
   - Phase 2: `commit_with_engine(engine)` 在 committed 标记后新增 btree 节点修改
   - Phase 3: `add_entry(&res, serialized)` 填充 journal 到已保留空间
   - Phase 4: `journal_res_put(&res)` 释放保留（refcount→0 自动触发写）
2. 新增 `calc_journal_u64s()` 辅助方法（粗略预计算：16 u64s/entry，至少 64 u64s）
3. `commit_with_journal()` 标记 `#[deprecated]`，序列化逻辑（分组 JsetEntry + bincode serialize）内联到 Phase 3
4. Volume 的 `write_extent`/`delete_extent` 移除手动 `drain_journal()` + `engine.insert_entry()` 循环

## 验收

- [ ] `trans_commit()` 按 reserve→modify→fill→release 顺序执行
- [ ] `BtreeTrans::journal_seq` 在 Phase 1 的 `journal_res_get()` 后正确设置
- [ ] `commit_with_engine()` 在 committed 后使用 `self.journal_seq` 调用 `insert_entry_raw`/`delete_entry_raw` 修改 btree 节点
- [ ] Phase 3 `add_entry()` 将序列化后 journal 条目写入已保留空间
- [ ] Phase 4 `journal_res_put()` 正确释放保留
- [ ] Volume 的 `write_extent`/`delete_extent` 移除手动 drain 循环
- [ ] `cargo build -p volmount-core` 0 errors
- [ ] `cargo test -p volmount-core --lib` 763 pass, 5 pre-existing, 0 new failure

## 参考文档

父任务 design：`.trellis/tasks/06-29-bcachefs-transaction-chain/design.md` §"TC1+TC2: trans_commit 到 bcachefs 顺序"
父任务 implement：`.trellis/tasks/06-29-bcachefs-transaction-chain/implement.md` §"Child-A: trans_commit 集成 (TC1+TC2)"
