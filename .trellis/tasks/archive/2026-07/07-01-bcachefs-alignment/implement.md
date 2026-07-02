# 全面对齐 bcachefs 磁盘布局 — 实施计划

## 阶段总览

```
Phase 1: 基础结构定义     → Phase 2: Btree 序列化    → Phase 3: CRC 基础设施
                                                             ↓
Phase 5: 收尾修复         → Phase 4: Journal 序列化
```

依赖关系：
- P1 无外部依赖 → P2 依赖 P1
- P3 无外部依赖 → P4 依赖 P3
- P5 无外部依赖（独立修复）

---

## Phase 1: 基础结构定义

### 目标
对齐核心数据类型的内存布局：Bpos、BkeyFormat、BtreeNodeHeader，添加 BsetHeader。

### 修改清单

| 文件 | 变更 | 风险 |
|------|------|------|
| `btree/key.rs` | `Bpos`: 添加 `#[repr(C)]`；`BkeyFormat`: 改为 `#[repr(C, packed)]` | 低——字段顺序不变 |
| `btree/key.rs` | `BtreeNodeDiskEntry`（如存在）：添加 `#[repr(C, packed)]` | 低 |
| `btree/node.rs` | 添加 `BsetHeader` 结构体定义（`#[repr(C, packed)]`，16字节） | 低——新结构 |
| `btree/node.rs` | `BtreeNodeHeader`: 改为 `#[repr(C, packed)]`，字段微调 | 中——影响序列化 |
| `btree/node.rs` | 更新 `BtreeNode::new()` 等构造器（如果有影响） | 低 |
| `btree/node.rs` | 更新 `BtreeNode` 中的 `sets: [BsetTree; BSET_COUNT]` 以初始化 BsetHeader 相关字段 | 低 |
| `btree/node.rs` | 更新 `entry_size()` 等辅助函数（如有影响） | 低 |

### 验证
```
cargo build — 编译通过
cargo test — 现有测试不受影响（结构体布局变化不应影响逻辑）
cargo test -p volmount-core — 同上
```

### 回滚点
如果无法编译或大量测试失败，撤销对 key.rs 和 node.rs 的修改，保留 `BsetHeader` 新定义。

---

## Phase 2: Btree 序列化 Pipeline

### 目标
替换 `serialize_to_bucket()` 和 `deserialize_from_slice()` 为固定布局。

### 修改清单

| 文件 | 变更 | 风险 |
|------|------|------|
| `btree/node.rs` | 重写 `serialize_to_bucket()`：直接填充 buf（memcpy/ptr 写），包含 BsetHeader | **高**——核心变更 |
| `btree/node.rs` | 重写 `deserialize()` / `deserialize_from_slice()`：直接 ptr 读取，BsetHeader + entries | **高**——核心变更 |
| `btree/node.rs` | 更新 CRC 计算逻辑（覆盖范围扩展） | 中 |
| `btree/io.rs` | 更新 `validate_bset()`（如有新字段/对齐规则） | 低 |
| `btree/bucket_io.rs` | 更新 `load_btree_node()` / `write_node_to_bucket()`（接口不应变） | 低 |
| `btree/io.rs` | `sort_merge()` 中 bset 合并逻辑（更新 entry 计数或 bset_count） | 中 |

### 关键设计

`serialize_to_bucket()` 新伪代码：
```rust
fn serialize_to_bucket(&self, bucket_addr: u64) -> Result<Vec<u8>, StorageError> {
    let mut buf = vec![0u8; BLOCK_SIZE];
    let buf_ptr = buf.as_mut_ptr() as *mut u8;

    // 1. 写 Header
    let header = BtreeNodeHeader { ... };
    unsafe { ptr::write(buf_ptr as *mut BtreeNodeHeader, header); }

    let mut offset = size_of::<BtreeNodeHeader>();  // 80

    // 2. 写 BsetHeaders
    for set in &self.sets {
        if set.data_offset == set.end_offset {
            continue;  // 空 bset
        }
        let bset_hdr = BsetHeader {
            version: 1,
            level: self.level,
            pad: 0,
            data_bytes: set.end_offset - set.data_offset,
            seq: self.journal_seq,
        };
        unsafe { ptr::write(buf_ptr.add(offset) as *mut BsetHeader, bset_hdr); }
        offset += size_of::<BsetHeader>();  // 16

        // 3. 写 packed entries
        let entry_bytes = &self.data[set.data_offset as usize..set.end_offset as usize];
        buf[offset..offset + entry_bytes.len()].copy_from_slice(entry_bytes);
        offset += entry_bytes.len();
    }

    // 4. CRC（覆盖已写的完整内容，CRC 字段为 0 时计算）
    let crc = crc32c(0, &buf[..offset]);
    // 写回 header.crc32
    // 如果 header 的 crc32 字段偏移已知：
    unsafe {
        let hdr_mut = buf.as_mut_ptr() as *mut BtreeNodeHeader;
        (*hdr_mut).crc32 = crc;
    }

    // 5. 填零到 BLOCK_SIZE（已是零，但确保尾部正确）
    // buf 初始化就是 vec![0u8; BLOCK_SIZE]

    Ok(buf)
}
```

### 验证
```
cargo test btree — 所有 btree 序列化/反序列化 roundtrip 测试通过
cargo test bucket_io — bucket I/O 测试通过  
cargo test — 无回归
```

需要确认现有测试数据是否硬编码了旧格式。如果有，需要更新测试数据。

### 回滚点
如果 roundtrip 测试失败，说明序列化/反序列化不一致。检查：
1. Header 字段偏移在新 layout 中的位置
2. BsetHeader 与 entries 的 offset 计算
3. CRC 计算范围（是否覆盖足够内容）

---

## Phase 3: CRC 基础设施

### 目标
将 CRC 从 `crc32fast`（IEEE CRC32）迁移到 Castagnoli CRC32C，支持 x86_64 硬件加速。

### 修改清单

| 文件 | 变更 | 风险 |
|------|------|------|
| `lib.rs` 或 `types.rs` | 添加 `crc32c_sw()` 纯软件回退（Castagnoli 查表） | 低 |
| `lib.rs` 或 `types.rs` | 添加 `#[cfg(target_arch="x86_64")] crc32c_hw()`（SSE 4.2 内联 asm 或 `core::arch::x86_64`） | 中 |
| `types.rs` | 更新 `Crc32CHasher` 或替换为新 CRC 基础设施 | 中 |
| `btree/jset.rs` | 更新 CRC 常量/引用 | 低 |
| `btree/validate.rs` | 更新 CRC 校验引用 | 低 |

### x86_64 硬件 CRC32C 实现

```rust
#[cfg(target_arch = "x86_64")]
pub fn crc32c_hw(data: &[u8], crc: u32) -> u32 {
    #[cfg(target_feature = "sse4.2")]
    {
        let mut crc = crc as u64;
        for chunk in data.chunks_exact(8) {
            let val = u64::from_le_bytes(chunk.try_into().unwrap());
            unsafe { crc = _mm_crc32_u64(crc, val); }
        }
        for &b in data.chunks_exact(8).remainder() {
            unsafe { crc = _mm_crc32_u8(crc as u64, b); }
        }
        crc as u32
    }
    #[cfg(not(target_feature = "sse4.2"))]
    { crc32c_sw(data, crc) }
}
```

依赖决策：
- 可选：`crc` crate 提供 `CRC_32_ISCSI`（Castagnoli）——最简方案
- 可选：`core::arch::x86_64::_mm_crc32_u64` ——无外部依赖，需要 `target_feature` 门控
- **建议**：先用 `crc` crate 的 `CRC_32_ISCSI` 实现（纯软件已正确），后续添加硬件检测

### 验证
```
cargo test crc — CRC 测试通过
cargo test jset — Journal 测试通过（CRC 变更）
# 手动验证：在 x86_64 机器上确认硬件路径被启用
RUSTFLAGS="-C target-feature=+sse4.2" cargo test — 无报错
```

### 回滚点
如果 CRC 值与当前不兼容，journal 和 btree 的旧数据在校验时会失败。建议先合并 CRC 变更但推迟启用验证（允许新旧 CRC 同时存在），等 P4/P2 完成后再切。

---

## Phase 4: Journal 序列化

### 目标
将 Journal Jset 从 bincode 序列化改为固定布局。

### 依赖
- P3（CRC 基础设施）

### 修改清单

| 文件 | 变更 | 风险 |
|------|------|------|
| `journal/jset.rs` | 重写 `Jset` 为双结构：`JsetHeader`（固定布局）+ `JsetEntryHeader`（固定布局） | **高** |
| `journal/jset.rs` | 重写 `serialize_padded()`：直接 buf 填充 + 单次 CRC | 中 |
| `journal/jset.rs` | 重写 `deserialize()`：直接 struct 读取 | 中 |
| `journal/jset.rs` | 更新 `verify_crc()`、`crc32_matches()` | 低 |
| `journal/types.rs` | 重写 `append()`：直接写入 buf，消除 5 次 serde | 中 |
| `journal/types.rs` | `add_entry()` 更新 | 低 |
| `journal/validate.rs` | 更新校验逻辑（如有硬编码偏移） | 低 |

### JsetHeader 设计

```rust
#[repr(C)]
pub struct JsetHeader {
    pub magic: [u8; 8],        // 8
    pub seq: u64,              // 8
    pub last_seq: u64,         // 8
    pub crc32: u32,            // 4    (被 CRC 覆盖时置 0)
    pub entry_count: u32,      // 4
    pub version: u32,          // 4
    pub csum_type: u8,         // 1
    pub pad: [u8; 15],         // 15   对齐到 64 字节（可选）
}  // 52→64 bytes

#[repr(C)]
pub struct JsetEntryHeader {
    pub btree_type: u8,        // 1
    pub entry_type: u8,        // 1
    pub payload_len: u16,      // 2
    pub has_last: u16,         // 2
    pub has_prev: u16,         // 2
}  // 8 bytes, followed by `payload_len` bytes of btree_keys
```

### append() 优化

```rust
pub async fn append(&self, btree_type, entries, must_flush, backend) -> Result<u64, ...> {
    // 1. 计算 entries 总大小（无需序列化）
    let total_payload: usize = entries.iter().map(|e| /* bincode::serialized_size */).sum();
    let entry_count = entries.len();
    let total_size = size_of::<JsetHeader>() + entry_count * size_of::<JsetEntryHeader>() + total_payload;
    let block_aligned = total_size.next_multiple_of(JSET_BLOCK_SIZE as usize);
    let req_u64s = block_aligned.div_ceil(8) as u32;

    // 2. 获取 journal slot
    let res = self.journal_res_get_fast(Watermark::Btree, req_u64s)?;

    // 3. 直接填充 buf
    let mut buf = vec![0u8; block_aligned];
    let hdr = JsetHeader { seq: res.seq, ... };
    unsafe { ptr::write(buf.as_mut_ptr() as *mut JsetHeader, hdr); }
    // ... entries ...

    // 4. 单次 CRC
    let crc = crc32c(0, &buf);
    unsafe { ... write crc back to header offset ... }

    // 5. 提交
    self.add_entry(&res, &buf);
    ...
}
```

### 验证
```
cargo test journal — 所有 journal 测试通过
cargo test jset — roundtrip 测试通过  
# 手动验证 journal 序列化效率提升
```

### 回滚点
如果 journal 测试大量失败，可能是旧测试数据硬编码了 bincode 格式。需要更新所有 `make_test_jset()` 和硬编码的序列化数据。

---

## Phase 5: 收尾修复

### 目标
修复发现的小差异，确认 superblock 对齐。

### 修改清单

| 文件 | 变更 | 风险 |
|------|------|------|
| `config.rs` | 修复 off-by-one：错误消息 `65536` → `65535`，或限制改为 `512..=65536` | 极低 |
| `storage/superblock.rs` | 确认 `BchSb` 结构体字段顺序/bincode vs repr(C) | 低 |
| `btree/key.rs` | `BkeyFormat` 的 `#[repr(C, packed)]`（P1 已改，确认） | 低 |
| 测试文件 | 更新所有硬编码的旧格式测试数据 | 低 |

### 验证
```
cargo test — 全绿
```

---

## 实施顺序决策

### 建议串行执行（原因：各阶段依赖紧）

```
Step 1: Phase 1 (基础结构)
  → 编译通过，测试通过
Step 2: Phase 3 (CRC 基础设施)
  → 独立于 btree/journal 序列化，先就位
Step 3: Phase 2 (Btree 序列化)
  → 核心变更，依赖 P1 的新结构体，且使用 P3 的 CRC
Step 4: Phase 4 (Journal 序列化)
  → 依赖 P3 CRC，不依赖 btree 变更，可与 P2 并行（如果资源允许）
Step 5: Phase 5 (收尾)
  → 最后验证全量测试
```

### 并行选项
P2 (btree) 和 P4 (journal) 可并行开发——两者无代码依赖。两者都依赖 P3 CRC，但 P3 接口简单，可先约定接口然后 mock/fallback 实现并行。

---

## 风险清单

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| BtreeNodeHeader 新布局偏移错误 | data corruption | 中 | 静态 assert size_of + offset_of 测试 |
| BsetHeader 反序列化时偏移计算偏 16B | 读错 all entries | 中 | roundtrip 测试覆盖 |
| CRC 多项式切换导致旧数据校验失败 | 旧 journal/节点无法读取 | 中 | 读路径兼容新旧 CRC |
| 测试数据硬编码了 bincode 格式 | 大量测试失败 | 高 | 逐一更新或重写测试 |
| `read_unaligned` 在 BtreeNodeHeader repr(C,packed) 上 | 需要大量 unsafe | 中 | 用 `ptr::read_unaligned` 安全访问 |

## 测试策略

| 测试类型 | 覆盖范围 | 工具 |
|---------|---------|------|
| Roundtrip: serialize → deserialize | BtreeNode、Bset、Jset | `#[test]` |
| 布局 assert: size_of + offset_of | BtreeNodeHeader、BsetHeader、JsetHeader | `static_assertions` crate |
| CRC 向量测试 | CRC32C 与已知向量对比 | `#[test]` |
| 旧格式兼容解析 | 解析已知的旧 bincode 数据 | `#[test]` |
| 模糊测试 | 随机数据反序列化不 panic | `proptest` 或 `fuzz` |
