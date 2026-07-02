# clean-load 去 checkpoint 权威

## 1. 目标

把卷的持久化/恢复权威统一到：

1. `superblock root pointers`
2. `Journal` state 与 `BtreeRoot` journal entry
3. 实际节点 blocks

不再让 bincode 全树 checkpoint 成为 daemon 的持久化或恢复路径。

## 2. 现状与问题

当前实现里，`init_volume()` 已经切到 `superblock + Journal + recovery`，但 daemon 侧仍保留：

- `checkpoint_volume()`
- `BchSb.btree_cp_addr / btree_cp_len`
- `BtreeEngine::serialize_checkpoint_to_bytes()`
- `BtreeEngine::deserialize_checkpoint_from_bytes()`

这会导致两个问题：

1. 代码里同时存在两套“恢复来源”，语义边界不清晰。
2. 关闭流程仍可以把 checkpoint 当成权威数据源，和 bcachefs 的 clean stop / journal roots 模型不一致。

另外，`meta.volmount` 也是冗余的：卷元数据已经保存在 backend superblock 中，稀疏文件只是块设备后端的一种实现方式；对外语义仍是虚拟块设备。文件名由 volumed 启动配置指定；文件偏移与虚拟块号直接对应（`offset = block_no * block_size`）。S3 后端则通过 bucket/key 前缀表示设备。卷名由目录名或 API 请求名提供，因此不需要额外的外部元文件。

## 3. 对齐原则

bcachefs 的 clean shutdown 不是“写一份全树 checkpoint 再复原”，而是：

- 写回/提交 btree roots
- 写 superblock clean 相关状态
- 让 journal / roots 成为重新挂载时的恢复入口

因此本任务只保留与该模型一致的路径：

- `init_volume()`：统一走 recovery
- `checkpoint_volume()`：删除或替换为 stop/drain 语义
- superblock checkpoint 字段：删除其权威性

## 4. 技术方案

### 4.1 删除 daemon checkpoint 关口

`crates/volmountd/src/volume.rs` 中：

- 删除 `checkpoint_volume()`
- 将 `delete_volume()` 与 `main.rs` 的 stop 逻辑改为新的 stop/drain 入口
- stop/drain 入口只做：
  - drain writeback worker
  - flush journal
  - 持久化 superblock clean 状态与 root pointers

### 4.2 收口 superblock checkpoint 字段

`BchSb` 中的：

- `btree_cp_addr`
- `btree_cp_len`

不再参与正常流程。若实现上为了兼容保留字段，也必须是明确的“历史残留、不可再写再读”的状态；更推荐直接删除并让编译器驱动所有调用点收敛。

### 4.3 保留 recovery 路径为唯一恢复入口

clean / unclean mount 都必须通过：

1. 读取 superblock
2. 读取 journal state
3. `recovery::bch2_fs_recovery()`
4. `load_root_from_ptr()`

不再使用 `deserialize_checkpoint_from_bytes()` 作为挂载入口。

### 4.4 停止语义

关闭时的语义应接近 bcachefs `bch2_fs_stop()`：

- 停止接收写入
- drain writeback
- flush journal
- 写回 clean superblock

而不是：

- 序列化整树 checkpoint
- 再把 checkpoint 当成重启权威

## 5. 迁移与兼容

本任务允许破坏旧格式。

- 不提供旧 checkpoint 兼容读路径
- 不提供双读 / 双写
- 不需要把旧 checkpoint 数据自动迁移到新格式
- 不需要把旧 `meta.volmount` 自动迁移到新格式

如果某些 superblock 字段必须短期保留，必须在代码和测试里标明它们不再参与正常流程。

## 6. 风险点

- 删除 `btree_cp_*` 字段后，可能会引出一批编译失败，这些失败应当被当作迁移清单，不应补回旧逻辑。
- `delete_volume()` 和 CLI stop 路径都必须改到新的 stop/drain 入口，否则 checkpoint 代码会残留调用点。
- 恢复测试要覆盖 clean / unclean 两条路径，避免 clean mount 又回退到 checkpoint 侧门。

## 7. 验收标准

- `init_volume()` 不再依赖 checkpoint 作为权威恢复源。
- daemon 关闭路径不再写入或读取全树 checkpoint。
- superblock checkpoint 字段不再进入正常读写路径。
- `meta.volmount` 不再作为卷元数据来源。
- 相关测试证明 clean / unclean mount 仍可恢复。
