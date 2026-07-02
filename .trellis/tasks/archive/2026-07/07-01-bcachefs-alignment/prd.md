# 全面对齐 bcachefs 磁盘布局与序列化

## Goal

volmount 当前使用 bincode 序列化（自描述格式），与 bcachefs C 的固定 C struct + memcpy 磁盘格式不兼容。本 task 的目标是将 volmount 的 **on-disk 结构体布局与 bcachefs C 全面对齐**，但保留 volmount 自己的 magic 标识和扩展字段。

### 对齐策略

**选项 B：布局对齐/独立 magic**
- 结构体字段类型、顺序、padding 与 bcachefs C 一致
- 使用 `#[repr(C, packed)]` 替代 bincode serde
- 保留 volmount 自己的 magic 值（`0x56544E42`）
- 可保留 volmount-only 扩展字段（用 #[serde(default)] 或版本号管理）
- 不追求 byte-level 二进制兼容，但 struct-level 兼容——未来的迁移工具可直接 memcpy 字段

## 背景（已确认的差异）

### 🔴 必须修复的差异
- `BtreeNodeHeader` — 使用 bincode serde 而非固定 C struct 布局，无 `#[repr(C)]`
- 缺少 `struct bset` 磁盘结构 — bcachefs 每个 node 含 N 个 16 字节 bset header，volmount 不持久化
- serialization pipeline 完全不同 — bincode 变长编码 vs C 固定偏移 memcpy
- `BkeyFormat` — 因 `#[repr(C)]` 引入 1 字节 padding（48B vs C 47B）
- `Bpos` 无 `#[repr(C)]` — 编译器可重排字段
- Journal `serialize_padded` — 每条 append 5 次 serde + 2 次 CRC

### 🟡 应修复的差异
- CRC — 当前使用纯软件 `crc32fast`(IEEE) vs bcachefs 硬件 CRC32C(Castagnoli)
- `BchDataType` — volmount 在 `BCH_DATA_NR=11` 后新增 4 个变体——保留但确认安全
- `config.rs` block_size 验证 off-by-one（错误消息写 65536 而非 65535）

### 🟢 与对齐无关的可选项
- `Bucket` 是纯内存结构（不直接序列化到磁盘），不影响磁盘对齐
- `BtreeKey` packed vs aligned 布局——当前通过在内存中使用 packed 避免 on-disk 兼容问题

### ✅ 已对齐（保持）
- `BkeyPacked` — `#[repr(C, packed)]`，3B header → 与 C `struct bkey_packed` 完全一致
- bitstream pack/unpack 协议 — LE 大端位流编码与 `bkey_pack()` 一致
- 所有 `#[repr(u8)]` 枚举值 — 与 C enum 值一致
- split/merge 阈值常量 — 3/4, 1/3, 3/5, 5/12 全部对齐
- 基本常量 — `BLOCK_SIZE=4096`, `BTREE_MAX_DEPTH=8`, `MAX_BSETS=3`, `BSET_CACHELINE=256` 全部对齐

## Requirements

1. `Bpos` 添加 `#[repr(C)]` 确保字段顺序
2. `BtreeNodeHeader` 改为 `#[repr(C, packed)]` 固定布局，字段顺序/类型与 `struct btree_node` 对齐
3. 添加 `struct bset` 磁盘结构（与 bcachefs `struct bset` 对齐）
4. `BkeyFormat` 修复 1 字节 padding（改为 packed 或确认不影响磁盘布局）
5. Journal Jset 改为固定布局 + 单次序列化 + CRC32C
6. CRC 从 `crc32fast`(IEEE) 迁移到 Castagnoli CRC32C + x86_64 硬件指令支持
7. Superblock 布局对齐（当前已固定 4KB，确认字段对齐）
8. 所有 on-disk 结构使用 `#[repr(C, packed)]`，不用 bincode
9. 序列化管道改为直接 buf 读写

## Acceptance Criteria

- [ ] `Bpos` 有 `#[repr(C)]`，字段顺序确认
- [ ] `BtreeNodeHeader` 使用 `#[repr(C, packed)]`，字段与 bcachefs `struct btree_node` 一致（magic 保留 volmount 值）
- [ ] `struct bset`(on-disk) 已添加，序列化/反序列化支持（MAX_BSETS=3）
- [ ] `BkeyFormat` padding 已处理
- [ ] Jset 使用固定 `#[repr(C)]` 布局 + 单次序列化
- [ ] CRC 使用 Castagnoli CRC32C + 硬件指令（x86_64）
- [ ] 所有 bincode on-disk 序列化路径已替换
- [ ] `BtreeNode` roundtrip 测试通过（序列化→反序列化→数据一致）
- [ ] Jset roundtrip 测试通过
- [ ] 现有测试全部通过
- [ ] magic 值保持 `0x56544E42`

## Out of Scope

- Btree 算法逻辑对齐（语义已对齐）
- 性能优化作为独立目标（布局对齐带来的性能提升是副产品）
- bcachefs-tools 工具链集成或互操作测试
- 多设备/副本功能
- 修改 volmount-only 扩展功能（BchDataType 扩展变体、group 字段、version: u32）

## Design Decisions

### D1 — 与 bcachefs 二进制兼容级别
**决策：布局对齐 + 独立 magic（选项 B）**
- on-disk 结构体字段类型/顺序/padding 与 bcachefs C 一致
- magic 值保持 `0x56544E42`
- 保留 volmount-only 扩展字段（BchDataType 扩展变体、group、version: u32）
- 独立 magic 防止 bcachefs-tools 误挂载，是安全保护
