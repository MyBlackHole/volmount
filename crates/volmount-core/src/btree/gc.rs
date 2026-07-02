//! B-tree GC (Garbage Collection / Consistency Check) — bcachefs 对齐
//!
//! 对应 bcachefs btree_gc.c + check.h 中的公开 API。
//! GC 子系统负责：
//! - Mark-and-sweep 回收：标记引用 → 回收未标记的 bucket
//! - 拓扑检查：验证 btree 节点之间的引用完整性
//! - 分配一致性检查：验证 alloc btree 与实际分配的匹配
//!
//! bcachefs 的 GC 是增量 / 并发的：使用 gc_pos 跟踪进度。
//! volmount 当前实现为最小骨架，提供 API 对齐的桩函数。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::alloc::btree::deserialize_alloc_entry;
use crate::alloc::bucket::BchDataType;
use crate::alloc::BchAllocEntry;
use crate::alloc::BchAllocator;
use crate::alloc::BLOCKS_PER_BUCKET;
use crate::btree::key::{Addr48, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
use crate::btree::node::BtreeNode;
use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
use crate::btree::{BtreeEngine, BtreeId, BTREE_ID_NR};
use crate::storage::superblock::BchSb;
use crate::types::StorageError;

// ─── GC Phase ───────────────────────────────────────────────────────────

/// bcachefs 对齐: enum gc_phase — GC 阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum GcPhase {
    #[default]
    NotRunning = 0,
    Start = 1,
    Sb = 2,
    Btree = 3,
}

/// bcachefs 对齐: struct gc_pos — GC 位置跟踪
///
/// journal_seq 记录完成此 GC pass 时的最新 journal seq。
/// recovery 时通过比较 gc_pos.journal_seq 与 journal last_seq 判断
/// 是否需要重新执行 gc_gens pass。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GcPos {
    pub phase: GcPhase,
    pub btree: u32,
    pub level: u16,
    pub pos: u64,
    /// 完成此 GC pass 时的 journal seq（用于 recovery 判断是否需要重做）
    pub journal_seq: u64,
}

// ─── GC State ───────────────────────────────────────────────────────────

/// GC 子系统状态
#[derive(Debug)]
pub struct BtreeGc {
    /// GC 是否正在运行
    pub running: AtomicBool,
    /// 当前 GC 位置
    pub pos: GcPos,
    /// GC 是否已被触发
    pub triggered: AtomicBool,
    /// GC 排他锁 — GC 运行时持有写锁，事务持有读锁
    pub lock: RwLock<()>,
}

impl Default for BtreeGc {
    fn default() -> Self {
        Self::new()
    }
}

impl BtreeGc {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            pos: GcPos {
                phase: GcPhase::NotRunning,
                btree: 0,
                level: 0,
                pos: 0,
                journal_seq: 0,
            },
            triggered: AtomicBool::new(false),
            lock: RwLock::new(()),
        }
    }
}

// ─── Public API ─────────────────────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_phase — 创建 GC phase 位置
pub fn gc_phase(phase: GcPhase) -> GcPos {
    GcPos {
        phase,
        btree: 0,
        level: 0,
        pos: 0,
        journal_seq: 0,
    }
}

/// bcachefs 对齐: gc_pos_btree — 创建 btree GC 位置
pub fn gc_pos_btree(btree: u32, level: u16, pos: u64) -> GcPos {
    GcPos {
        btree,
        level,
        pos,
        phase: GcPhase::Btree,
        journal_seq: 0,
    }
}

/// bcachefs 对齐: gc_pos_cmp — 比较两个 GC 位置
pub fn gc_pos_cmp(l: &GcPos, r: &GcPos) -> std::cmp::Ordering {
    l.phase
        .cmp(&r.phase)
        .then_with(|| l.btree.cmp(&r.btree))
        .then_with(|| l.level.cmp(&r.level))
        .then_with(|| l.pos.cmp(&r.pos))
}

/// bcachefs 对齐: gc_visited — 检查 GC 是否已访问过该位置
pub fn gc_visited(gc: &BtreeGc, pos: &GcPos) -> bool {
    gc_pos_cmp(pos, &gc.pos) == std::cmp::Ordering::Less
        || gc_pos_cmp(pos, &gc.pos) == std::cmp::Ordering::Equal
}

/// bcachefs 对齐: bch2_gc_gens — GC generation 传递
///
/// 遍历 Extents btree 中所有 extent 条目，收集被引用的 paddr，
/// 将对应 bucket 标记为 `BchDataType::User`。
///
/// `journal_seq` 参数记录当前 journal seq，用于 recovery 判断
/// 是否需要重新执行此 GC pass。gc_pos 的 journal_seq 按 btree 级别存储，
/// 供后续 recovery pass 查询。
///
/// 对应 bcachefs `bch2_gc_gens()` (gc.c)。
pub fn bch2_gc_gens(
    engine: &BtreeEngine,
    allocator: &mut BchAllocator,
    gc: &mut BtreeGc,
    journal_seq: u64,
) -> Result<(), StorageError> {
    // 收集 Extents btree 中所有被引用的 bucket
    // 注意：bch2 使用 `for_each_entry` 遍历，值的变体为 KeyValue::Raw（8 字节：
    // paddr 48-bit LE 在前 6 字节，ver 16-bit LE 在后 2 字节）。
    let extents_btree = engine.get(BtreeId::Extents);
    let mut referenced: HashSet<u64> = HashSet::new();

    extents_btree.for_each_entry(|entry| {
        if let KeyValue::Raw(bytes) = &entry.value {
            if bytes.len() >= 6 {
                let paddr = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0,
                ]);
                if paddr > 0 && paddr <= Addr48::MAX {
                    let bucket_idx = paddr / BLOCKS_PER_BUCKET;
                    referenced.insert(bucket_idx);
                }
            }
        }
    });

    // 将每个被引用的 bucket 标记为 User
    allocator.for_each_bucket_mut(|bucket_idx, bucket| {
        if referenced.contains(&bucket_idx) {
            bucket.mark_allocated();
        }
    });

    // 更新 gc_pos — 记录此 GC pass 完成时的 journal seq。
    // recovery 时通过比较 gc_pos.journal_seq 与 journal last_seq
    // 判断是否需要重新执行 gc_gens pass。
    gc.pos = GcPos {
        phase: GcPhase::Btree,
        btree: BtreeId::Extents as u32,
        level: 0,
        pos: 0,
        journal_seq,
    };

    Ok(())
}

/// bcachefs 对齐: bch2_gc_gens_async — 异步触发 GC generations
pub fn bch2_gc_gens_async(gc: &BtreeGc) {
    gc.triggered.store(true, Ordering::Release);
}

/// bcachefs 对齐: bch2_check_topology — 检查 btree 拓扑完整性（P0-5）
///
/// 验证：
/// - 每个 btree 中的条目按 Bpos 排序（原有）
/// - 无重复位置条目（原有）
/// - 多级树中每个内部节点都能递归找到 child，且 child level 连续
/// - 相邻 child 的 key span 不重叠，且按 key 顺序连续
pub fn bch2_check_topology(engine: &BtreeEngine) -> Result<(), StorageError> {
    for ty in BTREE_ID_NR {
        let btree = engine.get(ty);

        let mut visited_children = HashSet::new();
        validate_tree_node(
            ty,
            btree.root().node.as_ref(),
            btree.cache(),
            &mut visited_children,
        )?;

        let mut entries: Vec<Bpos> = Vec::new();

        btree.for_each_entry(|entry| {
            if entry.key_type != KeyType::Deleted {
                entries.push(entry.pos);
            }
        });

        // 检查排序顺序
        for i in 1..entries.len() {
            if entries[i] < entries[i - 1] {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} entries out of order at index {}",
                    ty, i,
                )));
            }
        }

        // 检查重复位置
        let mut seen = HashSet::new();
        for pos in &entries {
            if !seen.insert(*pos) {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} duplicate entry at {:?}",
                    ty, pos,
                )));
            }
        }
    }

    Ok(())
}

fn validate_tree_node(
    ty: BtreeId,
    node: &BtreeNode,
    cache: &crate::btree::types::NodeCache,
    visited_children: &mut HashSet<u64>,
) -> Result<(), StorageError> {
    let mut entries: Vec<(BtreeKey, crate::btree::key::BchVal)> = Vec::new();
    for set in &node.sets {
        for idx in 1..=set.size as usize {
            entries.push(node.read_entry(set, idx));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    if node.level == 0 {
        if entries.is_empty() {
            return Ok(());
        }

        let first = Bpos::from_key(&entries[0].0);
        let last = Bpos::from_key(&entries[entries.len() - 1].0);
        if node.min_key != Bpos::MAX && node.min_key > first {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} leaf min_key {:?} > first entry {:?}",
                ty, node.min_key, first,
            )));
        }
        if node.max_key != Bpos::MIN && node.max_key < last {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} leaf max_key {:?} < last entry {:?}",
                ty, node.max_key, last,
            )));
        }
        return Ok(());
    }

    if entries.is_empty() {
        return Err(StorageError::Transaction(format!(
            "check_topology: btree {:?} empty interior node at level {}",
            ty, node.level,
        )));
    }

    let mut prev_child_max: Option<Bpos> = None;
    for (_idx, (key, value)) in entries.iter().enumerate() {
        let child_addr = value.paddr.get();
        if child_addr == 0 {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} interior entry {:?} has null child pointer",
                ty, key,
            )));
        }

        if !visited_children.insert(child_addr) {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} child node {} visited twice",
                ty, child_addr,
            )));
        }

        let child = cache.get(child_addr).ok_or_else(|| {
            StorageError::Transaction(format!(
                "check_topology: btree {:?} missing child node {} at {:?}",
                ty, child_addr, key,
            ))
        })?;

        if child.level != node.level.saturating_sub(1) {
            return Err(StorageError::Transaction(format!(
                "check_topology: btree {:?} cached child level mismatch at {:?}: child.level={} parent.level={}",
                ty,
                key,
                child.level,
                node.level,
            )));
        }

        if let Some(prev_max) = prev_child_max {
            if child.min_key <= prev_max {
                return Err(StorageError::Transaction(format!(
                    "check_topology: btree {:?} child boundary overlap at {:?}: prev_max {:?}, child_min {:?}",
                    ty,
                    key,
                    prev_max,
                    child.min_key,
                )));
            }
        }

        validate_tree_node(ty, child.as_ref(), cache, visited_children)?;
        prev_child_max = Some(child.max_key);
    }

    Ok(())
}

/// bcachefs 对齐: bch2_check_allocations — 检查分配一致性
///
/// 对比 extent 引用的 bucket 与 allocator 中实际分配状态的差异。
/// 返回不一致的描述列表；空 Vec 表示一致。
pub fn bch2_check_allocations(
    engine: &BtreeEngine,
    allocator: &BchAllocator,
) -> Result<Vec<String>, StorageError> {
    // 收集所有 btree 中 extent 引用的 paddr → bucket_index
    let mut referenced: HashSet<u64> = HashSet::new();

    for ty in BTREE_ID_NR {
        let btree = engine.get(ty);
        btree.for_each_entry(|entry| {
            // 遍历所有 btree 的 entry，从 KeyValue::Raw 字节中提取 paddr
            // 前 6 字节为 paddr（48-bit LE），后 2 字节为 ver
            if let KeyValue::Raw(bytes) = &entry.value {
                if bytes.len() >= 6 {
                    let paddr = u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0,
                    ]);
                    if paddr > 0 && paddr <= Addr48::MAX {
                        let bucket_idx = paddr / BLOCKS_PER_BUCKET;
                        referenced.insert(bucket_idx);
                    }
                }
            }
        });
    }

    // 收集 allocator 中已分配（非 Free/NeedDiscard）的 bucket
    let mut allocated: Vec<(u64, BchDataType)> = Vec::new();
    allocator.for_each_bucket(|bucket_idx, bucket| {
        if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
            allocated.push((bucket_idx, bucket.state));
        }
    });

    // 找出已分配但未被任何 extent 引用的 bucket（潜在泄漏）
    let mut discrepancies = Vec::new();
    for &(bi, state) in &allocated {
        if !referenced.contains(&bi) {
            discrepancies.push(format!(
                "bucket {} allocated ({:?}) but not referenced by any extent",
                bi, state,
            ));
        }
    }

    Ok(discrepancies)
}

/// bcachefs 对齐: bch2_check_alloc_info
///
/// 校验 alloc / freespace / allocator 三者的一致性：
/// - Alloc btree 记录必须与 allocator 当前 bucket 状态一致
/// - Free bucket 必须有对应的 freespace key
/// - Allocated bucket 不得残留任何 freespace key
pub fn bch2_check_alloc_info(
    engine: &BtreeEngine,
    allocator: &BchAllocator,
) -> Result<Vec<String>, StorageError> {
    let mut discrepancies = Vec::new();

    let mut allocator_snapshot: HashMap<u64, BchAllocEntry> = HashMap::new();
    allocator.for_each_bucket(|bucket_idx, bucket| {
        allocator_snapshot.insert(
            bucket_idx,
            BchAllocEntry {
                journal_seq: bucket.journal_seq,
                dirty_sectors: bucket.dirty_sectors,
                cached_sectors: bucket.cached_sectors,
                stripe: bucket.stripe as u16,
                state: bucket.state,
                version: bucket.version,
                io_time_read: 0,
                nr_external_backpointers: 0,
                group: bucket.group,
            },
        );
    });

    let mut alloc_entries: HashMap<u64, BchAllocEntry> = HashMap::new();
    let alloc_btree = engine.get(BtreeId::Alloc);
    alloc_btree.for_each_entry(|entry| {
        if entry.key_type == KeyType::Normal {
            if let KeyValue::Raw(bytes) = &entry.value {
                if let Ok(alloc_data) = deserialize_alloc_entry(bytes) {
                    alloc_entries.insert(entry.pos.offset, alloc_data);
                } else {
                    discrepancies.push(format!(
                        "alloc key {} failed to deserialize",
                        entry.pos.offset
                    ));
                }
            }
        }
    });

    let mut freespace_entries: HashMap<u64, Vec<u32>> = HashMap::new();
    let freespace_btree = engine.get(BtreeId::Freespace);
    freespace_btree.for_each_entry(|entry| {
        if entry.key_type == KeyType::Normal {
            freespace_entries
                .entry(entry.pos.offset)
                .or_default()
                .push(entry.pos.snapshot);
        }
    });

    for (bucket_idx, bucket) in &allocator_snapshot {
        match alloc_entries.get(bucket_idx) {
            Some(alloc_entry) => {
                if alloc_entry != bucket {
                    discrepancies.push(format!(
                        "alloc bucket {} mismatch: alloc={:?} allocator={:?}",
                        bucket_idx, alloc_entry, bucket
                    ));
                }
            }
            None => discrepancies.push(format!("missing alloc entry for bucket {}", bucket_idx)),
        }

        let freespaces = freespace_entries.get(bucket_idx);
        match bucket.state {
            BchDataType::Free => {
                if !matches!(freespaces, Some(gens) if gens.contains(&bucket.version)) {
                    discrepancies.push(format!(
                        "missing freespace entry for free bucket {} gen {}",
                        bucket_idx, bucket.version
                    ));
                }
                if let Some(gens) = freespaces {
                    for gen in gens {
                        if *gen != bucket.version {
                            discrepancies.push(format!(
                                "stale freespace entry for bucket {} gen {} (expected {})",
                                bucket_idx, gen, bucket.version
                            ));
                        }
                    }
                }
            }
            _ => {
                if let Some(gens) = freespaces {
                    for gen in gens {
                        discrepancies.push(format!(
                            "stale freespace entry for allocated bucket {} gen {}",
                            bucket_idx, gen
                        ));
                    }
                }
            }
        }
    }

    for (bucket_idx, alloc_entry) in alloc_entries {
        if !allocator_snapshot.contains_key(&bucket_idx) {
            discrepancies.push(format!(
                "alloc entry {} references missing bucket {:?}",
                bucket_idx, alloc_entry
            ));
        }
    }

    Ok(discrepancies)
}

/// bcachefs 对齐: bch2_fs_btree_gc_init_early — GC 子系统早期初始化
pub fn bch2_fs_btree_gc_init_early(gc: &BtreeGc) {
    gc.running.store(false, Ordering::Release);
    gc.triggered.store(false, Ordering::Release);
}

/// bcachefs 对齐: bch2_gc_pos_to_text — 将 GC 位置格式化为文本
pub fn bch2_gc_pos_to_text(pos: &GcPos) -> String {
    format!(
        "GC phase={:?} btree={} level={} pos={}",
        pos.phase, pos.btree, pos.level, pos.pos
    )
}

/// bcachefs 对齐: bch2_merge_btree_nodes — 合并相邻 btree 节点
pub fn bch2_merge_btree_nodes() -> i32 {
    0
}

/// bcachefs 对齐: bch2_presplit_shard_boundaries — 预分裂分片边界
///
/// 遍历所有 btree type，对每个 depth=0 的 btree 检查其 entries 是否跨越
/// SHARD_FACTOR（1024）分片边界。如果跨越则将 root leaf 节点分裂为两个，
/// 创建深度为 1 的多级树，使后续写入能按 shard 分散到不同子树。
///
/// 仅在 recovery 过程的 presplit_shard_boundaries pass 中调用。
pub fn bch2_presplit_shard_boundaries(engine: &mut BtreeEngine) -> Result<(), StorageError> {
    for ty in BTREE_ID_NR {
        let btree = engine.get_mut(ty);
        btree.presplit_shard_boundaries();
    }
    Ok(())
}

/// bcachefs 对齐: bch2_gc_accounting_start — 开始 GC 会计阶段
pub fn bch2_gc_accounting_start() -> i32 {
    0
}

/// bcachefs 对齐: bch2_gc_accounting_done — 完成 GC 会计阶段
pub fn bch2_gc_accounting_done() -> i32 {
    0
}

// ─── G1: Mark-and-Sweep 核心 ──────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_mark_key — 标记一个 btree entry 引用的 bucket（P0-5 增强）
///
/// 从 entry 的 value 中提取 paddr，在 allocator 中将对应的 bucket 标记为 User。
/// 只有 paddr 合法（> 0 且 ≤ Addr48::MAX）时才进行标记。
///
/// P0-5 增强：当提供了有效的 engine 引用时，在标记前确保拓扑检查已执行。
/// 拓扑检查由调用者（bch2_gc_btrees）统一调度，此函数不做重复检查。
pub fn bch2_gc_mark_key(
    _engine: &BtreeEngine,
    allocator: &mut BchAllocator,
    entry: &BtreeEntry,
) -> Result<(), StorageError> {
    let paddr = match &entry.value {
        KeyValue::Extent(v) => v.paddr.get(),
        KeyValue::BtreePtr(ptr) => ptr.block_addr,
        KeyValue::Raw(bytes) => {
            if bytes.len() >= 6 {
                u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0,
                ])
            } else {
                return Ok(());
            }
        }
    };

    if paddr == 0 || paddr > Addr48::MAX {
        return Ok(());
    }

    let bucket_idx = paddr / BLOCKS_PER_BUCKET;
    allocator.for_each_bucket_mut(|bi, bucket| {
        if bi == bucket_idx {
            bucket.state = BchDataType::User;
            bucket.version = bucket.version.wrapping_add(1);
        }
    });

    Ok(())
}

/// 触发 GC 阶段的 key trigger（P0-6）
///
/// 对应 bcachefs `bch2_gc_mark_key` 中构造 `btree_trigger_op` 并调用
/// `bch2_key_trigger` 的逻辑。使用 `TriggerPhase::Gc` 作为执行阶段。
///
/// 参数：
/// - `key_bytes`: bincode 序列化的 BtreeKey
/// - `value_bytes`: bincode 序列化的值（新值）
fn fire_gc_key_trigger(
    engine: &mut BtreeEngine,
    registry: &TriggerRegistry,
    ty: BtreeId,
    key_type: u8,
    key_bytes: &[u8],
    value_bytes: &[u8],
) -> Result<(), StorageError> {
    // bcachefs: BTREE_TRIGGER_gc | BTREE_TRIGGER_insert
    // volmount: TriggerPhase::Gc 阶段对应
    registry.fire(
        engine,
        ty,
        key_type,
        TriggerPhase::Gc,
        key_bytes,
        None,              // old_val = None（GC 标记无前值）
        Some(value_bytes), // new_val = 当前值
    )
}

/// bcachefs 对齐: bch2_gc_btrees — 全树标记遍历（P0-5 + P0-6 增强）
///
/// 遍历所有 BtreeId 类型的每个 entry：
/// 1. P0-5: 先对每个 btree 执行拓扑检查（bch2_check_topology）
/// 2. 对非 Deleted entry 调用 bch2_gc_mark_key 标记 bucket
/// 3. P0-6: 对每个非 Deleted entry 触发 GC 阶段的 key trigger（若 registry 已提供）
///
/// 拓扑检查确保在 split/merge 后节点链接一致。
/// 触发 key trigger 确保 bucket 引用计数经 GC 正确更新。
///
/// trigger_registry 参数为可选：在未注册触发器的简单模式下可传 None。
pub fn bch2_gc_btrees(
    engine: &mut BtreeEngine,
    allocator: &mut BchAllocator,
    trigger_registry: Option<&TriggerRegistry>,
) -> Result<(), StorageError> {
    // P0-5: 首次标记前执行全量拓扑检查，确保树结构完整
    bch2_check_topology(engine)?;

    for ty in BTREE_ID_NR {
        // 收集非 Deleted 条目（与 engine 的借用分离以便后续触发 trigger）
        let entries: Vec<BtreeEntry> = {
            let btree = engine.get(ty);
            let mut collected = Vec::new();
            btree.for_each_entry(|entry| {
                if entry.key_type != KeyType::Deleted {
                    collected.push(entry);
                }
            });
            collected
        };

        for entry in &entries {
            // P0-5: 调用 GC mark key 标记 bucket
            bch2_gc_mark_key(engine, allocator, entry)?;

            // P0-6: 若注册了 trigger registry，触发 GC 阶段 key trigger
            if let Some(registry) = trigger_registry {
                let key = BtreeKey::from_bpos(entry.pos, entry.key_type);
                let key_bytes = bincode::serialize(&key).unwrap_or_default();
                let value_bytes = entry.value.to_bytes();

                fire_gc_key_trigger(
                    engine,
                    registry,
                    ty,
                    entry.key_type as u8,
                    &key_bytes,
                    &value_bytes,
                )?;
            }
        }
    }
    Ok(())
}

// ─── Sweep Phase ────────────────────────────────────────────────────

/// GC sweep 回收统计 — 记录 sweep phase 回收的 bucket 信息
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReclaimStats {
    /// 回收为 Free 的 bucket 数量
    pub reclaimed_count: u32,
    /// 回收的 bucket 索引列表（便于测试验证）
    pub reclaimed_buckets: Vec<u64>,
    /// 因非 User/NeedGcGens 状态跳过的 bucket 数量
    pub skipped_state: u32,
}

// ─── G2: Allocator Snapshot ───────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_alloc_start — 快照分配器状态
///
/// 将当前 allocator 中所有非 Free/NeedDiscard 的 bucket 状态快照到
/// HashMap，供 GC 完成后对比以检测不一致。
pub fn bch2_gc_alloc_start(allocator: &BchAllocator) -> HashMap<u64, BchDataType> {
    let mut snapshot = HashMap::new();
    allocator.for_each_bucket(|bi, bucket| {
        if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
            snapshot.insert(bi, bucket.state);
        }
    });
    snapshot
}

/// bcachefs 对齐: bch2_gc_alloc_done — 对比分配器快照
///
/// 比较当前 allocator 状态与 GC 前的快照，返回变化描述列表。
/// 包括：新分配的 bucket、释放的 bucket、类型变更的 bucket。
/// 空 Vec 表示一致。
pub fn bch2_gc_alloc_done(
    _engine: &BtreeEngine,
    allocator: &mut BchAllocator,
    snapshot: HashMap<u64, BchDataType>,
) -> Result<Vec<String>, StorageError> {
    let mut current = HashMap::new();
    allocator.for_each_bucket(|bi, bucket| {
        if bucket.state != BchDataType::Free && bucket.state != BchDataType::NeedDiscard {
            current.insert(bi, bucket.state);
        }
    });

    let mut changes = Vec::new();

    // 之前分配了而现在空闲的 bucket（潜在泄漏）
    for (bi, old_state) in &snapshot {
        if !current.contains_key(bi) {
            changes.push(format!(
                "bucket {} was {:?} but is now free/unreferenced",
                bi, old_state,
            ));
        }
    }

    // 新分配的 bucket
    for (bi, new_state) in &current {
        if !snapshot.contains_key(bi) {
            changes.push(format!(
                "bucket {} is now {:?} but was not previously allocated",
                bi, new_state,
            ));
        }
    }

    // 类型变更的 bucket
    for (bi, old_state) in &snapshot {
        if let Some(new_state) = current.get(bi) {
            if old_state != new_state {
                changes.push(format!(
                    "bucket {} changed from {:?} to {:?}",
                    bi, old_state, new_state,
                ));
            }
        }
    }

    Ok(changes)
}

// ─── G3: Sweep Phase ─────────────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_alloc_done — GC Sweep phase
///
/// 遍历所有 bucket，将状态为 User 但未被任何 extent 引用的 bucket 回收为 Free。
/// 对应 bcachefs `bch2_gc_alloc_done` + `bch2_alloc_write_key` 的修正逻辑。
///
/// 回收规则（DD-2）:
/// - state == User && !referenced → mark_free()（回收）
/// - state == NeedGcGens → mark_free()（transient 状态清理）
/// - 其他状态（Sb/Journal/Btree/Cached/Parity/Stripe/NeedDiscard/Reserved/...）→ 不动
///
/// 安全性（DD-2）: sweep 在回收前重新收集所有 extent 引用的 bucket（double-check），
/// 只回收确证未被引用的 bucket。保留 Sb/Journal/Btree 等非 User 状态不动。
pub fn bch2_gc_sweep(
    engine: &BtreeEngine,
    allocator: &mut BchAllocator,
) -> Result<ReclaimStats, StorageError> {
    // 1. 收集所有 btree 中 extent 引用的 bucket（ground truth）
    let mut referenced: HashSet<u64> = HashSet::new();
    for ty in BTREE_ID_NR {
        let btree = engine.get(ty);
        btree.for_each_entry(|entry| {
            if let KeyValue::Raw(bytes) = &entry.value {
                if bytes.len() >= 6 {
                    let paddr = u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], 0, 0,
                    ]);
                    if paddr > 0 && paddr <= Addr48::MAX {
                        let bucket_idx = paddr / BLOCKS_PER_BUCKET;
                        referenced.insert(bucket_idx);
                    }
                }
            }
        });
    }

    // 2. 遍历所有 bucket，回收未引用的 User/NeedGcGens bucket
    let mut stats = ReclaimStats::default();
    allocator.for_each_bucket_mut(|bi, bucket| match bucket.state {
        BchDataType::User => {
            if !referenced.contains(&bi) {
                bucket.mark_free();
                stats.reclaimed_count += 1;
                stats.reclaimed_buckets.push(bi);
            }
        }
        BchDataType::NeedGcGens => {
            bucket.mark_free();
            stats.reclaimed_count += 1;
            stats.reclaimed_buckets.push(bi);
        }
        _ => {
            stats.skipped_state += 1;
        }
    });

    Ok(stats)
}

// ─── G7: Superblock helpers ───────────────────────────────────────────

/// bcachefs 对齐: bch2_gc_pos_to_sb — 将 GC 位置写入 superblock
pub fn bch2_gc_pos_to_sb(gc: &BtreeGc, sb: &mut BchSb) {
    sb.gc_pos = gc.pos;
    sb.gc_pos_valid = true;
}

/// bcachefs 对齐: bch2_gc_pos_from_sb — 从 superblock 读取 GC 位置
///
/// 如果 superblock 中记录了有效的 gc_pos，返回 Some；否则返回 None。
pub fn bch2_gc_pos_from_sb(sb: &BchSb) -> Option<GcPos> {
    if sb.gc_pos_valid {
        Some(sb.gc_pos)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_gc_phase_order() {
        assert!(GcPhase::NotRunning < GcPhase::Start);
        assert!(GcPhase::Start < GcPhase::Sb);
        assert!(GcPhase::Sb < GcPhase::Btree);
    }

    #[test]
    fn test_gc_pos_cmp() {
        let a = gc_phase(GcPhase::Start);
        let b = gc_phase(GcPhase::Btree);
        assert_eq!(gc_pos_cmp(&a, &b), std::cmp::Ordering::Less);
    }

    #[test]
    fn test_gc_visited() {
        let gc = BtreeGc::new();
        // 初始状态 gc.pos = NotRunning, pos 为 0
        let pos = gc_phase(GcPhase::Start);
        assert!(
            !gc_visited(&gc, &pos),
            "gc should not have visited Start yet"
        );
    }

    #[test]
    fn test_gc_default() {
        let gc = BtreeGc::default();
        assert_eq!(gc.pos.phase, GcPhase::NotRunning);
        assert!(!gc.running.load(Ordering::Acquire));
    }

    #[test]
    fn test_gc_trigger() {
        let gc = BtreeGc::new();
        bch2_gc_gens_async(&gc);
        assert!(gc.triggered.load(Ordering::Acquire));
    }

    // ─── P0-3: bch2_gc_gens tests ───────────────────────────────

    #[test]
    fn test_gc_gens_basic() {
        let mut gc = BtreeGc::new();
        let mut engine = crate::btree::BtreeEngine::new();
        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // 插入一个 extent 条目，paddr=0x1000 → bucket_idx=16
        let paddr = 0x1000u64;
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 1, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(paddr, 1),
            ),
            0,
        );

        let result = bch2_gc_gens(&engine, &mut allocator, &mut gc, 1);
        assert!(result.is_ok(), "bch2_gc_gens should succeed");

        // 验证 paddr 对应的 bucket 已被标记为 User
        let bucket_idx = paddr / crate::alloc::BLOCKS_PER_BUCKET;
        let mut found = false;
        allocator.for_each_bucket(|bi, bucket| {
            if bi == bucket_idx {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::User,
                    "bucket {} should be marked User after gc_gens",
                    bi,
                );
                found = true;
            }
        });
        assert!(found, "bucket {} should exist in allocator", bucket_idx);
    }

    // ─── P0-4: bch2_check_topology tests ─────────────────────────

    #[test]
    fn test_check_topology_basic() {
        let mut engine = crate::btree::BtreeEngine::new();

        // 插入一些有序条目
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 10, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x100, 1),
            ),
            0,
        );
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 20, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x200, 1),
            ),
            0,
        );

        let result = bch2_check_topology(&engine);
        assert!(
            result.is_ok(),
            "topology check on consistent btree should pass"
        );
    }

    #[test]
    fn test_check_topology_empty_btree() {
        let engine = crate::btree::BtreeEngine::new();
        let result = bch2_check_topology(&engine);
        assert!(result.is_ok(), "topology check on empty btree should pass");
    }

    fn make_two_level_tree() -> crate::btree::Btree {
        use crate::btree::node::BsetTree;
        use crate::btree::types::{BtreeRoot, NodeCache};
        use crate::btree::{Btree, BtreeNode};
        use std::sync::Arc;

        let cache = Arc::new(NodeCache::new());

        let mut left = BtreeNode::new_leaf();
        left.insert(
            crate::btree::key::BtreeKey::new(10, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(100, 0),
        );
        left.insert(
            crate::btree::key::BtreeKey::new(20, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(200, 0),
        );
        left.insert(
            crate::btree::key::BtreeKey::new(30, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(300, 0),
        );
        let left = Arc::new(left);

        let mut right = BtreeNode::new_leaf();
        right.insert(
            crate::btree::key::BtreeKey::new(40, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(400, 0),
        );
        right.insert(
            crate::btree::key::BtreeKey::new(50, 1, crate::btree::key::KeyType::Normal),
            crate::btree::key::BchVal::new(500, 0),
        );
        let right = Arc::new(right);

        let left_addr = cache.alloc_addr();
        let right_addr = cache.alloc_addr();
        cache.insert(left_addr, left);
        cache.insert(right_addr, right);

        let mut internal = BtreeNode::new_internal();
        let mut cur = 0u32;
        cur += internal.write_entry(
            cur,
            &crate::btree::key::BtreeKey::MIN_KEY,
            &crate::btree::key::BchVal::new(left_addr, 0),
        );
        cur += internal.write_entry(
            cur,
            &crate::btree::key::BtreeKey::new(40, 1, crate::btree::key::KeyType::Normal),
            &crate::btree::key::BchVal::new(right_addr, 0),
        );
        internal.sets[0] = BsetTree {
            data_offset: 0,
            end_offset: cur,
            aux_offset: 0,
            size: 2,
            extra: 0,
        };
        internal.key_count = 2;

        Btree::from_root(BtreeRoot::new(Arc::new(internal), 1), cache)
    }

    fn child_addrs(tree: &crate::btree::Btree) -> (u64, u64) {
        let root = &tree.root().node;
        let set = &root.sets[0];
        let (_, left_val) = root.read_entry(set, 1);
        let (_, right_val) = root.read_entry(set, 2);
        (left_val.paddr.get(), right_val.paddr.get())
    }

    // ─── P0-4: bch2_check_allocations tests ──────────────────────

    #[test]
    fn test_check_allocations_basic() {
        let engine = crate::btree::BtreeEngine::new();
        let allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // 无 extent、无分配，应返回空 Vec（一致）
        let result = bch2_check_allocations(&engine, &allocator);
        assert!(result.is_ok(), "allocations check should succeed");
        let discrepancies = result.unwrap();
        assert!(
            discrepancies.is_empty(),
            "expected no discrepancies, got: {:?}",
            discrepancies,
        );
    }

    // ─── P0-5: 拓扑检查增强 ─────────────────────────────────

    #[test]
    fn test_check_topology_with_root_range() {
        let mut engine = crate::btree::BtreeEngine::new();
        // 插入有序条目
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 10, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x100, 1),
            ),
            0,
        );
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 20, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x200, 1),
            ),
            0,
        );
        // 设置 root min_key/max_key 与实际条目一致（正常情况）
        let result = bch2_check_topology(&engine);
        assert!(
            result.is_ok(),
            "topology check should pass with correct root range"
        );
    }

    #[test]
    fn test_check_topology_detects_out_of_order() {
        let mut engine = crate::btree::BtreeEngine::new();
        // 插入无序条目
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 20, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x200, 1),
            ),
            0,
        );
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 10, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x100, 1),
            ),
            0,
        );
        let result = bch2_check_topology(&engine);
        assert!(
            result.is_err(),
            "topology check should detect out-of-order entries"
        );
    }

    #[test]
    fn test_check_topology_recursive_tree() {
        let btree = make_two_level_tree();
        let mut engine = crate::btree::BtreeEngine::new();
        *engine.get_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&engine);
        assert!(result.is_ok(), "recursive topology check should pass");
    }

    #[test]
    fn test_check_topology_detects_child_boundary_overlap() {
        let btree = make_two_level_tree();
        let (left_addr, right_addr) = child_addrs(&btree);
        let cache = btree.cache();

        let mut right = cache
            .take_node(right_addr)
            .expect("right child should exist in cache");
        {
            let right_node = Arc::get_mut(&mut right).expect("right child Arc should be unique");
            right_node.min_key = crate::btree::key::Bpos::new(25, 1, 0);
        }
        cache.insert(right_addr, right);

        let mut engine = crate::btree::BtreeEngine::new();
        *engine.get_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&engine);
        assert!(
            result.is_err(),
            "overlapping child boundary should fail topology check"
        );

        // left child remains untouched; ensure helper extracted a real root tree.
        assert_ne!(left_addr, right_addr);
    }

    #[test]
    fn test_check_topology_detects_missing_child() {
        let btree = make_two_level_tree();
        let (_, right_addr) = child_addrs(&btree);
        let cache = btree.cache();
        let _ = cache
            .take_node(right_addr)
            .expect("right child should exist");

        let mut engine = crate::btree::BtreeEngine::new();
        *engine.get_mut(crate::btree::BtreeId::Extents) = btree;

        let result = bch2_check_topology(&engine);
        assert!(
            result.is_err(),
            "missing child node should fail topology check"
        );
    }

    // ─── P0-6: bch2_gc_btrees 触发增强 ─────────────────────

    #[test]
    fn test_gc_btrees_with_registry_none() {
        // 验证 bch2_gc_btrees 在 trigger_registry=None 时的向后兼容行为
        let mut engine = crate::btree::BtreeEngine::new();
        let paddr = 0x2000u64;
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 1, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(paddr, 1),
            ),
            0,
        );

        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);
        let result = bch2_gc_btrees(&mut engine, &mut allocator, None);
        assert!(
            result.is_ok(),
            "bch2_gc_btrees with None registry should succeed"
        );

        // 验证 bucket 被正确标记
        let bucket_idx = paddr / crate::alloc::BLOCKS_PER_BUCKET;
        let mut found = false;
        allocator.for_each_bucket(|bi, bucket| {
            if bi == bucket_idx {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::User,
                    "bucket {} should be marked User after gc_btrees",
                    bi,
                );
                found = true;
            }
        });
        assert!(found, "bucket {} should exist", bucket_idx);
    }

    #[test]
    fn test_gc_btrees_collects_entries() {
        // 验证 bch2_gc_btrees 能正确处理多个 btree 中的条目
        let mut engine = crate::btree::BtreeEngine::new();

        // 在 Extents btree 中插入
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 1, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x3000, 1),
            ),
            0,
        );
        // 在 Alloc btree 中插入
        engine.insert_entry_raw(
            crate::btree::BtreeId::Alloc,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 5, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(0x4000, 1),
            ),
            0,
        );

        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);
        let result = bch2_gc_btrees(&mut engine, &mut allocator, None);
        assert!(
            result.is_ok(),
            "bch2_gc_btrees with multiple btrees should succeed"
        );

        // 验证 Extents btree 中的 bucket 被标记
        let bucket_idx_ext = 0x3000 / crate::alloc::BLOCKS_PER_BUCKET;
        let bucket_idx_alloc = 0x4000 / crate::alloc::BLOCKS_PER_BUCKET;
        let mut ext_found = false;
        let mut alloc_found = false;
        allocator.for_each_bucket(|bi, bucket| {
            if bi == bucket_idx_ext {
                assert_eq!(bucket.state, crate::alloc::BchDataType::User);
                ext_found = true;
            }
            if bi == bucket_idx_alloc {
                assert_eq!(bucket.state, crate::alloc::BchDataType::User);
                alloc_found = true;
            }
        });
        assert!(
            ext_found,
            "Extents bucket {} should be marked",
            bucket_idx_ext
        );
        assert!(
            alloc_found,
            "Alloc bucket {} should be marked",
            bucket_idx_alloc
        );
    }

    // ─── Sweep Phase tests ──────────────────────────────────────

    #[test]
    fn test_gc_sweep_reclaims_unreferenced_user_bucket() {
        let mut engine = crate::btree::BtreeEngine::new();
        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // Insert an extent that references bucket at paddr=0x5000
        let paddr = 0x5000u64;
        engine.insert_entry_raw(
            crate::btree::BtreeId::Extents,
            crate::btree::key::BtreeEntry::new(
                crate::btree::key::Bpos::new(0, 1, 0),
                crate::btree::key::KeyType::Normal,
                crate::btree::key::KeyValue::extent(paddr, 1),
            ),
            0,
        );

        let bucket_ref = paddr / crate::alloc::BLOCKS_PER_BUCKET; // = 80
        let bucket_unref = 30u64;

        // Manually mark both buckets as User (simulating mark phase)
        allocator.for_each_bucket_mut(|bi, bucket| {
            if bi == bucket_ref || bi == bucket_unref {
                bucket.state = crate::alloc::BchDataType::User;
                bucket.version = bucket.version.wrapping_add(1);
            }
        });

        let stats = bch2_gc_sweep(&engine, &mut allocator).unwrap();

        // Referenced bucket should remain User
        allocator.for_each_bucket(|bi, bucket| {
            if bi == bucket_ref {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::User,
                    "referenced bucket should remain User",
                );
            }
            if bi == bucket_unref {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::Free,
                    "unreferenced bucket should be reclaimed to Free",
                );
            }
        });

        assert_eq!(stats.reclaimed_count, 1, "should reclaim 1 bucket");
        assert_eq!(
            stats.reclaimed_buckets,
            vec![bucket_unref],
            "should reclaim the unreferenced bucket",
        );
    }

    #[test]
    fn test_gc_sweep_preserves_sb_journal_btree() {
        let engine = crate::btree::BtreeEngine::new();
        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // Manually set some buckets to non-reclaimable states
        allocator.for_each_bucket_mut(|bi, bucket| match bi {
            0 => bucket.state = crate::alloc::BchDataType::Sb,
            1 => bucket.state = crate::alloc::BchDataType::Journal,
            2 => bucket.state = crate::alloc::BchDataType::Btree,
            _ => {}
        });

        let stats = bch2_gc_sweep(&engine, &mut allocator).unwrap();

        // Verify non-reclaimable states are preserved
        let mut sb_found = false;
        let mut journal_found = false;
        let mut btree_found = false;
        allocator.for_each_bucket(|bi, bucket| match bi {
            0 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Sb);
                sb_found = true;
            }
            1 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Journal);
                journal_found = true;
            }
            2 => {
                assert_eq!(bucket.state, crate::alloc::BchDataType::Btree);
                btree_found = true;
            }
            _ => {}
        });
        assert!(sb_found, "bucket 0 should exist");
        assert!(journal_found, "bucket 1 should exist");
        assert!(btree_found, "bucket 2 should exist");

        assert!(
            stats.skipped_state > 0,
            "should have skipped non-User/non-NeedGcGens buckets",
        );
    }

    #[test]
    fn test_gc_sweep_cleans_needgcgens_transient() {
        let engine = crate::btree::BtreeEngine::new();
        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // Manually set some buckets to NeedGcGens
        allocator.for_each_bucket_mut(|bi, bucket| {
            if bi == 5 || bi == 10 {
                bucket.state = crate::alloc::BchDataType::NeedGcGens;
            }
        });

        let stats = bch2_gc_sweep(&engine, &mut allocator).unwrap();

        // NeedGcGens buckets should be reclaimed to Free
        allocator.for_each_bucket(|bi, bucket| {
            if bi == 5 || bi == 10 {
                assert_eq!(
                    bucket.state,
                    crate::alloc::BchDataType::Free,
                    "NeedGcGens bucket {} should be reclaimed to Free",
                    bi,
                );
            }
        });

        assert_eq!(
            stats.reclaimed_count, 2,
            "should reclaim 2 NeedGcGens buckets",
        );
    }

    #[test]
    fn test_gc_sweep_empty_engine_no_reclaim() {
        let engine = crate::btree::BtreeEngine::new();
        let mut allocator = crate::alloc::BchAllocator::new(1024 * 256, 1024 * 256, 0);

        // All buckets are Free by default; no User buckets to reclaim
        let stats = bch2_gc_sweep(&engine, &mut allocator).unwrap();

        assert_eq!(
            stats.reclaimed_count, 0,
            "no User buckets should be reclaimed from empty engine",
        );
    }
}
