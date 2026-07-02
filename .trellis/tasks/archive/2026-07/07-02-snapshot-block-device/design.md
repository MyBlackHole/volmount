# Snapshot Block Device

## Summary

把现有“volume”外部语义升级成“block device”语义，并把快照模型升级为 COW + cloneable 的 block 资源：

- 外部 API 改为 `/api/v1/blocks/...`
- 磁盘布局、配置项、目录名改为 `blocks`
- 后端仅保留 sparse 文件路径作为第一版落地后端
- 快照是 COW；clone 也走 COW，共享同一物理块池
- 元数据继续由 bcachefs 风格 btree 管理，不再引入独立 checkpoint / 外部元文件

## Architecture

### Public surface

- daemon 对外暴露 block device 资源，不再对外说 volume
- block 资源拥有自己的创建、挂载、卸载、删除、快照、回滚、克隆入口
- 旧 `volumes` 语义不保留兼容别名，直接切换到 `blocks`

### On-disk layout

- 目录命名从 `home/volumes/<name>` 切到 `home/blocks/<name>`
- sparse 文件后端文件名仍由配置控制，但归属目录改为 blocks 命名空间
- superblock 继续作为唯一持久化权威，保存 block 元数据与 btree roots

### Metadata model

- 每个 block device 维护自己的 btree root/snapshot/root-pointer 记录
- block clone 不是整块复制，而是创建新的 block 元数据记录，初始 root 指向源快照对应的 COW 基底
- 物理块由共享块池分配；多个 block device 共享同一套后端块地址空间，通过 btree 元数据分裂实现 COW

### Data flow

1. 创建 block device
   - 生成新的 block 元数据
   - 初始化 sparse 后端文件
   - 写 superblock / root pointers / btree roots
2. 创建快照
   - 固化当前 block 的 root/snapshot 记录
   - 新写入不改旧快照，继续走 COW
3. 从快照克隆 block device
   - 新 block 记录引用快照作为初始基底
   - 后续写入在新 block 上分裂，不影响源 block
4. mount / umount
   - 保持现有 NBD 暴露路径
   - 只是把资源名、路由和目录从 volume 改成 block

## API / Interface Changes

- HTTP routes:
  - `/api/v1/blocks`
  - `/api/v1/blocks/:name`
  - `/api/v1/blocks/:name/mount`
  - `/api/v1/blocks/:name/umount`
  - `/api/v1/blocks/:name/snapshots`
  - `/api/v1/blocks/:name/snapshots/:id`
  - `/api/v1/blocks/:name/snapshots/:id/rollback`
  - 新增克隆入口：从快照创建新 block device
- 配置/路径：
  - `volumes_dir` / `volume_backend_path` 改为 blocks 命名
  - daemon 状态输出中的资源名改为 blocks

## Compatibility / Migration

- 允许 breaking change
- 不保留 `/api/v1/volumes/...` 兼容别名
- 不做旧磁盘 layout 迁移
- 不保留旧 checkpoint 或旧元文件兼容路径

## Tests

- block 创建与删除
- 多 block 实例隔离
- sparse 后端 reopen 持久化
- snapshot create/list/delete/rollback
- clone from snapshot 后：
  - 新 block 初始可见内容与源一致
  - 后续写入不影响源 block
  - 删除/回滚/挂载流程仍正常
- API 路由只保留 blocks 命名

## Assumptions

- 第一版只支持 sparse 后端；S3/NFS 不纳入本任务
- 内部 core 结构可以先保留现有实现骨架，外部命名先统一到 blocks
- clone 使用 COW 共享物理块池，不 materialize 完整副本
