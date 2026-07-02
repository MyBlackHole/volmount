# Child-D: journal 集成扩展 (TC6)

## 目标

将 `Volume::btree_insert()` 改为可选走 journal WAL，确保需要 crash-safe 的写入路径有 WAL 保护。

## 需求

1. 新增 `async btree_insert_with_journal()` 方法：
   - 使用 `BtreeTrans::default()` + `journal_insert()` + `trans_commit()` 走完整 journal 流程
   - 返回 `Result<u64, StorageError>`（journal_seq）
2. 保留旧 `btree_insert()` 作为轻量同步版本（不走 journal，直接 `insert_guarded()`）
3. 调用方适配 await（如有直接调用 `btree_insert` 且需要 journal 的场景）

## 验收

- [ ] `btree_insert_with_journal()` 通过 BtreeTrans + trans_commit 走 journal WAL
- [ ] 旧 `btree_insert()` 保持向后兼容（签名不变）
- [ ] `cargo build` 全部 crate 0 errors
- [ ] `cargo test -p volmount-core --lib` 无新失败

## 参考文档

父任务 design：`.trellis/tasks/06-29-bcachefs-transaction-chain/design.md` §"TC6: Journal 集成扩展"
父任务 implement：`.trellis/tasks/06-29-bcachefs-transaction-chain/implement.md` §"Child-D: journal 集成扩展 (TC6)"
