# Snapshot Block Device

## Goal

实现支持快照功能的块设备：可以创建多个彼此独立的块设备实例；每个实例的后端存储由虚拟块设备提供；虚拟块设备以稀疏文件抽象实现；元数据管理沿用 bcachefs 的 btree 方案。

## Confirmed Facts

- 代码库里已经有稀疏文件后端：`crates/volmount-core/src/block_device/file.rs`。
- 代码库里已经有块设备抽象：`crates/volmount-core/src/block_device/mod.rs`。
- 代码库里已经有卷级快照/子卷能力：`crates/volmount-core/src/snap/` 与 `crates/volmount-core/src/volume/mod.rs`。
- daemon 侧已经支持按卷创建、列出、挂载、卸载、删除，并通过 backend 类型区分后端实现：`crates/volmountd/src/volume.rs`。
- 现有实现已经把卷元数据放进 superblock，不再依赖 `meta.volmount` 之类的外部元文件。

## Requirements

- 支持创建多个块设备实例，实例之间互相独立。
- 每个块设备实例都有自己的后端存储空间。
- 后端存储实现基于稀疏文件。
- 元数据管理使用 bcachefs 风格的 btree 体系，而不是再引入一套平行的元数据存储。
- 快照语义采用 COW：创建后保留历史点，后续写入不破坏已有快照。
- 对外 API 改为 block device 语义，而不是继续沿用 volume 语义。
- 支持从快照克隆新的独立 block device。
- 磁盘布局、目录名、配置项也一起改成 blocks 语义。
- 快照相关行为需要能通过明确的 API / 任务流验证。

## Acceptance Criteria

- 可以创建至少两个独立块设备实例，彼此写入不会互相影响。
- 块设备后端可以在稀疏文件上持久化数据，并在重启/重新打开后恢复。
- 快照相关操作有明确的创建/读取/回滚/删除行为定义。
- 元数据与快照状态由 btree 路径持久化，而不是单独的外部元文件。
- 相关测试覆盖创建、多实例隔离、后端持久化和快照语义。

## Open Question

None.

## Notes

- 这个任务是新一轮规划，当前还没有实现方案和拆解。
