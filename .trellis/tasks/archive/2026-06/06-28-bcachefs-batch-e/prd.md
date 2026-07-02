# Batch E — Btree IO 节点读写对齐

## 需求概述

将 `crates/volmount-core/src/btree/io.rs` 从「带 bcachefs 命名对齐的 thin wrapper」升级为与 bcachefs `btree/read.c` + `btree/write.c` 的完整语义对齐。当前 `io.rs` 中多个关键函数是 no-op/stub，缺乏节点验证、IO 重试、bset 校验和确认等 bcachefs 的关键保证。

## 参考源

- `bcachefs-tools/fs/btree/read.c` (1327 行) — 节点读取、验证、bset 校验、IO 错误处理
- `bcachefs-tools/fs/btree/write.c` (936 行) — 节点写入、bset 排序合并、write_done 回调
- `bcachefs-tools/fs/btree/bset.h` — bset 布局、aux tree 类型、cacheline 常量
- `bcachefs-tools/fs/btree/bset.c` (1873 行) — bset 操作（排序、合并、搜索树构建）
- `bcachefs-tools/fs/bcachefs_format.h` — 磁盘格式定义（`struct btree_node`, `struct bset`）

## 涉及 volmount 文件

**主改动：**
- `crates/volmount-core/src/btree/io.rs` (319 行) — 核心改动目标

**可能涉及的辅助文件：**
- `crates/volmount-core/src/btree/node.rs` (2421 行) — 节点结构体、bset 布局可能需调整
- `crates/volmount-core/src/btree/bucket_io.rs` (98 行) — bucket 级 IO 可能需扩展
- `crates/volmount-core/src/btree/update.rs` (198 行) — write_done 可能需回调 update
- `crates/volmount-core/src/btree/types.rs` (284 行) — 可能新增类型

## 验收标准

### P0（必须完成）

1. **`bch2_btree_node_read_done()` 完整实现** — 不再是最小 stub，需包含：
   - 节点 header 校验（magic、版本、format 兼容性）
   - bset 校验和验证（至少 CRC32C）
   - bset 内 key 排序顺序验证
   - bset 间 key 范围一致性验证
   - 读取完成后 bset 排序合并 + aux tree 重建

2. **`bch2_validate_bset()` 完整实现** — 验证 bset 结构完整性

3. **`bch2_btree_node_drop_keys_outside_node()` 实现** — 丢弃 min_key/max_key 范围外的 key

4. **写入前 bset 排序合并** — `bch2_btree_node_write` 前调用 sort 合并多个 bset

### P1（高优先级）

5. **IO 锁实现** — `bch2_btree_node_io_lock/unlock` 使用实际的同步原语（不再 no-op），防止并发 IO

6. **写入完成回调** — `__btree_node_write_done` 语义：journal pin drop、dirty 状态传播、write_in_flight 清除

7. **IO 错误处理** — 读取/写入失败时的重试或错误传播（对齐 bcachefs `bch2_dev_io_failures` 模型）

### P2（完成度提升）

8. **读取后 bset 自动 merge** — load 时若读取了多个 bset，自动排序合并到 set[0]

9. **测试覆盖** — 新增测试覆盖验证路径（corrupt header、bad checksum、key out of range 等负面测试）

10. **`bch2_btree_node_header_to_text`** — 节点 header 的 debug 输出（用于错误日志）

## 非目标（明确不做的）

- 不改为 async I/O 后台模型（保持同步，但加上接口正确锁定）
- 不改动 volmount 自有的 bset 序列化格式（不追求与 bcachefs 磁盘格式二进制兼容，只追求语义对齐）
- 不实现多设备读重试（当前单 backend 模型）
