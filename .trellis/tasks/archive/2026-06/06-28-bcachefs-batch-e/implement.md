# Batch E — Btree IO 节点读写对齐：执行计划

## 阶段划分

### Phase 1: Read 验证流水线 (P0, 6 个任务点)

```
依赖: 无
验证: cargo test -p volmount-core --lib btree::io
```

1a. **`StorageError` 新增变体** — `CorruptData`, `ChecksumMismatch`
1b. **`bch2_validate_bset()` 实现** — bset header 格式验证：
    - 验证 bset version 兼容性
    - 验证 `BSET_OFFSET` 与扇区偏移一致
    - 验证非首 bset 的 `u64s > 0`
1c. **`bch2_validate_bset_keys()` 新增函数** — 遍历 bset 内所有 key：
    - 验证每个 key 的 `bkey_packed` 格式正确
    - 检查相邻 key 的 bpos 非降序（`bkey_cmp_left_packed`）
1d. **`bch2_btree_node_read_done()` 完整实现** — header magic + version 验证 + 遍历 bsets 调用 `validate_bset` + `validate_bset_keys`
1e. **全局 key 排序** — 读取后 sort_iter 收集所有 bsets 的 keys，全局排序，替换节点数据
1f. **`bch2_btree_node_drop_keys_outside_node()` 实现** — 遍历 bsets，memmove 丢弃范围外 key，重建 aux tree

### Phase 2: Write 预排序模式 (P0, 3 个任务点)

```
依赖: Phase 1
验证: cargo test -p volmount-core --lib
```

2a. **sort_iter 架构引入** — 在 `io.rs` 或新建 `sort.rs` 中实现 sort_iter：
    - `sort_iter_init` / `sort_iter_add` / `sort_iter_sort`
    - `bch2_sort_keys_keep_unwritten_whiteouts` — 排序输出到目标 buffer
2b. **写入路径集成** — `bch2_btree_node_write` / `__bch2_btree_node_write` 在序列化前执行 sort_iter 排序合并 → 然后一次写入
2c. **写入 checksum** — sort_iter 输出的 bset 计算 CRC32C 后写入

### Phase 3: IO 标志位协议 + write_done 回调 (P1, 3 个任务点)

```
依赖: Phase 2
验证: cargo test -p volmount-core --lib
```

3a. **`write_in_flight`/`read_in_flight` 标志位** — `io_lock`/`io_unlock` 使用 CAS flag 而非 no-op，`wait_on_read`/`wait_on_write` 使用 spin 等待
3b. **写入完成状态传播** — 写入成功后清除 `write_in_flight` + `just_written`，journal pin drop 确认
3c. **`post_write_cleanup` 微调** — 对齐 bcachefs：检查 `just_written` → 清除 → nsets>1 时 sort 合并 → drop_whiteouts → init_next → build_aux

### Phase 4: 测试 + 清理 (P2, 3 个任务点)

```
依赖: Phase 3
验证: cargo test -p volmount-core --lib, cargo clippy --all-targets
```

4a. **正面测试** — 写入 → 读取 roundtrip 验证各路径完整性
4b. **负面测试** — corrupt magic, bad checksum, out-of-order keys, out-of-range keys
4c. **`bch2_btree_node_header_to_text()` 实现** — 格式化输出 header debug 信息

## 关键数据结构

### sort_iter（volmount Rust 版）

```rust
/// bcachefs sort_iter 的 Rust 移植
/// 用于收集多个 bset 中的 key 范围，排序输出到单一 bset
struct SortIter {
    entries: Vec<SortIterEntry>,
    size: usize,  // 总 key 数
}

struct SortIterEntry {
    start: *const u8,  // bkey_packed 起始地址
    end: *const u8,    // bkey_packed 结束地址
    bset: u32,         // 源 bset 索引
}
```

## 验证命令

```bash
# 每阶段后
cargo test -p volmount-core --lib btree::io
cargo test -p volmount-core --lib

# 最终验证
cargo test -p volmount-core --lib
cargo test -p volmount-nbd --lib
cargo clippy --all-targets
```

## 验收检查列表

- [ ] Phase 1: `read_done` 完整验证流水线通过
- [ ] Phase 2: 写入前 sort_iter 排序合并通过
- [ ] Phase 3: IO 标志位 + write_done 通过
- [ ] Phase 4: 负面测试覆盖 + clippy clean
- [ ] 所有 btree 模块测试通过（预存失败测试列表未增加）
