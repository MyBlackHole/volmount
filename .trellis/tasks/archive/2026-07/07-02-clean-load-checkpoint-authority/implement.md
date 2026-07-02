# clean-load 去 checkpoint 权威

## 当前状态

本任务的主线实现已完成：

- clean-load / recovery 已切到 `superblock + Journal + recovery`
- `checkpoint_volume()` 的权威作用已移除
- `meta.volmount` 已不再作为卷元数据来源
- 块设备后端命名已收口为 `sparse`，实现层通过稀疏文件模拟块设备

## 已完成项

- [x] `volmountd::init_volume()` 不再以 checkpoint 作为恢复主路径
- [x] clean / unclean mount 统一通过 `superblock + Journal + recovery`
- [x] `checkpoint_volume()` 已从 daemon 主流程中移除
- [x] `meta.volmount` 已删除为权威元数据来源
- [x] `set_may_go_rw` 的恢复语义已补齐
- [x] 相关测试已覆盖 clean-load、unclean recovery、RW 过渡恢复

## 已拆分到后续任务

- [x] 清理 `alloc/background.rs` 中与 bucket_gens 相关的 `checkpoint` 术语歧义
- [x] 清理 `journal/jset.rs` 中与 journal seq / blacklist 相关的 `checkpoint` 术语歧义
- [ ] 继续扫 recovery / btree/io / volume 的 bcachefs 对齐差异项

## 验证命令

```bash
cargo test -p volmountd volume -- --nocapture
cargo test -p volmount-core recovery -- --nocapture
cargo check --workspace
```

