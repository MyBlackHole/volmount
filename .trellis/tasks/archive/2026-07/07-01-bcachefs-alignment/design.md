# 全面对齐 bcachefs 磁盘布局 — 技术设计

## 1. 目标布局总览

### 1.1 Btree Node 磁盘格式（变更最大）

**当前（bincode）：**
```
[bincode(BtreeNodeHeader) ~66-114B] + [8B align pad] + [packed entries] + [zeros to 4096]
```

**目标（repr(C,packed)）：**
```
[BtreeNodeHeader repr(C,packed)] + [[BsetHeader x N]] + [packed entries] + [zeros to BLOCK_SIZE]
```

### 1.2 Journal 磁盘格式

**当前：**
```
[bincode(Jset) 含 entries Vec] + [pad to JSET_BLOCK_SIZE=4096]  // 5 serde + 2 CRC
```

**目标：**
```
[fixed-size JsetHeader repr(C)] + [JsetEntry x M] + [pad to 4096]  // 1 memcpy + 1 CRC
```

---

## 2. Core Struct Layout Definitions

### 2.1 `Bpos` — target

```rust
/// On-disk bpos
/// bcachefs: struct bpos { __le64 inode; __le64 offset; __le32 snapshot; }
#[repr(C)]
pub struct Bpos {
    pub inode: u64,      // 8
    pub offset: u64,     // 8
    pub snapshot: u32,   // 4
}  // 20 bytes, repr(C) natural pad to 24 if followed by non-packed field
```

Current state: no `#[repr(C)]` → compiler may reorder fields.
Change: add `#[repr(C)]`.

### 2.2 `BkeyFormat` — target

```rust
/// bcachefs: struct bkey_format __packed
#[repr(C, packed)]
pub struct BkeyFormat {
    pub key_u64s: u8,                              // 1
    pub nr_fields: u8,                             // 1
    pub bits_per_field: [u8; BKEY_NR_FIELDS],      // 5
    pub field_offset: [u64; BKEY_NR_FIELDS],       // 40 (5 * 8)
}  // 47 bytes — matches __packed C size
```

Current: `#[repr(C)]` → 1 byte padding (48 bytes).  
Change: switch to `#[repr(C, packed)]` → 47 bytes, matches C.

### 2.3 `struct bset` (new) — BsetHeader

```rust
/// 磁盘 bset header — 对齐 bcachefs `struct bset`
///
/// bcachefs C: `struct bset { __le64 seq; __le64 journal_seq; __le32 flags;
///                            __le16 version; __le16 u64s; } __packed __aligned(8)`
/// — 24 字节
#[repr(C, packed)]
pub struct BsetHeader {
    pub seq: u64,           // 0-7   最近一次写入此 bset 的 journal seq
    pub journal_seq: u64,   // 8-15  此 bset 被写入时的 journal seq
    pub flags: u32,         // 16-19 BSET_* 标志位（csum type、offset 等）
    pub version: u16,       // 20-21 bset 格式版本
    pub u64s: u16,          // 22-23 整个 bset 的 u64 数（含 header）
}  // 24 bytes — 精确对齐 bcachefs struct bset
```

This is a **new on-disk structure**. volmount 当前没有它。`data_bytes = u64s * 8 - sizeof(BsetHeader)`。

### 2.4 `BtreeNodeHeader` — target

基于当前结构 + repr(C,packed) + 审阅修正（方案 A：Bpos 拆为内联字段，消除 padding）：

```rust
#[repr(C, packed)]
pub struct BtreeNodeHeader {
    pub magic: u32,                      // 4    magic number
    pub version: u16,                    // 2    格式版本 (v2 = 新固定布局)
    pub level: u8,                       // 1    btree level
    pub node_type: u8,                   // 1    btree node type
    pub key_count: u32,                  // 4    entries 总数
    pub bset_count: u16,                 // 2    bset headers 数量
    pub crc32: u32,                      // 4    节点 CRC (header + bsets + entries)
    pub seq: u64,                        // 8    journal seq
    pub bucket_addr: u64,                // 8    bucket 地址 (COW)
    pub parent_addr: u64,                // 8    父节点 bucket 地址

    // Bpos 拆为三个独立字段 → packed 上下文中无 padding（20 精确字节）
    pub min_key_inode: u64,              // 8    subtree min key — inode
    pub min_key_offset: u64,             // 8    subtree min key — offset
    pub min_key_snapshot: u32,           // 4    subtree min key — snapshot

    pub max_key_inode: u64,              // 8    subtree max key — inode
    pub max_key_offset: u64,             // 8    subtree max key — offset
    pub max_key_snapshot: u32,           // 4    subtree max key — snapshot
}  // 82 bytes — 无 padding

impl BtreeNodeHeader {
    pub fn min_key(&self) -> Bpos {
        Bpos::new(self.min_key_inode, self.min_key_offset, self.min_key_snapshot)
    }
    pub fn set_min_key(&mut self, key: Bpos) {
        self.min_key_inode = key.inode;
        self.min_key_offset = key.offset;
        self.min_key_snapshot = key.snapshot;
    }
    pub fn max_key(&self) -> Bpos {
        Bpos::new(self.max_key_inode, self.max_key_offset, self.max_key_snapshot)
    }
    pub fn set_max_key(&mut self, key: Bpos) {
        self.max_key_inode = key.inode;
        self.max_key_offset = key.offset;
        self.max_key_snapshot = key.snapshot;
    }
}
```

### 2.5 BtreeNode 内存表示（Mutex 保护 + flags 等）

当前 `BtreeNode` 是内存结构（不会直接写到磁盘）。其 header 部分在序列化时通过 `serialize_to_bucket` 抽取为 `BtreeNodeHeader`。这个分离设计保持不变——只改序列化时的转换逻辑。

### 2.6 `BsetTree` — 内存结构（不串行化）

```rust
pub struct BsetTree {
    pub data_offset: u32,      // 相对于节点 data 缓冲区起始
    pub end_offset: u32,       // 相对于节点 data 缓冲区结束
    pub aux_offset: u32,       // aux search tree 偏移
    pub size: u16,             // entries 数量
    pub extra: u16,            // 填充
}
```

BsetTree 保持纯内存——反序列化时从 BsetHeader.data_bytes 重建。  
serialize 时：BsetHeader.data_bytes = end_offset - data_offset。

---

## 3. Btree 序列化 Pipeline

### 3.1 serialize_to_bucket (写路径)

```
BtreeNode → serialize_to_bucket():

1. 从 BtreeNode.sets[] 收集每 bset 信息
2. 写 BtreeNodeHeader 到 buf (memcpy/直接填充)
3. 对 set[0] (主压缩集): 写 BsetHeader( seq, journal_seq, flags, version, u64s )
   → data = u64s * 8 - sizeof(BsetHeader) 字节的 packed entries
   → 跟 packed entries (BtreeNode.data[data_offset..end_offset])
4. 对 set[1..] (增量集) 同样处理（当前 volmount 只有 set[0] 有 entry）
   → 当前大多数节点只有 1 个 bset (bset_count=1)
5. 计算 buf 的 CRC32C (覆盖 header + bset headers + entries)
6. 填零到 BLOCK_SIZE

buf = [BtreeNodeHeader(82B)] + [BsetHeader(24B)] + [...BsetHeader x N] + [packed entries] + [zeros to 4096]
```

### 3.2 deserialize_from_slice (读路径)

```
buf (4096B) → deserialize_from_slice():

1. ptr::read_unaligned 将 buf 前 82B 读为 BtreeNodeHeader
2. 验证 magic + CRC32C
3. header + first BsetHeader offset = 82
4. 从 BsetHeader.u64s 计算 data_bytes = u64s * 8 - sizeof(BsetHeader)
5. 将 entries 拷贝到 BtreeNode.data Vec
6. 重建 BsetTree 元数据 (data_offset/end_offset + aux tree)
```

### 3.3 关键变化

| 方面 | 当前 | 目标 |
|------|------|------|
| Header 写入 | bincode::serialize → Vec | 直接填充到 buf（memcpy 或 ptr 写入） |
| Bset header | 无 | 新增 24B BsetHeader 在每个 bset 前（对齐 bcachefs struct bset） |
| Header + entries | 分开处理（header 后 8B align） | 连续布局 |
| CRC | CRC32C 仅覆盖 entries | CRC32C 覆盖 header + bsets + entries |
| 尾填充 | resize 到 BLOCK_SIZE | 相同（填零到 4096） |

---

## 4. CRC 基础设施

### 4.1 当前状态
- `Crc32CHasher` 使用 `crc32fast` crate（IEEE CRC32 多项式 `0xEDB88320`）
- bcachefs 使用 Castagnoli CRC32C 多项式 `0x1EDC6F41`
- 当前纯软件实现

### 4.2 目标

```rust
// 使用 Castagnoli 多项式 0x1EDC6F41
// x86_64: 使用 SSE 4.2 CRC32 指令 (crc32q, crc32l)
// ARM: 使用 ARMv8 CRC32 指令

#[cfg(target_arch = "x86_64")]
pub fn crc32c_hw(data: &[u8], crc: u32) -> u32 {
    // 使用 _mm_crc32_u64 / _mm_crc32_u8 内联
    // 每次处理 8 字节，剩余用 1 字节指令
}

// 回退：纯软件 CRC32C 表驱动实现
pub fn crc32c_sw(data: &[u8], crc: u32) -> u32 {
    // 基于 Castagnoli 多项式的查表法
}
```

依赖决策：使用 `crc` crate 的 `CRC_32_ISCSI`（已经是 Castagnoli 多项式，别名 CRC32C），或直接内联 SSE4.2 指令。

### 4.3 覆盖范围变更
- **当前**：CRC32C 只覆盖 packed entries（btree node）或 Jset entries（journal）
- **目标**：CRC32C 覆盖**完整的序列化块**（header + bset headers + entries）

---

## 5. Journal 序列化

### 5.1 当前结构

```rust
pub struct Jset {
    pub magic: [u8; 8],
    pub seq: u64,
    pub last_seq: u64,
    pub crc32: u32,
    pub entry_count: u32,
    pub version: u32,
    pub csum_type: u8,
    pub _pad0: [u8; 3],  // 显式填充到某个边界
    pub entries: Vec<JsetEntry>,  // bincode serde 递归
}
```

### 5.2 目标结构

```rust
#[repr(C)]
pub struct JsetHeader {
    pub magic: [u8; 8],        // 8    magic
    pub seq: u64,              // 8    journal seq number
    pub last_seq: u64,         // 8    依赖的最新 seq
    pub crc32: u32,            // 4    CRC32C of full block
    pub entry_count: u32,      // 4    number of entries
    pub version: u32,          // 4    format version
    pub csum_type: u8,         // 1    checksum type
    pub pad: [u8; 15],         // 15   pad to 64 bytes
}  // 64 bytes

/// Fixed-size on-disk journal entry header
#[repr(C)]
pub struct JsetEntryHeader {
    pub btree_type: u8,        // 1    BtreeId as u8
    pub entry_type: u8,        // 1    JsetEntryType as u8
    pub payload_len: u16,      // 2    payload (btree_keys) byte length
    pub has_last: u16,         // 2    last key offset
    pub has_prev: u16,         // 2    prev key offset
}  // 8 bytes
```

### 5.3 序列化路径

```
Jset::serialize():
  1. 分配固定 buf: [JsetHeader(64B)] + [[JsetEntryHeader(8B) + payload] x N] + pad to 4096
  2. 直接填充每个字段（ptr 写入）
  3. CRC 覆盖全 buf（CRC 字段置 0 后计算）
  4. pad 填零
```

### 5.4 append() 优化

当前 5 次 serde → 1 次直接填充 + 1 次 CRC：

```
append():
  1. 预计算 entries 内存布局 (header + payloads)，直接计算总大小
  2. journal_res_get_fast(req_u64s) — 大小已知，无需预序列化
  3. 写入 buf（一次 memcpy 或 ptr 写入）
  4. add_entry()
```

---

## 6. 兼容性与迁移策略

### 6.1 Magic 值
保留 `0x56544E42`（"BNTV"）。不兼容旧 bcachefs 格式——这是安全特性而非问题。

### 6.2 旧格式检测
- 尝试以新格式解析，失败 fallback 到旧 bincode 格式
- 通过 magic 区分：`BTREE_NODE_MAGIC = 0x56544E42` 有两种含义需区分
- 或：在读路径中**先尝试新格式**，如果 magic 匹配则用新格式，否则回退 bincode

实际上最简单的方式：**旧格式文件在新格式下 magic 不同**。旧 bincode 格式的 header 的起始字节是 bincode 编码的 `0x56544E42`；新格式的前 4B 是 packed `0x56544E42`。bincode 编码 u32 也是 4B LE，所以前 4 字节看起来一样。

更可靠的方式：使用 `version` 字段区分。
- 旧 bincode 格式：`version = 1`
- 新固定格式：`version = 2`

### 6.3 迁移
- 本 task 首次启动后会写新格式
- 读路径同时支持新旧格式（短期的向前兼容）
- 写路径只写新格式（version = 2）
- 在一段过渡期后移除旧格式解析代码

---

## 7. 不在此 task 中的变更

- `BtreeKey` 内存布局改为自然对齐——这是纯内存优化，不涉及磁盘格式
- BchDataType 扩展变体——保留不动
- Bucket/Alloc 内存在 struct 修改——不涉及磁盘对齐
