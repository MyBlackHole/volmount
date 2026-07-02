# P0 功能逻辑修复 — recovery + cache + CRC

## Goal

修复 4 个 P0 级功能逻辑差异（源于 06-26-remaining-subsystems-review 审查结果），使 volmount-core 的核心数据安全路径与 bcachefs C 参考实现一致。

## Requirements

### R1 — Recovery 模块集成到 Volume 启动路径

`bch2_fs_recovery()` 已在 `recovery/mod.rs` 定义，但 `Volume::new()`（`volume/mod.rs`）从未调用它。recovery passes 从不执行 → 崩溃后卷无法恢复。

- 在 `Volume::new()` 的合适位置插入 `bch2_fs_recovery()` 调用
- 确认 recovery pass 顺序正确（journal replay → alloc_read → gc → ...）
- 处理 recovery 失败的错误传播

### C1 — cache dirty.clear() 数据丢失

`btree/cache.rs` 中 `mark_dirty` 在脏节点超过 `MAX_DIRTY` 时调用 `inner.dirty.clear()` 直接丢弃所有脏节点引用。

- 改为 `flush_all_dirty()` 真正刷出脏数据再清空集合
- 确保 flush 过程中新加入的脏节点不会被错误丢弃
- 验证无其他 `dirty.clear()` 或 `drain()` 调用路径

### J1 — CRC32 覆盖完整 Jset

`journal/jset.rs` 中 CRC32 仅覆盖 entries，不覆盖 magic/seq/last_seq/entry_count 等头部字段。

- 扩展 CRC32 计算范围：`crc32c(magic || seq || last_seq || entry_count || entries)`
- 保持向后兼容：写入时使用新格式，读取时尝试新格式回退旧格式
- 更新相关序列化和验证函数

### R2 — btree root level 信息丢失

`recovery/btree_roots.rs` 的 `load_from_superblock()` 只从 superblock 提取 `(BtreeId, u64)` 地址，丢失 `level` 字段。

- 在 `BtreeRoots` 中增加 `levels: HashMap<BtreeId, u32>` 字段（或并入现有结构）
- 从 superblock 读取 level 信息
- btree 加载器使用正确的 level 重建非 level-0 root

## Acceptance Criteria

- [ ] R1: Volume 启动时执行 recovery passes，625 测试不退化，新文件系统初始化正常
- [ ] C1: mark_dirty 的 auto-flush 不再丢弃脏数据，通过 dirty/page 管理测试
- [ ] J1: CRC32 覆盖完整 Jset 头部，写入→关闭→读取循环校验通过
- [ ] R2: btree root level 从 superblock 正确加载
- [ ] `cargo test -p volmount-core --lib` 全部通过
- [ ] `cargo clippy --all-targets` 无新增警告
- [ ] 未改动的模块（lock/snap/subvol/alloc）行为不变

## Constraints

- 不改动任何 API 签名
- 不新增外部依赖
- 修改范围限于 volmount-core（不改 volmountd/volmount/volmount-nbd）
- 每个修复独立可验证
