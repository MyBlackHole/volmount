//! Journal entry (Jset) — 对应 bcachefs `struct jset`
//!
//! Jset 是 journal 中的基本条目单位。每个 Jset 包含一个或多个 entries，
//! 每个 entry 记录对某个 btree type 的批量修改。
//!
//! # 格式（v2，repr(C) 固定布局）
//!
//! ```text
//! ┌────────────────────────────────────┐
//! │ JsetHeader        (64 B fixed)     │
//! ├────────────────────────────────────┤
//! │ JsetEntryHeader 0 (8 B)           │
//! │ JsetEntryHeader 0 payload (变长)   │
//! │ JsetEntryHeader 1 (8 B)           │
//! │ JsetEntryHeader 1 payload (变长)   │
//! │ ...                                │
//! ├────────────────────────────────────┤
//! │ 零填充到 JSET_BLOCK_SIZE (4096)    │
//! └────────────────────────────────────┘
//! ```
//!
//! CRC32C 覆盖：从 JsetHeader（crc32 字段置 0）到最后一个 entry payload 末尾。

use serde::{Deserialize, Serialize};
use std::mem::size_of;
use std::ptr;

use crate::types::StorageError;
use crc::Crc;

/// CRC32C 算法（bcachefs 对齐）：Castagnoli 多项式（0x1EDC6F41，lsb 0x82F63B78）
pub(crate) const CRC32C: Crc<u32> = Crc::<u32>::new(&crc::CRC_32_ISCSI);

// ═══════════════════════════════════════════════════════════════
// CRC32C 硬件加速 + 自动调度
// ═══════════════════════════════════════════════════════════════

/// CRC32C Castagnoli 查表（反射形式 0x82F63B78）
const CRC32C_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0x82F63B78u32 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
};

/// CRC32C 纯软件实现（Castagnoli 多项式 0x1EDC6F41，反射 0x82F63B78）
///
/// `crc` 为初始 seed（0 表示从头开始，非零用于分块连续计算）。
/// 对应 bcachefs `crc32c_le_bch(crc, buf, len)` 语义。
pub fn crc32c_sw(data: &[u8], crc: u32) -> u32 {
    let mut crc = !crc;
    for &byte in data {
        let idx = ((crc as u8) ^ byte) as usize;
        crc = CRC32C_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

/// CRC32C SSE4.2 硬件加速（x86_64 only）
///
/// 使用 `_mm_crc32_u64` 一次处理 8 字节，剩余用 `_mm_crc32_u8`。
/// 调用方必须确保 SSE4.2 可用（通过 is_x86_feature_detected 或 compile-time feature gate）。
///
/// **重要**: 硬件 CRC32 指令不做标准 CRC32 的初始补码（!crc）和最终补码（!result）。
/// 因此我们在进入指令前将 `crc` 取补，在返回前将结果取补，与 `crc32c_sw` 语义保持一致。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw_impl(data: &[u8], crc: u32) -> u32 {
    // `!crc` 是标准 CRC32 初始值（seed=0 → 0xFFFFFFFF，链式调用 seed=X → !X）
    let mut crc64 = (!crc) as u64;
    for chunk in data.chunks_exact(8) {
        let val: u64 = u64::from_le_bytes(chunk.try_into().unwrap());
        crc64 = core::arch::x86_64::_mm_crc32_u64(crc64, val);
    }
    // _mm_crc32_u8 操作低 32 位
    let mut ret = crc64 as u32;
    for &b in data.chunks_exact(8).remainder() {
        ret = core::arch::x86_64::_mm_crc32_u8(ret, b);
    }
    // 最终取补，与 crc32c_sw 的 !crc 末尾语义一致
    !ret
}

/// CRC32C 自动选择硬件/软件路径
///
/// x86_64: 运行时检测 SSE4.2，有则用硬件路径，否则回退软件路径。
/// 非 x86_64: 始终使用软件路径。
///
/// `crc` 为初始 seed（用于分块连续计算），单次调用传 0。
/// 对应 bcachefs `crc32c_le_bch(0, buf, len)`。
pub fn crc32c(data: &[u8], crc: u32) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        #[cfg(target_feature = "sse4.2")]
        {
            // 编译时已知 SSE4.2 可用（如 RUSTFLAGS="-C target-feature=+sse4.2"）
            unsafe { crc32c_hw_impl(data, crc) }
        }
        #[cfg(not(target_feature = "sse4.2"))]
        {
            // 运行时检测：std crate 中 is_x86_feature_detected! 始终可用
            if std::is_x86_feature_detected!("sse4.2") {
                unsafe { crc32c_hw_impl(data, crc) }
            } else {
                crc32c_sw(data, crc)
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        crc32c_sw(data, crc)
    }
}

/// Journal 魔数（原始 volmount 格式）
pub const JOURNAL_MAGIC: [u8; 8] = *b"VOLM_JNL";

/// 新 volatile 魔数（volmount + 时间戳版本，对应 bcachefs `JSET_MAGIC` / `VMNT_JSET_MAGIC`）
pub const VMNT_JSET_MAGIC: [u8; 8] = *b"VMNTJNL0";

/// Jset padding 对齐块大小（对齐 backend block size）
pub const JSET_BLOCK_SIZE: u32 = 4096;

/// 当前 Jset 格式版本号（对应 bcachefs `bcachefs_metadata_version`）
/// v1: bincode 序列化格式
/// v2: repr(C) 固定布局
pub const JSET_VERSION: u32 = 2;

/// 校验和类型：无校验
pub const CSUM_TYPE_NONE: u8 = 0;
/// 校验和类型：crc32c
pub const CSUM_TYPE_CRC32C: u8 = 1;

/// 当前 Jset entry header 格式版本。
///
/// `JsetHeader::version` 描述整个 Jset 的磁盘布局；entry version 描述单个
/// `JsetEntryHeader` 的局部布局，避免未来 entry header 扩展时只能依赖外层版本。
pub const JSET_ENTRY_VERSION: u8 = 1;

const JSET_HEADER_CRC32_OFFSET: usize = 24;

// ═══════════════════════════════════════════════════════════════
// Jset 固定布局数据结构（repr(C)，直接 ptr 读写）
// ═══════════════════════════════════════════════════════════════

/// Jset 头部（64 字节固定，repr(C)），对应 bcachefs `struct jset`
///
/// 磁盘布局：
/// - magic:      [0..8)    魔数
/// - seq:        [8..16)   递增序列号
/// - last_seq:   [16..24)  最老未 flush seq
/// - crc32:      [24..28)  CRC32C 校验和（计算时此字段置 0）
/// - entry_count: [28..32)  包含的 entry 数量
/// - version:    [32..36)  格式版本
/// - csum_type:  [36..37)  校验和类型
/// - pad:        [37..64)  填充到 64 字节
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JsetHeader {
    pub magic: [u8; 8],
    pub seq: u64,
    pub last_seq: u64,
    pub crc32: u32,
    pub entry_count: u32,
    pub version: u32,
    pub csum_type: u8,
    pub pad: [u8; 27],
}

/// Jset entry 头部（8 字节固定，repr(C)），对应 bcachefs `struct jset_entry`
///
/// 磁盘布局：
/// - btree_type:  [0]    btree 类型
/// - entry_type:  [1]    entry 类型（JsetEntryType 的 u8 值）
/// - version:     [2]    entry header 格式版本
/// - flags:       [3]    保留 flags（未来用于 has_last/has_prev bit 扩展）
/// - payload_len: [4..6) payload 字节数
/// - has_last:    [6]    是否有上一 Jset（journal 链表遍历）
/// - has_prev:    [7]    是否有下一 Jset
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JsetEntryHeader {
    pub btree_type: u8,
    pub entry_type: u8,
    pub version: u8,
    pub flags: u8,
    pub payload_len: u16,
    pub has_last: u8,
    pub has_prev: u8,
}

/// Jset entry 的高层表示（header + 反序列化后的 payload）
///
/// `payload` 以 `Vec<u8>` 存储序列化的 btree keys（bincode 格式）。
#[derive(Debug, Clone)]
pub struct RawJsetEntry {
    pub hdr: JsetEntryHeader,
    pub payload: Vec<u8>,
}

impl RawJsetEntry {
    /// 创建新的 RawJsetEntry
    pub fn new(btree_type: u8, entry_type: u8, payload: Vec<u8>) -> Result<Self, StorageError> {
        let payload_len = u16::try_from(payload.len()).map_err(|_| {
            StorageError::InvalidData(format!(
                "jset entry payload too large: {} > {}",
                payload.len(),
                u16::MAX
            ))
        })?;
        Ok(Self {
            hdr: JsetEntryHeader {
                btree_type,
                entry_type,
                version: JSET_ENTRY_VERSION,
                flags: 0,
                payload_len,
                has_last: 0,
                has_prev: 0,
            },
            payload,
        })
    }
}

// ═══════════════════════════════════════════════════════════════
// 旧格式保留类型
// ═══════════════════════════════════════════════════════════════

/// JsetEntry 的类型（对齐 bcachefs `enum journal_entry_type`）
///
/// 值定义与 bcachefs 一致以保证格式兼容性：
/// - 0:  BtreeKeys — btree insert/delete keys
/// - 1:  BtreeRoot — root pointer update
/// - 2:  Blacklist — 标记已落盘的 seq 范围
/// - 3:  Overwrite — overwrite entry（bcachefs BCH_JSET_ENTRY_overwrite）
/// - 6:  BtreeNodeRewrite — btree node rewrite（bcachefs BCH_JSET_ENTRY_btree_node_write）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsetEntryType {
    /// btree insert/delete keys
    BtreeKeys = 0,
    /// root pointer update
    BtreeRoot = 1,
    /// blacklist 条目：标记已落盘的 seq 范围（recovery 时跳过）
    Blacklist = 2,
    /// overwrite entry：覆盖式写入（bcachefs BCH_JSET_ENTRY_overwrite）
    Overwrite = 3,
    /// btree node rewrite entry：btree 节点重写（bcachefs BCH_JSET_ENTRY_btree_node_write）
    BtreeNodeRewrite = 6,
}

impl JsetEntryType {
    /// 从 u8 转换到 JsetEntryType，未知值返回 None
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(JsetEntryType::BtreeKeys),
            1 => Some(JsetEntryType::BtreeRoot),
            2 => Some(JsetEntryType::Blacklist),
            3 => Some(JsetEntryType::Overwrite),
            6 => Some(JsetEntryType::BtreeNodeRewrite),
            _ => None,
        }
    }
}

/// 分块 CRC32C 计算（对齐 bcachefs 的 crc32c 分块校验）
///
/// bcachefs 将 Jset 数据按 4KB block 分块后分别计算 CRC 再合并。
///
/// 使用 `crc::Digest` 的 `update()` 方法实现多块追加计算，
/// 与 bcachefs 的 `crc32c_le_bch()` 分块语义一致。
///
/// # 示例
///
/// ```text
/// let mut hasher = Crc32CHasher::new();
/// hasher.update(&block1);
/// hasher.update(&block2);
/// let result = hasher.finalize();
/// ```
pub struct Crc32CHasher {
    digest: crc::Digest<'static, u32>,
}

impl Crc32CHasher {
    /// 创建新的 CRC32C 计算器（初始值 0）
    pub fn new() -> Self {
        Self {
            digest: CRC32C.digest(),
        }
    }

    /// 追加数据块到 CRC 计算
    pub fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    /// 完成 CRC 计算，返回最终的 32 位校验值
    pub fn finalize(&self) -> u32 {
        self.digest.clone().finalize()
    }

    /// 从单个数据块计算 CRC32C（自动选择硬件/软件路径）
    pub fn hash(data: &[u8]) -> u32 {
        crc32c(data, 0)
    }
}

impl Default for Crc32CHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Blacklist entry — 对应 bcachefs `struct jset_entry_blacklist`
///
/// 写回 journal 状态时将当前已落盘的 journal seq 范围写入 blacklist entries。
/// recovery 时 journal_read pass 跳过 blacklist 范围内的 seq。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlacklistEntry {
    /// blacklist 覆盖的最旧 seq
    pub start_seq: u64,
    /// blacklist 覆盖的最新 seq（exclusive）
    pub end_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyJsetEntry {
    btree_type: u8,
    entry_type: JsetEntryType,
    btree_keys: Vec<u8>,
    #[serde(default)]
    has_last: u8,
    #[serde(default)]
    has_prev: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyJset {
    magic: [u8; 8],
    seq: u64,
    last_seq: u64,
    crc32: u32,
    entry_count: u32,
    #[serde(default)]
    version: u16,
    #[serde(default)]
    csum_type: u8,
    entries: Vec<LegacyJsetEntry>,
}

// ═══════════════════════════════════════════════════════════════
// Jset — 高层封装
// ═══════════════════════════════════════════════════════════════

/// Journal entry — 对应 bcachefs `struct jset`
///
/// 一次提交的所有 btree 修改被打包成一个 Jset 写入 journal。
/// v2 格式使用 repr(C) 固定布局序列化。
#[derive(Debug, Clone)]
pub struct Jset {
    /// Jset header（repr(C)，64 字节固定）
    pub header: JsetHeader,
    /// 本 Jset 包含的 entries
    pub entries: Vec<RawJsetEntry>,
}

impl Jset {
    /// 创建新的 Jset（使用原始魔数）
    pub fn new(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: JOURNAL_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION as u32,
                csum_type: CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: Vec::new(),
        }
    }

    /// 创建新的 Jset（使用 volatile 魔数 VMNT_JSET_MAGIC）
    pub fn new_volatile(seq: u64, last_seq: u64) -> Self {
        Self {
            header: JsetHeader {
                magic: VMNT_JSET_MAGIC,
                seq,
                last_seq,
                crc32: 0,
                entry_count: 0,
                version: JSET_VERSION as u32,
                csum_type: CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: Vec::new(),
        }
    }

    /// 计算序列化数据的字节数（不含 padding）
    fn data_size(&self) -> usize {
        let mut sz = size_of::<JsetHeader>();
        for entry in &self.entries {
            sz += size_of::<JsetEntryHeader>();
            sz += entry.payload.len();
        }
        sz
    }

    /// 返回序列化后按 `JSET_BLOCK_SIZE` 填充的字节数。
    ///
    /// append 路径用它预估 journal reservation，避免为了计算大小先构造一份完整 buffer。
    pub fn serialized_padded_len(&self) -> usize {
        let data_size = self.data_size();
        let block_size = JSET_BLOCK_SIZE as usize;
        let pad = (block_size - (data_size % block_size)) % block_size;
        data_size + pad
    }

    fn crc32_over_entries(&self) -> u32 {
        let mut header_zero = self.header;
        header_zero.crc32 = 0;
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &header_zero as *const JsetHeader as *const u8,
                size_of::<JsetHeader>(),
            )
        };
        let mut crc = crc32c(header_bytes, 0);

        for entry in &self.entries {
            let entry_bytes = unsafe {
                std::slice::from_raw_parts(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    size_of::<JsetEntryHeader>(),
                )
            };
            crc = crc32c(entry_bytes, crc);
            if !entry.payload.is_empty() {
                crc = crc32c(&entry.payload, crc);
            }
        }

        crc
    }

    fn from_legacy(legacy: LegacyJset) -> Result<Self, StorageError> {
        let mut entries = Vec::with_capacity(legacy.entries.len());
        for legacy_entry in legacy.entries {
            let mut entry = RawJsetEntry::new(
                legacy_entry.btree_type,
                legacy_entry.entry_type as u8,
                legacy_entry.btree_keys,
            )?;
            entry.hdr.has_last = legacy_entry.has_last;
            entry.hdr.has_prev = legacy_entry.has_prev;
            entries.push(entry);
        }

        let mut jset = Self {
            header: JsetHeader {
                magic: legacy.magic,
                seq: legacy.seq,
                last_seq: legacy.last_seq,
                crc32: 0,
                entry_count: legacy.entry_count,
                version: JSET_VERSION,
                csum_type: legacy.csum_type,
                pad: [0u8; 27],
            },
            entries,
        };
        jset.header.entry_count = jset.entries.len() as u32;
        jset.header.crc32 = jset.crc32_over_entries();
        Ok(jset)
    }

    /// 验证 Jset 的 CRC32 和 magic
    ///
    /// CRC32C 覆盖完整 Jset header（crc32 字段置 0）+ 所有 entries。
    /// 支持两种魔数：JOURNAL_MAGIC（原始格式）和 VMNT_JSET_MAGIC（volatile 格式）。
    pub fn verify(&self) -> bool {
        if self.header.magic != JOURNAL_MAGIC && self.header.magic != VMNT_JSET_MAGIC {
            return false;
        }

        self.crc32_over_entries() == self.header.crc32
    }

    /// 序列化 + CRC32 计算（覆盖完整 header + entries）+ padding 到 JSET_BLOCK_SIZE
    ///
    /// 1. 分配 buf，写 header（crc32=0）+ entries
    /// 2. crc32c(0, &buf[..data_end]) 计算完整数据的 CRC
    /// 3. 写 crc 回 header
    /// 4. 零填充到 JSET_BLOCK_SIZE
    pub fn serialize_padded(&self) -> Result<Vec<u8>, StorageError> {
        let data_size = self.data_size();
        let total_size = self.serialized_padded_len();

        let mut buf = vec![0u8; total_size];

        // 写 header（crc32=0）
        let mut header = self.header;
        header.crc32 = 0;
        header.entry_count = self.entries.len() as u32;

        unsafe {
            let ptr = buf.as_mut_ptr();
            ptr::copy_nonoverlapping(
                &header as *const JsetHeader as *const u8,
                ptr,
                size_of::<JsetHeader>(),
            );

            let mut off = size_of::<JsetHeader>();
            for entry in &self.entries {
                ptr::copy_nonoverlapping(
                    &entry.hdr as *const JsetEntryHeader as *const u8,
                    ptr.add(off),
                    size_of::<JsetEntryHeader>(),
                );
                off += size_of::<JsetEntryHeader>();

                if !entry.payload.is_empty() {
                    ptr::copy_nonoverlapping(
                        entry.payload.as_ptr(),
                        ptr.add(off),
                        entry.payload.len(),
                    );
                    off += entry.payload.len();
                }
            }
            debug_assert_eq!(off, data_size);
        }

        // 计算 CRC（覆盖 header + entries，crc32 字段已置 0）
        let crc = crc32c(&buf[..data_size], 0);

        // 写 CRC 回 header 的 crc32 字段。Vec<u8> 只保证 1 字节对齐，必须使用 unaligned write。
        unsafe {
            ptr::write_unaligned(
                buf.as_mut_ptr().add(JSET_HEADER_CRC32_OFFSET).cast::<u32>(),
                crc,
            );
        }

        Ok(buf)
    }

    /// 从字节反序列化 Jset
    ///
    /// 格式检测逻辑：
    /// 1. 读取 magic 字段，不匹配则返回 None
    /// 2. 读取 version 字段（以 v2 JsetHeader offset=32 的 u32 值）：
    ///    - 如果 2 ≤ version ≤ JSET_VERSION → 以 v2 repr(C) 固定布局读取
    ///    - 如果 version 超出范围（含旧 v1 bincode 格式，其 version 字段是 u16 + csum_type，
    ///      读取为 u32 后可能 > JSET_VERSION）→ 尝试 bincode 回退
    /// 3. v1 bincode 失败 → 返回 None
    pub fn deserialize(data: &[u8]) -> Result<Option<Self>, StorageError> {
        if data.len() < size_of::<JsetHeader>() {
            return Ok(None);
        }

        // 输入 &[u8] 不保证 JsetHeader 对齐，必须使用 read_unaligned。
        let header: JsetHeader = unsafe { ptr::read_unaligned(data.as_ptr().cast::<JsetHeader>()) };

        if header.magic != JOURNAL_MAGIC && header.magic != VMNT_JSET_MAGIC {
            return Ok(None);
        }

        // v2+ 固定布局：version 字段在 JsetHeader 的 u32 偏移位置，
        // 且值必须在 [2, JSET_VERSION] 范围内
        if (2..=JSET_VERSION).contains(&header.version) {
            return Self::parse_v2(data, &header);
        }

        // v1 bincode 回退（或 version 字段因布局不匹配而不可靠时尝试）：
        // 旧格式的 "version: u16 + csum_type: u8" 在 offset 32 处被 JsetHeader 读为 u32，
        // 其值可能 ≥2 或 > JSET_VERSION，不能作为可靠判据。
        if let Ok(legacy) = bincode::deserialize::<LegacyJset>(data) {
            return Self::from_legacy(legacy).map(Some);
        }

        // version > JSET_VERSION 且 bincode 也失败 → 确实无法识别的格式
        Ok(None)
    }

    /// 以 v2+ repr(C) 固定布局读取 Jset。
    ///
    /// `header` 必须来自 `ptr::read_unaligned` 读取的有效数据，
    /// 且已确认 `header.version` 在 2..=JSET_VERSION 范围内。
    fn parse_v2(data: &[u8], header: &JsetHeader) -> Result<Option<Self>, StorageError> {
        let entry_count = header.entry_count as usize;

        let mut entries = Vec::with_capacity(entry_count);
        let mut off = size_of::<JsetHeader>();

        for _ in 0..entry_count {
            if off + size_of::<JsetEntryHeader>() > data.len() {
                return Ok(None);
            }

            let entry_hdr: JsetEntryHeader =
                unsafe { ptr::read_unaligned(data.as_ptr().add(off).cast::<JsetEntryHeader>()) };
            off += size_of::<JsetEntryHeader>();

            if entry_hdr.version > JSET_ENTRY_VERSION {
                return Ok(None);
            }

            let payload_len = entry_hdr.payload_len as usize;
            if off + payload_len > data.len() {
                return Ok(None);
            }

            let payload = if payload_len > 0 {
                data[off..off + payload_len].to_vec()
            } else {
                Vec::new()
            };
            off += payload_len;

            entries.push(RawJsetEntry {
                hdr: entry_hdr,
                payload,
            });
        }

        Ok(Some(Jset {
            header: *header,
            entries,
        }))
    }
}

// ═══════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{Bpos, BtreeEntry, KeyType, KeyValue};

    fn make_test_jset() -> Jset {
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1),
        )])
        .unwrap();
        let entry = RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload).unwrap();
        let mut jset = Jset::new(1, 0);
        jset.entries.push(entry);
        jset.header.entry_count = 1;
        jset
    }

    #[test]
    fn test_jset_roundtrip() {
        let jset = make_test_jset();
        let data = jset.serialize_padded().unwrap();

        // 验证 padding
        assert_eq!(data.len() % JSET_BLOCK_SIZE as usize, 0);

        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.magic, JOURNAL_MAGIC);
        assert_eq!(restored.header.seq, 1);
        assert_eq!(restored.header.entry_count, 1);
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].hdr.btree_type, 0);
        assert_eq!(
            restored.entries[0].hdr.entry_type,
            JsetEntryType::BtreeKeys as u8
        );
        assert_eq!(restored.entries[0].hdr.version, JSET_ENTRY_VERSION);

        // 验证 CRC32（非零）
        assert_ne!(restored.header.crc32, 0);
        assert!(restored.verify());
    }

    #[test]
    fn test_jset_crc32_verify() {
        let jset = make_test_jset();
        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();

        // 正常情况：通过
        assert!(restored.verify());

        // 篡改 crc32 字段 → 不匹配
        let mut tampered = restored.clone();
        tampered.header.crc32 = 0xDEAD_BEEF;
        assert!(!tampered.verify());

        // 篡改 header 字段（seq）→ 全 Jset CRC 覆盖检测到
        let mut tampered_seq = restored.clone();
        tampered_seq.header.seq = 999;
        assert!(!tampered_seq.verify());

        // 篡改 header 字段（last_seq）
        let mut tampered_ls = restored.clone();
        tampered_ls.header.last_seq = 999;
        assert!(!tampered_ls.verify());

        // 篡改 magic
        let mut tampered_magic = restored.clone();
        tampered_magic.header.magic = [0; 8];
        // magic 不匹配在 verify 入口直接返回 false，不经过 CRC 检查
        assert!(!tampered_magic.verify());
    }

    #[test]
    fn test_jset_invalid_magic() {
        let data = vec![0u8; JSET_BLOCK_SIZE as usize];
        let result = Jset::deserialize(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_jset_empty_entries() {
        let jset = Jset::new(42, 10);
        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.seq, 42);
        assert_eq!(restored.header.last_seq, 10);
        assert!(restored.entries.is_empty());
        assert!(restored.verify());
    }

    #[test]
    fn test_jset_header_size() {
        // 验证 JsetHeader 是精确 64 字节
        assert_eq!(size_of::<JsetHeader>(), 64);
    }

    #[test]
    fn test_jset_entry_header_size() {
        // 验证 JsetEntryHeader 是精确 8 字节
        assert_eq!(size_of::<JsetEntryHeader>(), 8);
    }

    #[test]
    fn test_jset_entry_unknown_version_rejected() {
        let jset = make_test_jset();
        let mut data = jset.serialize_padded().unwrap();
        let entry_version_offset = size_of::<JsetHeader>() + 2;
        data[entry_version_offset] = JSET_ENTRY_VERSION + 1;
        assert!(Jset::deserialize(&data).unwrap().is_none());
    }

    #[test]
    fn test_jset_entry_type_from_u8() {
        assert_eq!(JsetEntryType::from_u8(0), Some(JsetEntryType::BtreeKeys));
        assert_eq!(JsetEntryType::from_u8(1), Some(JsetEntryType::BtreeRoot));
        assert_eq!(JsetEntryType::from_u8(2), Some(JsetEntryType::Blacklist));
        assert_eq!(JsetEntryType::from_u8(3), Some(JsetEntryType::Overwrite));
        assert_eq!(
            JsetEntryType::from_u8(6),
            Some(JsetEntryType::BtreeNodeRewrite)
        );
        assert_eq!(JsetEntryType::from_u8(99), None);
    }

    #[test]
    fn test_raw_jset_entry_new() {
        let payload = vec![1, 2, 3, 4];
        let entry = RawJsetEntry::new(5, 1, payload.clone()).unwrap();
        assert_eq!(entry.hdr.btree_type, 5);
        assert_eq!(entry.hdr.entry_type, 1);
        assert_eq!(entry.hdr.version, JSET_ENTRY_VERSION);
        assert_eq!(entry.hdr.payload_len, 4);
        assert_eq!(entry.payload, payload);
    }

    #[test]
    fn test_raw_jset_entry_rejects_payload_len_overflow() {
        let payload = vec![0u8; u16::MAX as usize + 1];
        assert!(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload).is_err());
    }

    #[test]
    fn test_jset_deserialize_legacy_v1_bincode() {
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(7, 700, 0),
            KeyType::Normal,
            KeyValue::extent(0x7000, 1),
        )])
        .unwrap();
        let legacy = LegacyJset {
            magic: JOURNAL_MAGIC,
            seq: 7,
            last_seq: 3,
            crc32: 0,
            entry_count: 1,
            version: 1,
            csum_type: CSUM_TYPE_CRC32C,
            entries: vec![LegacyJsetEntry {
                btree_type: 0,
                entry_type: JsetEntryType::BtreeKeys,
                btree_keys: payload.clone(),
                has_last: 1,
                has_prev: 0,
            }],
        };

        let data = bincode::serialize(&legacy).unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.version, JSET_VERSION);
        assert_eq!(restored.header.seq, 7);
        assert_eq!(restored.header.last_seq, 3);
        assert_eq!(restored.entries.len(), 1);
        assert_eq!(restored.entries[0].hdr.version, JSET_ENTRY_VERSION);
        assert_eq!(restored.entries[0].hdr.has_last, 1);
        assert_eq!(restored.entries[0].payload, payload);
        assert!(restored.verify());
    }

    // ─── CRC32C 向量测试 ─────────────────────────────────────

    /// Castagnoli CRC-32C 标准验证向量
    const CRC32C_CHECK_VALUE: u32 = 0xE3069283;

    #[test]
    fn test_crc32c_known_vector() {
        // CRC-32C 标准验证："123456789" -> 0xE3069283
        let data = b"123456789";
        assert_eq!(crc32c_sw(data, 0), CRC32C_CHECK_VALUE);
        assert_eq!(crc32c(data, 0), CRC32C_CHECK_VALUE);
        assert_eq!(Crc32CHasher::hash(data), CRC32C_CHECK_VALUE);
    }

    #[test]
    fn test_crc32c_empty() {
        assert_eq!(crc32c_sw(b"", 0), 0);
        assert_eq!(crc32c(b"", 0), 0);
    }

    #[test]
    fn test_crc32c_chaining() {
        // 分块计算应等于一次性计算
        let large = b"Hello, World! This is a test of CRC32C chaining across multiple blocks.";
        let full = crc32c_sw(large, 0);

        // 分两块
        let mid = large.len() / 2;
        let c1 = crc32c_sw(&large[..mid], 0);
        let c2 = crc32c_sw(&large[mid..], c1);
        assert_eq!(c2, full, "chained CRC must match single-pass");

        // 分三块
        let third = large.len() / 3;
        let c1 = crc32c_sw(&large[..third], 0);
        let c2 = crc32c_sw(&large[third..2 * third], c1);
        let c3 = crc32c_sw(&large[2 * third..], c2);
        assert_eq!(c3, full, "three-chunk CRC must match single-pass");
    }

    #[test]
    fn test_crc32c_hw_sw_consistent() {
        // 软件和硬件路径（如果 SSE4.2 可用）结果一致
        let data = b"Consistency test data for CRC32C hardware and software paths.";
        let sw = crc32c_sw(data, 0);

        #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
        {
            let hw = unsafe { super::crc32c_hw_impl(data, 0) };
            assert_eq!(hw, sw, "hardware CRC must match software CRC");
        }

        let dispatch = crc32c(data, 0);
        assert_eq!(dispatch, sw, "auto-dispatch CRC must match software CRC");
    }

    #[test]
    fn test_crc32c_nonzero_seed() {
        // 非零 seed 测试
        let seed = 0xDEADBEEFu32;
        let data = b"non-zero seed test";
        // 链式调用：先用 seed 作初始值计算
        let result = crc32c_sw(data, seed);
        // 重新计算验证
        let recheck = crc32c_sw(data, seed);
        assert_eq!(result, recheck, "CRC with same seed must be deterministic");
    }

    #[test]
    fn test_jset_serialize_verify_deserialize_multiple_entries() {
        // 多个 entry 的完整 roundtrip
        let payload1 = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1),
        )])
        .unwrap();
        let payload2 = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(2, 200, 0),
            KeyType::Normal,
            KeyValue::Raw(vec![10, 20]),
        )])
        .unwrap();

        let mut jset = Jset::new_volatile(42, 10);
        jset.header.csum_type = CSUM_TYPE_CRC32C;
        jset.entries
            .push(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload1).unwrap());
        jset.entries
            .push(RawJsetEntry::new(1, JsetEntryType::BtreeRoot as u8, payload2).unwrap());
        jset.header.entry_count = 2;

        let data = jset.serialize_padded().unwrap();
        assert_eq!(data.len() % JSET_BLOCK_SIZE as usize, 0);

        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.seq, 42);
        assert_eq!(restored.header.last_seq, 10);
        assert_eq!(restored.header.entry_count, 2);
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries[0].hdr.btree_type, 0);
        assert_eq!(
            restored.entries[0].hdr.entry_type,
            JsetEntryType::BtreeKeys as u8
        );
        assert_eq!(restored.entries[1].hdr.btree_type, 1);
        assert_eq!(
            restored.entries[1].hdr.entry_type,
            JsetEntryType::BtreeRoot as u8
        );

        assert!(restored.verify());
    }

    #[test]
    fn test_jset_volatile_magic() {
        let mut jset = Jset::new_volatile(1, 0);
        let payload = bincode::serialize(&vec![BtreeEntry::new(
            Bpos::new(1, 100, 0),
            KeyType::Normal,
            KeyValue::extent(0x1000, 1),
        )])
        .unwrap();
        jset.entries
            .push(RawJsetEntry::new(0, JsetEntryType::BtreeKeys as u8, payload).unwrap());
        jset.header.entry_count = 1;

        let data = jset.serialize_padded().unwrap();
        let restored = Jset::deserialize(&data).unwrap().unwrap();
        assert_eq!(restored.header.magic, VMNT_JSET_MAGIC);
        assert!(restored.verify());
    }
}
