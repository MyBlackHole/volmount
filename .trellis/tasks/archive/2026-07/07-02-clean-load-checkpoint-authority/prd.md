# clean-load 去 checkpoint 权威

## Goal

把卷的 clean-load / recovery 权威切到 `superblock root pointers + Journal + node blocks`，不再依赖 bincode 全树 checkpoint 作为恢复主路径。

## Background

- 现有代码已经把 `volmountd::init_volume()` 的 clean-load 拉回到 `Journal + superblock root pointers + recovery` 同源路径。
- 之前 clean-load 仍有 `BtreeEngine::deserialize_checkpoint_from_bytes()` 的 checkpoint 快路径；这与当前 bcachefs 对齐目标不一致。
- 相关恢复语义已经存在：`recovery::bch2_fs_recovery()`、`passes::journal_read`、`passes::btree_roots`。
- `meta.volmount` 作为外部卷元数据文件是冗余的；卷元数据应只保存在 backend superblock 中。

## Confirmed Facts

- `recovery::effective_root_ptr()` 已经把 `superblock.root_ptrs` 作为 root 恢复优先级的第一来源。
- `volmountd::init_volume()` 目前已经不再优先读取 checkpoint，而是通过 `Journal::from_superblock()` / `Journal::create()` 后进入 recovery。
- `checkpoint_volume()` 仍保留了 btree checkpoint 的序列化和写回逻辑。
- `RecoveryState::restore_progress()` 已补了 `set_may_go_rw` 完成时恢复 `engine.enable_overlay()` 与 `may_go_rw = true` 的逻辑，避免 clean mount 卡在只读态。
- 块设备后端使用固定大小的稀疏文件来实现；对外语义仍是虚拟块设备。文件名由 volumed 启动配置指定，卷名和 backend 识别可以从目录布局与 superblock 组合恢复，不需要额外元文件。块号与文件偏移是直接映射关系：`offset = block_no * block_size`。

## Requirements

- clean-load 不应再把 bincode checkpoint 作为权威恢复来源。
- daemon 不能再依赖 `checkpoint_volume()` 进行持久化收尾；应改成 bcachefs 风格的 stop/drain/clean-superblock 路径。
- 恢复路径必须继续通过 `superblock root pointers + Journal` 恢复可达节点。
- 任何已完成的 recovery 进度与 RW 过渡状态必须在重新挂载时能正确恢复。
- superblock 中的 checkpoint 地址/长度字段不再参与权威路径；如果保留兼容字段，也只能是未使用的历史残留。

## Acceptance Criteria

- [ ] clean-load 不再调用 `BtreeEngine::deserialize_checkpoint_from_bytes()` 作为主恢复路径。
- [ ] `volmountd::init_volume()` 在 clean / unclean 两种情况下都只依赖 `superblock + Journal + recovery` 恢复 Btree。
- [ ] `checkpoint_volume()` 已被替换或删除，daemon 关闭流程不再写入/读取全树 checkpoint。
- [ ] superblock 的 checkpoint 地址/长度字段不再参与正常流程。
- [ ] `meta.volmount` 不再作为卷元数据来源；`VolumeMeta` 以 superblock 为唯一权威持久化位置。
- [ ] `set_may_go_rw` 的恢复语义在 clean mount / recovery 后保持正确。
- [ ] 相关测试覆盖 clean-load、unclean recovery、RW 过渡恢复。

## Notes

- 这是一个复杂任务；在范围确认后，需要补 `design.md` 和 `implement.md`。
