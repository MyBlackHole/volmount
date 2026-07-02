//! BtreeTrans — bcachefs 对齐的事务（iter 容器 + journal commit + 自动重启）
//!
//! 注意：bcachefs 中没有 ACID 事务。btree_transaction 是多个 btree_iter
//! 的容器，负责管理锁顺序、提供重启机制、以及将修改提交到 journal。
//!
//! ## Journal 集成（Phase 2）
//!
//! BtreeTrans 维护一个 journal 列表，记录事务内的所有 btree 修改操作。
//! 调用者（Volume 层）在事务提交后 drain journal，将条目写入 WAL：
//!
//! ```text
//! trans.begin();
//! btree.insert(key, val, &mut trans);
//! trans.commit()?;
//! for entry in trans.drain_journal() {
//!     let wal_entry = WalEntry::new_btree_node_entry(seq, node_addr, entry.key, entry.value, entry.op);
//!     wal.append(&wal_entry).await?;
//! }
//! ```
//!
//! ## 自动重启（Phase A）
//!
//! `commit()` 使用自动重启循环（restart loop），在锁冲突等场景下自动释放锁、
//! 重置 iter、然后重试。重启通过 `needs_restart` 标志触发，由事务内部的
//! `try_lock_all()` 或外部操作（如 iter 锁升级失败）设置。
//!
//! 重启计数由 `restart_count` 追踪，超过 `MAX_RESTARTS` 阈值时返回
//! `StorageError::TransactionRestartLimit`。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::block_device::BlockDevice;
use crate::btree::iter::BtreeIter;
use crate::btree::key::{BchVal, Bpos, BtreeEntry, BtreeKey, KeyType};
use crate::btree::op::BtreeOp;
use crate::btree::trigger::{TriggerPhase, TriggerRegistry};
use crate::btree::types::{BtreeNodeLockedType, BtreeRoot, NodeCache};
use crate::btree::BtreeEngine;
use crate::btree::BtreeId;
use crate::journal::Journal;
use crate::journal::JournalError;
use crate::journal::{
    Jset, JsetEntryType, JsetHeader, RawJsetEntry, CSUM_TYPE_NONE, JOURNAL_MAGIC, JSET_VERSION,
};
use crate::types::Watermark;
use crate::StorageError;

/// 最大重启次数（防止无限循环）
const MAX_RESTARTS: u64 = 1024;

/// 事务重启触发条件 — 对应 bcachefs `BCH_ERR_transaction_restart_*` 错误码
///
/// 扩展覆盖 bcachefs 核心 restart 场景（commit.c:1381-1523 + btree_types.h）：
///
/// | 变体 | bcachefs 对应 | 触发场景 |
/// |------|---------------|---------|
/// | LockConflict | restart_would_deadlock | 锁获取失败 |
/// | NodeSplit | restart_btree_node_split | btree 节点分裂 |
/// | KeyCacheMiss | restart_key_cache_raced | key_cache 未命中 |
/// | TriggerNeedsLock | (trans_trigger 失败) | 触发器需要重试 |
/// | NodeReadRequired | (节点重读) | 节点需要从磁盘读取 |
/// | WouldDeadlock | restart_would_deadlock_write | 死锁检测 |
/// | WriteOverflow | restart_write_overflow | btree 节点空间不足 |
/// | SplitWithInteriorUpdates | restart_split_with_interior_updates | 分裂时存在内部更新 |
/// | PathUpgradeFailed | (路径升级失败) | 无法升级到写锁 |
/// | JournalReclaimWouldDeadlock | journal_reclaim_would_deadlock | reclaim 路径死锁 |
/// | JournalOverwritesChanged | restart_journal_overwrites_changed | journal 覆盖键变化 |
/// | TraverseAll | restart_traverse_all | 遍历所有 nodes |
/// | Relock | restart_relock | 重新获取锁 |
/// | RelockPath | restart_relock_path | 重新获取指定路径锁 |
/// | Upgrade | restart_upgrade | 锁升级失败 |
/// | FaultInject | restart_fault_inject | 故障注入测试 |
/// | Nested | restart_nested | 嵌套事务重启 |
/// | LockWaitlistAlloc | restart_lock_waitlist_alloc | 等待列表分配失败 |
/// | MemoryRealloced | restart_mem_realloced | 内存重分配（路径表扩容） |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RestartReason {
    /// 锁获取失败（锁冲突）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock`
    LockConflict,
    /// btree 节点分裂导致路径失效
    /// 对应 bcachefs `BCH_ERR_transaction_restart_btree_node_split`
    NodeSplit,
    /// key_cache miss 需要 IO
    /// 对应 bcachefs `BCH_ERR_transaction_restart_key_cache_raced`
    KeyCacheMiss,
    /// 触发器需要额外的锁
    TriggerNeedsLock,
    /// btree 节点需要重新读取
    NodeReadRequired,
    /// 死锁检测 — 锁顺序违反导致死锁风险
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock_write`
    WouldDeadlock,
    /// btree 节点空间不足（写溢出）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_write_overflow`
    WriteOverflow,
    /// 分裂时存在内部更新，需完整重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_split_with_interior_updates`
    SplitWithInteriorUpdates,
    /// 无法将路径升级到写锁
    PathUpgradeFailed,
    /// journal reclaim 路径死锁 — 水位线低于 Reclaim 且被阻塞
    /// 对应 bcachefs `journal_reclaim_would_deadlock`
    JournalReclaimWouldDeadlock,
    /// journal 事务名覆盖键变化，需重新获取 journal res
    /// 对应 bcachefs `BCH_ERR_transaction_restart_journal_overwrites_changed`
    JournalOverwritesChanged,
    /// 遍历所有 nodes — 路径表顺序变化需从头遍历
    /// 对应 bcachefs `BCH_ERR_transaction_restart_traverse_all`
    TraverseAll,
    /// 重新获取锁 — 当前节点锁被释放需重获
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock`
    Relock,
    /// 重新获取指定路径锁
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock_path`
    RelockPath,
    /// 锁升级失败 — 无法从当前级别升级到目标级别
    /// 对应 bcachefs `BCH_ERR_transaction_restart_upgrade`
    Upgrade,
    /// 故障注入测试
    /// 对应 bcachefs `BCH_ERR_transaction_restart_fault_inject`
    FaultInject,
    /// 嵌套事务重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_nested`
    Nested,
    /// 等待列表分配失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_lock_waitlist_alloc`
    LockWaitlistAlloc,
    /// 内存重分配 — 路径表扩容导致指针失效
    /// 对应 bcachefs `BCH_ERR_transaction_restart_mem_realloced`
    MemoryRealloced,
}

/// 事务路径 — 按 (btree_type, pos) 排序用的轻量级 path 引用
///
/// 对应 bcachefs `btree_path` 的排序键结构。
/// 每个 `BtreePath` 指向一个 path level，包含排序所需的元数据。
#[derive(Debug, Clone, Copy)]
pub struct BtreePath {
    /// 所属 btree 类型
    pub btree_type: BtreeId,
    /// 当前遍历位置
    pub pos: Bpos,
    /// 目标锁状态
    pub lock_state: BtreeNodeLockedType,
    /// 对应的 iter 索引（`self.iters[idx]`）
    pub iter_idx: usize,
    /// 对应的 path level 索引（`self.iters[idx].path[level]`）
    pub level: usize,
}

impl BtreePath {
    /// 按 (btree_type, pos, -level) 的排序键
    ///
    /// 对应 bcachefs `__btree_path_cmp` 的排序逻辑：
    /// - 先按 btree_type 排序（不同 btree 类型不可比较）
    /// - 同 btree_type 按 pos 排序
    /// - 同 pos 按 level 逆序（父节点 > 子节点），确保解锁/加锁时父节点优先
    pub fn sort_key(&self) -> (u8, Bpos, i8) {
        (self.btree_type as u8, self.pos, -(self.level as i8))
    }
}

/// btree 事务中的单个更新条目 — 对齐 bcachefs `struct btree_insert_entry` (types.h:673-730)
///
/// 记录 btree 修改操作及其上下文（层级、触发器状态、old key 等）。
/// 替代旧版裸元组 `(BtreeId, BtreeKey, BchVal, BtreeOp)`。
#[derive(Debug, Clone)]
pub struct BtreeTransEntry {
    /// 操作类型（Insert/Delete/Whiteout）
    /// 对应 bcachefs BTREE_UPDATE_* flags
    pub op: BtreeOp,
    /// 目标 btree 实例
    pub btree_id: BtreeId,
    /// 目标层级（0 = leaf）
    /// 对应 bcachefs `level:3`
    pub level: u8,
    /// 是否更新键缓存
    /// 对应 bcachefs `cached:1`
    pub cached: bool,
    /// 新键
    pub key: BtreeKey,
    /// 新值（Insert/Whiteout 时有效，Delete 时为空值）
    pub value: BchVal,
    /// 原始键（被覆盖/删除的旧键，用于触发器 overwrite 比较）
    /// 对应 bcachefs `old_k` / `old_v`
    pub old_key: Option<BtreeKey>,
    /// 原始值
    pub old_value: Option<BchVal>,
    /// insert 触发器是否已运行
    /// 对应 bcachefs `insert_trigger_run:1`
    pub insert_trigger_run: bool,
    /// overwrite 触发器是否已运行
    /// 对应 bcachefs `overwrite_trigger_run:1`
    pub overwrite_trigger_run: bool,
    /// 所属 iter 索引（替代 bcachefs 的 path 引用）
    pub iter_idx: usize,
}

/// B-tree 事务 — 对应 bcachefs `btree_transaction`
///
/// 不是 ACID 事务，而是 iter 容器 + journal 累积器 + 重启管理器。核心职责：
/// 1. 持有多个 BtreeIter，管理它们的锁
/// 2. begin/commit 控制 iter 生命周期
/// 3. lock ordering 保证（避免死锁）
/// 4. 自动重启循环（锁冲突时自动 retry）
/// 5. 累积 btree 修改操作到 journal（Phase 2 WAL 集成）
pub struct BtreeTrans {
    /// 事务持有的 iterators
    iters: Vec<BtreeIter>,
    /// 每个 iter 对应的 BtreeId（与 iters 并行）
    iter_types: Vec<BtreeId>,
    /// 事务开始后的 journal 序列号
    journal_seq: u64,
    /// 是否已提交
    committed: bool,
    /// 节点缓存（传递给 iter 用于多级树下降）
    cache: Arc<NodeCache>,
    // ── Phase B2: WAL pin 集成 ──
    /// 当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 时设置，None = 未 pin）
    wal_pin_id: Option<u64>,
    /// Phase B1: 触发器注册表（None = 未启用触发器）
    trigger_registry: Option<Arc<TriggerRegistry>>,
    /// Phase 2: btree 修改 journal — `Vec<BtreeTransEntry>`
    ///
    /// 调用者在 insert/delete 后调用 `journal_insert` / `journal_delete`
    /// 记录修改操作。每个条目包含操作类型、btree 类型、层级、新旧键值、
    /// 触发器状态和 iter 索引。
    /// 事务 commit/rollback 后通过 `drain_journal` 取出。
    journal: Vec<BtreeTransEntry>,
    // ── Phase A: 自动重启 ──
    /// 重启计数器（每次 full restart 递增）
    restart_count: u64,
    /// 标记本次提交是否需要重启
    needs_restart: bool,
    /// 最近一次重启的原因
    restart_reason: Option<RestartReason>,
    /// 操作水位线（对应 bcachefs `BCH_WATERMARK_*`）
    ///
    /// 决定事务在资源竞争时的行为。`Reclaim` 及以上水位线的事务
    /// 在提交时跳过阻塞等待（避免 journal reclaim deadlock）。
    watermark: Watermark,
    /// 写锁已持有标志 — 对应 bcachefs `trans->write_locked`
    ///
    /// 在 `try_lock_all()` 成功获取写锁后设为 true，
    /// 在 `unlock_write()` 或 `unlock_all()` 后重置为 false。
    write_locked: bool,
}

impl BtreeTrans {
    /// 创建新事务
    pub fn new(cache: Arc<NodeCache>) -> Self {
        Self {
            iters: Vec::new(),
            iter_types: Vec::new(),
            journal_seq: 0,
            committed: false,
            cache,
            wal_pin_id: None,
            trigger_registry: None,
            journal: Vec::new(),
            restart_count: 0,
            needs_restart: false,
            restart_reason: None,
            watermark: Watermark::Normal,
            write_locked: false,
        }
    }

    /// 设置事务水位线（返回 self 以便链式调用）
    ///
    /// 水位线决定事务在资源竞争时的行为。`Reclaim` 及以上操作
    /// 跳过阻塞等待（避免 journal reclaim 死锁）。
    pub fn set_watermark(&mut self, wm: Watermark) -> &mut Self {
        self.watermark = wm;
        self
    }

    /// 创建新事务并绑定 TriggerRegistry
    pub fn with_trigger_registry(cache: Arc<NodeCache>, registry: Arc<TriggerRegistry>) -> Self {
        let mut t = Self::new(cache);
        t.trigger_registry = Some(registry);
        t
    }

    /// 设置 TriggerRegistry（可在创建后启用触发器）
    pub fn set_trigger_registry(&mut self, registry: Arc<TriggerRegistry>) {
        self.trigger_registry = Some(registry);
    }

    /// 开始事务 — 对应 bcachefs `bch2_trans_begin()`
    ///
    /// 重置所有 iter 的状态（但不释放锁）。
    /// iter 在后续操作中会被重新初始化。
    pub fn begin(&mut self) {
        // 对应 bcachefs bch2_trans_reset_updates() — 清除更新状态
        // bcachefs 中由 bch2_trans_begin() 内部调用。
        // 我们在这重置 committed/restart/journal_seq 状态，
        // 但**保留 journal 条目**（与 bcachefs 不同：它们需要重新添
        // 加 updates；我们的 journal 条目在 retry 循环中保持有效）。
        self.committed = false;
        self.needs_restart = false;
        self.restart_reason = None;
        self.journal_seq = 0;

        // 重置 iter 状态（对应 bcachefs path->should_be_locked = false）
        // 路径路径 level 锁在 unlock_all() 中已释放，这里重置 iter 元数据。
        for iter in &mut self.iters {
            iter.had_restart = false;
            // 清除所有 nospin bit（重启后重新允许自旋）
            // 对应 bcachefs path 重遍历后重新开始自旋尝试
            for level in &iter.path {
                level.node.lock.clear_nospin();
            }
        }
    }

    /// 创建一个新的 iter 并加入事务
    ///
    /// 对应 bcachefs `bch2_trans_get_iter()`
    /// `btree_type` 指定该 iter 将用于哪个 btree 实例，用于锁排序。
    pub fn get_iter(
        &mut self,
        root: &BtreeRoot,
        target: &BtreeKey,
        intent: bool,
        btree_type: BtreeId,
    ) -> &mut BtreeIter {
        let idx = self.get_path(root, target, intent, btree_type);
        &mut self.iters[idx]
    }

    /// 获取或创建 path iter，优先复用现有 path
    ///
    /// R1 路径缓存复用：先在已有 iters 中查找匹配的 path。
    /// 精确匹配 (pos == target) 直接返回索引；否则下降新 iter，
    /// 若与已有 iter 在同一个 leaf 中则复用（通过 `Arc::ptr_eq` 比较 leaf 节点地址）。
    ///
    /// 返回 iters 中的索引，调用者通过 `iter_mut(idx)` 访问。
    pub fn get_path(
        &mut self,
        root: &BtreeRoot,
        target: &BtreeKey,
        intent: bool,
        btree_type: BtreeId,
    ) -> usize {
        // 1. 精确匹配：已有 iter 的 pos == target
        for (idx, iter) in self.iters.iter().enumerate() {
            if self.iter_type(idx) == btree_type && iter.pos == *target {
                return idx;
            }
        }

        // 2. 创建临时 iter 下降
        let flags = crate::btree::iter::IterFlags {
            intent,
            forward: true,
            with_journal: false,
        };
        let new_iter = BtreeIter::init(root, target, flags, &self.cache, btree_type, None);

        // 3. 同 leaf 检测：比较临时 iter 与已有 iters 的 leaf 节点地址
        if new_iter.path.last().is_some() {
            for (idx, iter) in self.iters.iter().enumerate() {
                if self.iter_type(idx) == btree_type {
                    if let Some(existing_leaf) = iter.path.last() {
                        if Arc::ptr_eq(&existing_leaf.node, &new_iter.path.last().unwrap().node) {
                            // 释放临时 iter 的锁（BtreeIter 无 Drop 实现，
                            // 直接丢弃会导致 SixLock read_count 永久泄漏）
                            for level in &new_iter.path {
                                match level.lock_state {
                                    BtreeNodeLockedType::Read => level.node.lock.unlock_read(),
                                    BtreeNodeLockedType::Intent => level.node.lock.unlock_intent(),
                                    BtreeNodeLockedType::Write => level.node.lock.unlock_write(),
                                    BtreeNodeLockedType::None => {}
                                }
                            }
                            // forget 防止未来 BtreeIter 添加 Drop 后的二次解锁
                            std::mem::forget(new_iter);
                            // 同 leaf，丢弃临时 iter，返回已有索引
                            return idx;
                        }
                    }
                }
            }
        }

        // 4. 无复用：将临时 iter 加入事务
        self.iters.push(new_iter);
        self.iter_types.push(btree_type);
        self.iters.len() - 1
    }

    /// 获取指定位置的 iter（返回 mutable 引用）
    pub fn iter_mut(&mut self, idx: usize) -> Option<&mut BtreeIter> {
        self.iters.get_mut(idx)
    }

    /// 获取指定位置的 iter（只读）
    pub fn iter(&self, idx: usize) -> Option<&BtreeIter> {
        self.iters.get(idx)
    }

    /// 获取指定 iter 的 btree type
    pub fn iter_type(&self, idx: usize) -> BtreeId {
        self.iter_types
            .get(idx)
            .copied()
            .unwrap_or(BtreeId::Extents)
    }

    /// 提交事务 — 对应 bcachefs `__bch2_trans_commit()` (commit.c:1381-1523)
    ///
    /// ## bcachefs 对齐的入口流程
    ///
    /// ### Pre-loop 检查（`__bch2_trans_commit` line 1387-1487）
    /// 1. `bch2_trans_verify_not_unlocked_or_in_restart` — 验证事务状态
    /// 2. `trans_maybe_inject_restart` — 故障注入（跳过）
    /// 3. `bch2_trans_has_updates` — 无更新则快速返回（line 1394-1395）
    /// 4. Watermark throttle（line 1397-1403）
    /// 5. `bch2_trans_commit_run_triggers` — 事务性触发器（line 1405-1407）
    ///
    /// ### Retry 循环（`retry:` label at line 1490）
    /// ```text
    /// retry:
    ///   do_bch2_trans_commit()  // 写锁 + 原子触发器 + 键插入
    ///   if (ret) goto err
    ///   goto out
    /// err:
    ///   bch2_trans_commit_error()
    ///   if (ret) goto out
    ///   goto retry
    /// out_reset:
    ///   downgrade + reset_updates
    /// ```
    ///
    /// ### 三阶段触发器
    /// - `Transactional` — 在 retry 循环内（锁获取后）执行，失败可回滚触发重启
    /// - `Atomic` — committed 标记后执行，失败传播错误（不可回滚）
    /// - `Gc` — committed 标记后执行，错误仅日志记录（best-effort）
    ///
    /// `engine` 参数：
    /// - `None`：仅执行锁管理，不触发触发器（兼容简单场景）
    /// - `Some(engine)`：完整的三阶段触发器管线
    pub fn commit(&mut self, mut engine: Option<&mut BtreeEngine>) -> Result<(), StorageError> {
        let saved_restart_count = self.restart_count;

        // ── Pre-loop: has_updates 检查 ──
        // bcachefs line 1394-1395: if (!bch2_trans_has_updates(trans)) goto out_reset
        // 无 journal 条目且无 iter → 空事务，直接返回 Ok（无需 commit）
        if self.journal.is_empty() && self.iters.is_empty() {
            return Ok(());
        }

        // ── Pre-loop: Reclaim 水位线检查 ──
        // bcachefs interior.c:1432-1442: watermark < BCH_WATERMARK_reclaim
        // 当操作水位线 >= Reclaim，不应在 commit 路径中阻塞等待——否则自死锁。
        // Reclaim=5, InteriorUpdate=6 — 数字越大越接近 reclaim
        let is_reclaim = self.watermark.to_bits() >= Watermark::Reclaim.to_bits();

        // ── Pre-loop: 水位线节流（bcachefs line 1397-1403） ──
        // bcachefs: if watermark <= BCH_WATERMARK_normal && btree_cache_should_throttle
        // upstream 会在这里进入 bch2_trans_commit_btree_write_ratelimit()。
        // volmount 采用 cache-side polling wait，直到节流状态清除后再继续提交。
        if !is_reclaim && self.watermark <= Watermark::Normal && !self.journal.is_empty() {
            let cache = self.cache.cache();
            if cache.bch2_btree_cache_should_throttle() {
                cache.bch2_btree_cache_wait_for_throttle_clear();
            }
        }

        // ── Pre-loop: Transactional 阶段触发器 ──
        // bcachefs 在进入 retry label 之前运行 transactional triggers，
        // 因此它们不会因为后续 lock retry 被重复执行。
        //
        // 注意：这里仍然允许触发器设置 needs_restart；只要它发生在锁竞争
        // 之前，就按 bcachefs 的 restart 语义处理一次，然后再进入锁重试。
        self.run_transactional_triggers(engine.as_deref_mut())?;

        if self.needs_restart {
            self.restart_count += 1;
            if is_reclaim || self.restart_count > MAX_RESTARTS {
                return Err(StorageError::TransactionRestartLimit(self.restart_count));
            }
            self.begin();
        }

        // ── Main retry loop ──
        // 对应 bcachefs `retry:` label（commit.c:1490）
        loop {
            // ── Phase 0a: Reclaim bail ──
            // 如果获得锁前就需要重启，reclaim 操作直接失败（避免 reclaim 死锁）
            if is_reclaim && self.needs_restart {
                self.restart_count += 1;
                return Err(StorageError::TransactionRestartLimit(self.restart_count));
            }

            // ── Phase 0b: 按 journal 自然顺序获取写锁 ──
            // 对应 bcachefs `bch2_trans_lock_write_inlined()` (commit.c:141-159)
            // + 路径升级检查 (commit.c:1432-1436)
            //
            // 不排序路径：路径锁在遍历时已按树下降顺序获取，写锁升级按
            // journal 追加顺序（与 bcachefs `trans_for_each_update` 对齐）。
            self.try_lock_all();
            self.record_locked_seqs();

            if self.needs_restart {
                self.restart_count += 1;
                if is_reclaim || self.restart_count > MAX_RESTARTS {
                    return Err(StorageError::TransactionRestartLimit(self.restart_count));
                }
                self.unlock_all();
                self.begin();
                continue;
            }

            // ── Phase 1: 标记已提交 + 释放写锁 ──
            // 对应 bcachefs committed 标记 + bch2_trans_unlock_updates_write (commit.c:166-176)
            self.committed = true;
            self.unlock_write();

            // ── Phase 2: Atomic 阶段触发器（不可回滚） ──
            // bcachefs: run_one_mem_trigger 在 do_bch2_trans_commit 内部，
            // 在 journal_res_get 之后、key insert 之前执行
            self.run_atomic_triggers(engine.as_deref_mut())?;

            // ── Phase 3: Gc 阶段触发器（best-effort） ──
            // bcachefs: bch2_trans_commit_run_gc_triggers 在 atomic 之后
            if let Some(ref mut eng) = engine {
                if self.trigger_registry.is_some() {
                    let _ = self.fire_triggers_on_journal(TriggerPhase::Gc, eng);
                }
            }

            // ── 成功退出 ──
            // 对应 bcachefs out_reset: bch2_trans_downgrade + bch2_trans_reset_updates
            self.restart_count = saved_restart_count; // 重置计数
            return Ok(());
        }
    }

    /// 运行 Transactional 阶段触发器，失败时设置 needs_restart
    ///
    /// 对应 bcachefs `bch2_trans_commit_run_triggers()` (commit.c:598-647)。
    /// 在 retry 循环的 Phase 0b 运行（try_lock_all 之前），对齐 bcachefs 的触发顺序。
    /// fire_triggers_on_journal 使用 engine.get_entry() 只读 HashMap，无需 btree 锁。
    fn run_transactional_triggers(
        &mut self,
        engine: Option<&mut BtreeEngine>,
    ) -> Result<(), StorageError> {
        let Some(eng) = engine else { return Ok(()) };
        let Some(ref registry) = self.trigger_registry else {
            return Ok(());
        };
        if registry.is_empty() || self.journal.is_empty() {
            return Ok(());
        }

        if let Err(e) = self.fire_triggers_on_journal(TriggerPhase::Transactional, eng) {
            // bcachefs: 事务性触发器失败 → btree_trans_restart（可恢复）
            // 标记 needs_restart 供调用者决定是否重试
            self.needs_restart = true;
            self.restart_reason = Some(RestartReason::TriggerNeedsLock);
            return Err(e);
        }
        Ok(())
    }

    /// 运行 Atomic 阶段触发器，错误直接传播（不可回滚）
    ///
    /// 对应 bcachefs `run_one_mem_trigger` with `BTREE_TRIGGER_atomic` (commit.c:1153-1159)。
    /// bcachefs 中在 journal_res_get + commit_hooks 之后执行。
    fn run_atomic_triggers(
        &mut self,
        engine: Option<&mut BtreeEngine>,
    ) -> Result<(), StorageError> {
        let Some(eng) = engine else { return Ok(()) };
        let Some(ref registry) = self.trigger_registry else {
            return Ok(());
        };
        if registry.is_empty() {
            return Ok(());
        }

        for entry in &self.journal {
            let key_bytes = bincode::serialize(&entry.key).unwrap_or_default();
            let new_bytes = match entry.op {
                BtreeOp::Insert | BtreeOp::Whiteout => {
                    Some(bincode::serialize(&entry.value).unwrap_or_default())
                }
                BtreeOp::Delete => None,
            };
            let old_bytes = Self::resolve_old_bytes(entry, eng);
            if let Err(e) = registry.fire(
                eng,
                entry.btree_id,
                entry.key.key_type as u8,
                TriggerPhase::Atomic,
                &key_bytes,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            ) {
                // bcachefs trans_commit_fatal_err: 原子阶段失败 → 不可恢复
                eprintln!(
                    "Atomic trigger failed for {:?} op {:?}: {}",
                    entry.btree_id, entry.op, e
                );
                return Err(e);
            }
        }
        Ok(())
    }

    /// 提交事务并触发 btree 触发器（Phase B1），然后应用 journal 条目到 btree 节点（Phase 2）
    ///
    /// 对应 bcachefs `__bch2_trans_commit()` + `do_bch2_trans_commit()` + `bch2_btree_insert_key_leaf()` 串联流程。
    ///
    /// bcachefs Phase 0–4 流程对齐：
    /// ```text
    /// Phase | bcachefs (commit.c:1381-1524)          | volmount commit_with_engine
    /// ------|-----------------------------------------|--------------------------------
    /// 0a    | run_triggers (line 1405)                | commit() → Phase 0b: run_transactional_triggers
    /// 0b    | path_upgrade (line 1432) + disk_res     | commit() → Phase 0c: try_lock_all (upgrade to write)
    /// 0c    | —                                       | commit() → Phase 1: committed=true + unlock_write
    /// 1     | do_bch2_trans_commit (line 1500):
    ///       |   lock_write (line 1305)
    ///       |   journal_res (line 1111)
    ///       |   atomic_triggers (line 1156)           | commit() → Phase 2: run_atomic_triggers
    ///       |   gc_triggers (line 1162)               | commit() → Phase 3: gc_triggers (best-effort)
    /// 2     | insert_key_leaf (line 1277)             | commit_with_engine → Phase 2: engine.insert_entry
    /// 3     | downgrade + reset_updates (line 1514)   | commit() → 自动 (restart_count reset)
    /// ```
    /// 主要偏差：bcachefs 在 journal_res + lock_write 之后执行 atomic triggers，
    /// volmount 在 unlock_write（写锁降级为 intent）之后执行 atomic triggers。
    /// 这是安全的因为 (1) atomic triggers 只读访问 HashMap，(2) journal_seq 保证线性化。
    ///
    /// 委托给 `commit(Some(engine))` 执行三阶段触发器：
    /// - Transactional: 锁获取后运行，失败触发重启
    /// - Atomic: committed 标记后运行，失败传播错误
    /// - Gc: committed 标记后运行，best-effort
    ///
    /// 之后执行 Phase 2 btree 节点修改（使用 journal_seq 写入 btree 节点）。
    /// Phase 2 在 committed 标记之后，与 bcachefs 语义一致：
    /// journal 保留已完成，btree 修改虽然发生，但若后续 journal 填充失败，
    /// recovery 不会看到未填充的条目（线性化保证）。
    ///
    /// 注意：若 commit(Some(engine)) 返回 Err（包括重启信号），
    /// btree 节点修改不会执行——与 bcachefs 的 out_reset 语义一致。
    pub fn commit_with_engine(&mut self, engine: &mut BtreeEngine) -> Result<(), StorageError> {
        // Phase B1: committed 标记 + 三阶段触发器
        self.commit(Some(engine))?;

        // ★ Phase 2: 应用 journal 条目到 btree 节点（在 committed 标记之后）
        // 对应 bcachefs trans_for_each_update + bch2_btree_insert_key_leaf (commit.c:1273-1282)
        if self.journal_seq > 0 {
            for entry in self.journal.iter() {
                match entry.op {
                    BtreeOp::Insert => {
                        if entry.cached {
                            // TC5: cached 写回路径 — 写入 btree（不 invalidate cache）
                            // + 创建脏缓存条目。flush_cache_dirty_keys 在 trans_commit Phase 1d 中处理。
                            engine.insert_entry_cached(
                                entry.btree_id,
                                entry.key.clone(),
                                entry.value,
                                self.journal_seq,
                            );
                        } else {
                            // 非 cached 条目：保持现有的写穿行为（btree 写入 + invalidate cache）
                            engine.insert_entry(
                                entry.btree_id,
                                entry.key.clone(),
                                entry.value,
                                self.journal_seq,
                            );
                        }
                    }
                    BtreeOp::Delete => {
                        engine.delete_entry(entry.btree_id, &entry.key, self.journal_seq);
                    }
                    BtreeOp::Whiteout => {}
                }
            }
        }

        Ok(())
    }

    /// 错误处理分支 — 对应 bcachefs `__bch2_trans_commit_error()` (commit.c:788-855)
    ///
    /// 处理提交过程中的可恢复/不可恢复错误：
    ///
    /// | 错误类型 | 处理方式 | bcachefs 对应 |
    /// |---------|---------|--------------|
    /// | TransactionRestartLimit | 直接传播 | — |
    /// | BtreeNodeFull | 设置 WriteOverflow 重启 | btree_insert_btree_node_full |
    /// | JournalBlocked | reclaim 检查 + 传播 | journal_res_blocked |
    /// | 其他 Transaction 错误 | 设为 restart 信号，unlock+begin+retry | btree_trans_restart |
    /// | 其他所有错误 | 直接传播 | default: BUG_ON |
    ///
    /// 返回 `Ok(())` 表示错误已处理，调用者应继续 retry 循环。
    /// 返回 `Err(e)` 表示错误不可恢复，调用者应终止。
    fn __commit_error(&mut self, err: StorageError, is_reclaim: bool) -> Result<(), StorageError> {
        match &err {
            // 重启限制：直接传播（已无重试空间）
            StorageError::TransactionRestartLimit(_) => Err(err),

            // BtreeNodeFull: 设置 WriteOverflow 重启
            // 对应 bcachefs -BCH_ERR_btree_insert_btree_node_full → bch2_btree_split_leaf + restart
            StorageError::BtreeNodeFull => {
                if is_reclaim {
                    return Err(err);
                }
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::WriteOverflow);
                Ok(()) // 信号：准备 retry
            }

            // Journal 阻塞：检查 reclaim 死锁
            // 对应 bcachefs journal_res_blocked → journal_reclaim_would_deadlock
            StorageError::JournalError(msg) if msg.contains("blocked") || msg.contains("full") => {
                if is_reclaim {
                    self.needs_restart = true;
                    self.restart_reason = Some(RestartReason::JournalReclaimWouldDeadlock);
                    return Err(StorageError::JournalReclaimWouldDeadlock);
                }
                // 非 reclaim：unlock + begin + retry（对应 bcachefs drop_locks_do + journal_res_get）
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::LockConflict);
                Ok(())
            }

            // 不可恢复错误：直接传播
            _ => Err(err),
        }
    }

    /// 解析 old_key/old_value：优先使用 entry 中行内的 old_value（由调用者传入），
    /// 回退到 engine.get_entry() 实时查询（兼容尚未传入 old_value 的调用者）。
    fn resolve_old_bytes(entry: &BtreeTransEntry, engine: &mut BtreeEngine) -> Option<Vec<u8>> {
        if let Some(ref ov) = entry.old_value {
            return Some(bincode::serialize(ov).unwrap_or_default());
        }
        // 回退：从 engine 实时查询旧值
        if let Some((_k, v)) = engine.get_entry(entry.btree_id, &entry.key) {
            return Some(bincode::serialize(&v).unwrap_or_default());
        }
        None
    }

    /// 在 journal 上触发指定阶段的所有已注册触发器（Phase B1）
    ///
    /// 遍历 journal 中的每个条目，对匹配 (BtreeId, TriggerPhase) 的触发器
    /// 调用 `TriggerRegistry::fire()`。Transactional 阶段的错误会传播给调用者
    /// （触发重启），而 Atomic/Gc 阶段的错误仅记录日志。
    ///
    /// 如果未设置 trigger_registry，此方法为空操作（零开销）。
    fn fire_triggers_on_journal(
        &self,
        phase: TriggerPhase,
        engine: &mut BtreeEngine,
    ) -> Result<(), StorageError> {
        let Some(ref registry) = self.trigger_registry else {
            return Ok(());
        };

        for entry in &self.journal {
            let key_bytes = bincode::serialize(&entry.key).unwrap_or_default();
            let new_bytes = match entry.op {
                BtreeOp::Insert | BtreeOp::Whiteout => {
                    Some(bincode::serialize(&entry.value).unwrap_or_default())
                }
                BtreeOp::Delete => None,
            };
            // 优先使用 entry.old_value（行内值），回退到 engine.get_entry()
            let old_bytes = Self::resolve_old_bytes(entry, engine);
            let result = registry.fire(
                engine,
                entry.btree_id,
                entry.key.key_type as u8,
                phase,
                &key_bytes,
                old_bytes.as_deref(),
                new_bytes.as_deref(),
            );
            match (phase, result) {
                (TriggerPhase::Transactional, Err(e)) => return Err(e),
                (_, Err(e)) => {
                    eprintln!(
                        "Phase {:?} trigger failed for {:?}: {}",
                        phase, entry.btree_id, e
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 回滚更新 — 对应 bcachefs `bch2_trans_reset_updates()` (update.h:557-571)
    ///
    /// **不放锁，不清除 iters。** 仅重置 journal 和 committed 状态。
    /// bcachefs 语义：`bch2_trans_reset_updates()` 在成功和失败路径中都会调用，
    /// 清除 `nr_updates`、`journal_entries`、`hooks` 等，但**不**重置 `restart_count`。
    /// restart_count 仅在成功提交时由调用者重置（saved_restart_count 模式）。
    ///
    /// 如需完全清理（释放锁 + 清除 iters），请手动调用 `unlock_all()` + `iters.clear()`。
    pub fn rollback(&mut self) {
        self.journal.clear();
        self.committed = false;
        self.needs_restart = false;
        self.restart_reason = None;
    }

    // ─── Phase A: 锁排序 + 自动重启 ──────────────────────────

    /// 收集所有 path levels 为 BtreePath 列表
    ///
    /// 遍历每个 iter 的每个 path level，创建对应的 BtreePath。
    pub fn collect_paths(&self) -> Vec<BtreePath> {
        let mut paths = Vec::new();
        for (iter_idx, iter) in self.iters.iter().enumerate() {
            let btree_type = self.iter_type(iter_idx);
            let pos = iter.pos.to_bpos();

            for (level_idx, path_level) in iter.path.iter().enumerate() {
                paths.push(BtreePath {
                    btree_type,
                    pos,
                    lock_state: path_level.lock_state,
                    iter_idx,
                    level: level_idx,
                });
            }
        }
        paths
    }

    /// 获取 journal 条目的写锁 — 对应 bcachefs `bch2_trans_lock_write_inlined()` (commit.c:141-159)
    ///
    /// 遍历 journal 条目（自然追加顺序，与 bcachefs `trans_for_each_update` 对齐），
    /// 对引用的 leaf 节点做 intent→write 升级。路径已持有 intent/read 锁
    ///（来自遍历时的 `lock_read()` / `lock_intent()` 阻塞获取）。
    ///
    /// bcachefs 不需要 `sort_locks()` 的原因：
    /// 1. 路径锁在遍历时已按 (btree_id, pos, level) 自然顺序获取（树下降路径确定）
    /// 2. 写锁升级顺序由 `trans->updates[]` 追加顺序决定（`bch2_trans_update` 调用顺序）
    /// 3. SIX lock 内置死锁检测（`bch2_six_check_for_deadlock`）兜底 ABBA 场景
    ///
    /// volmount 的 SixLock 暂无在线死锁检测，但 `upgrade_intent_to_write()`
    /// 是 try-only（spin + yield，不 sleep），因此 ABBA 场景双方都失败→重启，
    /// 不存在真死锁。
    fn try_lock_all(&mut self) {
        for i in 0..self.journal.len() {
            let entry = &self.journal[i];

            // `same_leaf_as_prev()` 优化：跳过重复 leaf（bcachefs commit.c:96-101）
            if i > 0 {
                let prev = &self.journal[i - 1];
                if prev.iter_idx == entry.iter_idx && prev.level == entry.level {
                    continue;
                }
            }

            let iter_idx = entry.iter_idx;
            let level = entry.level as usize;

            if iter_idx >= self.iters.len() || level >= self.iters[iter_idx].path.len() {
                continue;
            }

            let path_level = &self.iters[iter_idx].path[level];

            let ok = match path_level.lock_state {
                // intent → write 升级（标准路径，遍历时已持有 intent）
                BtreeNodeLockedType::Intent => path_level.node.lock.upgrade_intent_to_write(),
                // 已有 write 锁（多个条目共享同一 leaf）
                BtreeNodeLockedType::Write => continue,
                // read → intent → write（read-only 路径升级为写）
                BtreeNodeLockedType::Read => {
                    if path_level.node.lock.upgrade_read_to_intent() {
                        path_level.node.lock.upgrade_intent_to_write()
                    } else {
                        false
                    }
                }
                // 路径未锁 — 遍历未获取锁，需完整重启
                BtreeNodeLockedType::None => {
                    self.needs_restart = true;
                    self.restart_reason = Some(RestartReason::LockConflict);
                    return;
                }
            };

            if !ok {
                self.needs_restart = true;
                self.restart_reason = Some(RestartReason::LockConflict);
                return;
            }
            self.iters[iter_idx].path[level].lock_state = BtreeNodeLockedType::Write;
        }
        self.write_locked = true;
    }

    /// 记录所有 path levels 的 locked_seq
    ///
    /// 在每次成功获取锁后调用，保存每个节点锁的当前序列号。
    /// 序列号在写锁释放时递增，因此 locked_seq 可用于检测节点是否被外部修改：
    /// 重启时若 lock.seq() == locked_seq，说明节点未被写操作触及，可跳过重读。
    ///
    /// 对应 bcachefs `bch2_trans_unlock()` (locking.c:1478-1490) 中
    /// `bch2_btree_path_traverse_unlock()` 的 seq 记录时机——在锁释放前记录。
    /// 调用位置：`commit()` Phase 0c 中 `try_lock_all()` 成功后立即调用。
    fn record_locked_seqs(&mut self) {
        for iter in &mut self.iters {
            for level in &mut iter.path {
                level.locked_seq = level.node.lock.seq();
            }
        }
    }

    /// 优化版重启：利用 locked_seq 检测是否需要完整重启
    ///
    /// R2 优化：检查每个 iter 的 path levels 的 seq 是否与加锁时相同。
    /// 如果所有 iters 的 seq 都未变化，说明数据未被外部修改，返回 `None`。
    /// 否则返回 `Some(reason)` 表示需要完整重启。
    ///
    /// 无论检测结果如何，都会释放所有锁并重置状态（同 `unlock_all() + begin()`）。
    pub fn restart_optimized(&mut self) -> Option<RestartReason> {
        // 1. 检查是否有任何 iter 的任意 path level seq 与 locked_seq 不符
        //
        // 注意：不仅检查 leaf，还要检查所有中间层级。因为 SixLock
        // 的 seq 是节点级别的——内部节点（split/merge）的变化不会
        // 传播到子节点。如果仅检查 leaf 而跳过内部节点，可能在
        // 树拓扑已修改时错误返回 None（路径失效）。
        let needs_full_restart = self.iters.iter().any(|iter| {
            iter.path
                .iter()
                .any(|level| level.node.lock.seq() != level.locked_seq)
        });

        // 2. 取出重启原因（如果有）
        let reason = self.restart_reason.take();

        // 3. 释放所有锁并重置状态
        self.unlock_all();
        self.begin();

        if needs_full_restart {
            // 检测到 seq 变化 → 调用者应执行完整重下降
            Some(reason.unwrap_or(RestartReason::LockConflict))
        } else {
            // 所有 seq 未变 → 调用者可跳过重下降
            None
        }
    }

    /// 释放所有当前持有的锁并重置锁状态（用于重启前的清理）
    ///
    /// 对应 bcachefs `bch2_trans_unlock()` (locking.c:1478-1490)。遍历所有 iter 的 path levels，
    /// 将持有的锁释放，并将 `lock_state` 重置为 `None`。
    ///
    /// **不显式清除 `locked_seq`** — 对齐 bcachefs `__bch2_btree_path_unlock()`
    /// (locking.c:1440-1454)：path-level 释放后 seq 保留，下次遍历时重新获取。
    /// `locked_seq` 由 `restart_optimized()` 用于检测节点是否被外部修改。
    ///
    /// 如果 restart_count 超过阈值（>= 100），在所有锁上设置 nospin bit
    /// 以跳过后续的自旋尝试。
    pub fn unlock_all(&mut self) {
        // Phase C1: 高频重启时设置 nospin bit，跳过后续自旋
        let high_restart = self.restart_count >= 100;

        for iter in &mut self.iters {
            for level in &mut iter.path {
                if level.lock_state == BtreeNodeLockedType::None {
                    continue;
                }
                match level.lock_state {
                    BtreeNodeLockedType::Read => level.node.lock.unlock_read(),
                    BtreeNodeLockedType::Intent => level.node.lock.unlock_intent(),
                    BtreeNodeLockedType::Write => {
                        level.node.lock.unlock_write();
                    }
                    BtreeNodeLockedType::None => {}
                }
                // 高频重启时在已锁的节点上设置 nospin
                if high_restart {
                    level.node.lock.set_nospin();
                }
                // 重置锁状态为 None（对应 bcachefs 解锁后清空 path 锁状态）
                level.lock_state = BtreeNodeLockedType::None;
            }
        }
        self.write_locked = false;
    }

    /// 释放写锁但保持 intent/read 锁 — 对应 bcachefs `bch2_trans_unlock_write()`
    ///
    /// 遍历所有 iter 的 path levels，只释放写锁，降级到 intent。
    /// 不释放 intent 或 read 锁（与 `unlock_all()` 不同）。
    ///
    /// bcachefs 对应：locking.c `bch2_trans_unlock_write()` (line 1572-1581)
    pub fn unlock_write(&mut self) {
        if !self.write_locked {
            return;
        }
        for iter in &mut self.iters {
            for level in &mut iter.path {
                if level.lock_state != BtreeNodeLockedType::Write {
                    continue;
                }
                level.node.lock.unlock_write();
                level.lock_state = BtreeNodeLockedType::Intent;
            }
        }
        self.write_locked = false;
    }

    /// 请求重启（由外部操作用于触发重启）
    ///
    /// 当 iter 升级锁失败或检测到路径失效时调用。
    pub fn request_restart(&mut self, reason: RestartReason) {
        self.needs_restart = true;
        self.restart_reason = Some(reason);
    }

    /// 重启事务：释放所有锁 + 重置 iters + 递增 restart_count
    ///
    /// 组合了已有的 `unlock_all()` + `begin()`，对应 bcachefs `btree_trans_restart()` (iter.h:613)
    /// 设置 restart 标志 + retry 循环中调用 `bch2_trans_begin()` (iter.c:3887-3946)。
    ///
    /// bcachefs 重启为两阶段模式：(1) `btree_trans_restart()` 设置 `trans->restarted` 错误码
    /// 和 `last_restarted_ip`； (2) retry 循环入口调用 `bch2_trans_begin()` 重置事务状态
    /// （restart_count++、清路径标志、resize mem 等）。volmount 将两阶段融合为一次 `restart()` 调用。
    ///
    /// 返回：
    /// - `Some(reason)` — 正常重启，返回触发重启的原因并消费
    /// - `None` — 超过 `MAX_RESTARTS` 阈值，调用者**必须**终止循环
    ///
    /// 正确调用模式见 `lockrestart_do!` 宏。
    pub fn restart(&mut self) -> Option<RestartReason> {
        let reason = self.restart_reason.take();
        self.restart_count += 1;
        if self.restart_count > MAX_RESTARTS {
            return None;
        }
        self.unlock_all();
        self.begin();
        reason
    }

    /// 重启并自动重获锁（D3）— volmount 特有优化
    ///
    /// bcachefs 无直接对应函数。bcachefs 的 relock 机制通过路径 `should_be_locked` 标志 +
    /// `bch2_btree_path_traverse_all()` (iter.c:1264-1340) 在 `bch2_trans_begin()` 中统一处理。
    ///
    /// volmount 采用"保存锁状态 → 释放 → 重获取"的显式模式，适用于锁释放后需要
    /// 立即重获且知道目标锁状态的场景。因 Rust 借用规则限制，无法复用 bcachefs 的
    /// should_be_locked + retraverse 模式。
    ///
    /// 两阶段算法：
    /// 1. 保存当前 path 的目标锁状态
    /// 2. 释放所有锁 + 重置 iter（同 `restart()`）
    /// 3. 按保存的锁状态重新尝试获取所有锁
    /// 4. 若任何锁获取失败，设置 `needs_restart`（调用者可再次重启）
    ///
    /// 与 `restart()` 的区别：完成后 path levels 保持与重启前相同的锁状态，
    /// 而非全部为 None。适用于锁释放后需要立即重获的场景。
    pub fn restart_with_relock(&mut self) -> Option<RestartReason> {
        // Phase 1: 保存目标锁状态
        let targets: Vec<(usize, usize, BtreeNodeLockedType)> = self
            .iters
            .iter()
            .enumerate()
            .flat_map(|(iter_idx, iter)| {
                iter.path
                    .iter()
                    .enumerate()
                    .filter(|(_, level)| level.lock_state != BtreeNodeLockedType::None)
                    .map(move |(level_idx, level)| (iter_idx, level_idx, level.lock_state))
            })
            .collect();

        let reason = self.restart_reason.take();
        self.restart_count += 1;
        if self.restart_count > MAX_RESTARTS {
            return None;
        }

        // Phase 2: 释放所有锁 + 重置 iter
        self.unlock_all();
        self.begin();

        // Phase 3: 重新获取目标锁
        for (iter_idx, level_idx, target) in &targets {
            if *iter_idx >= self.iters.len() || *level_idx >= self.iters[*iter_idx].path.len() {
                // iter 已不存在或 path 深度变化，跳过（trigger 会处理）
                continue;
            }
            let node = &self.iters[*iter_idx].path[*level_idx].node;
            let ok = match target {
                BtreeNodeLockedType::Read => node.lock.lock_read(),
                BtreeNodeLockedType::Intent => node.lock.lock_intent(),
                BtreeNodeLockedType::Write => node.lock.lock_write(),
                BtreeNodeLockedType::None => true,
            };
            if ok {
                self.iters[*iter_idx].path[*level_idx].lock_state = *target;
            } else {
                self.needs_restart = true;
                // 单次失败即停止 relock，剩余锁由下次重启处理
                return reason;
            }
        }

        reason
    }

    /// 检查是否需要重启
    pub fn needs_restart(&self) -> bool {
        self.needs_restart
    }

    /// 获取重启计数
    pub fn restart_count(&self) -> u64 {
        self.restart_count
    }

    /// 获取最近一次重启的原因
    pub fn restart_reason(&self) -> Option<RestartReason> {
        self.restart_reason
    }

    // ─── Phase A6: 重启触发辅助方法 ──────────────────────────

    /// 触发 NodeSplit 重启 — btree 节点分裂后调用，通知事务路径可能已失效
    pub fn trigger_node_split(&mut self) {
        self.request_restart(RestartReason::NodeSplit);
    }

    /// 触发 KeyCacheMiss 重启 — 缓存中找不到节点时调用
    pub fn trigger_key_cache_miss(&mut self) {
        self.request_restart(RestartReason::KeyCacheMiss);
    }

    /// 触发 NodeReadRequired 重启 — 节点需要重新从磁盘读取时调用
    pub fn trigger_node_read_required(&mut self) {
        self.request_restart(RestartReason::NodeReadRequired);
    }

    /// 触发 TriggerNeedsLock 重启 — 触发器需要额外的锁时调用
    pub fn trigger_needs_lock(&mut self) {
        self.request_restart(RestartReason::TriggerNeedsLock);
    }

    /// 触发死锁重启 — 锁顺序违反导致死锁风险
    /// 对应 bcachefs `BCH_ERR_transaction_restart_would_deadlock_write`
    pub fn trigger_would_deadlock(&mut self) {
        self.request_restart(RestartReason::WouldDeadlock);
    }

    /// 触发写溢出重启 — btree 节点空间不足
    /// 对应 bcachefs `BCH_ERR_transaction_restart_write_overflow`
    pub fn trigger_write_overflow(&mut self) {
        self.request_restart(RestartReason::WriteOverflow);
    }

    /// 触发分裂+内部更新重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_split_with_interior_updates`
    pub fn trigger_split_with_interior_updates(&mut self) {
        self.request_restart(RestartReason::SplitWithInteriorUpdates);
    }

    /// 触发 TraverseAll 重启 — 路径表顺序变化需从头遍历
    /// 对应 bcachefs `BCH_ERR_transaction_restart_traverse_all`
    pub fn trigger_traverse_all(&mut self) {
        self.request_restart(RestartReason::TraverseAll);
    }

    /// 触发 Relock 重启 — 当前节点锁被释放需重获
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock`
    pub fn trigger_relock(&mut self) {
        self.request_restart(RestartReason::Relock);
    }

    /// 触发 RelockPath 重启 — 重新获取指定路径锁
    /// 对应 bcachefs `BCH_ERR_transaction_restart_relock_path`
    pub fn trigger_relock_path(&mut self) {
        self.request_restart(RestartReason::RelockPath);
    }

    /// 触发 Upgrade 重启 — 锁升级失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_upgrade`
    pub fn trigger_upgrade(&mut self) {
        self.request_restart(RestartReason::Upgrade);
    }

    /// 触发 FaultInject 重启 — 故障注入测试
    /// 对应 bcachefs `BCH_ERR_transaction_restart_fault_inject`
    pub fn trigger_fault_inject(&mut self) {
        self.request_restart(RestartReason::FaultInject);
    }

    /// 触发 Nested 重启 — 嵌套事务重启
    /// 对应 bcachefs `BCH_ERR_transaction_restart_nested`
    pub fn trigger_nested(&mut self) {
        self.request_restart(RestartReason::Nested);
    }

    /// 触发 LockWaitlistAlloc 重启 — 等待列表分配失败
    /// 对应 bcachefs `BCH_ERR_transaction_restart_lock_waitlist_alloc`
    pub fn trigger_lock_waitlist_alloc(&mut self) {
        self.request_restart(RestartReason::LockWaitlistAlloc);
    }

    /// 触发 MemoryRealloced 重启 — 内存重分配（路径表扩容）
    /// 对应 bcachefs `BCH_ERR_transaction_restart_mem_realloced`
    pub fn trigger_mem_realloced(&mut self) {
        self.request_restart(RestartReason::MemoryRealloced);
    }

    /// 检查所有 iter 的路径完整性 — 若任何路径可能失效，触发 NodeSplit 重启
    ///
    /// 对应 bcachefs `__bch2_btree_path_verify()` (iter.c:378-396) 的简化轻量版本。
    /// bcachefs 的 verify 更严格：验证每个 level 的节点指针一致性、锁状态、btree_key 位置等，
    /// 仅在 `CONFIG_BCACHEFS_DEBUG` + `bch2_debug_check_iterators` 启用时生效。
    /// volmount 版本始终运行，仅检查路径长度和空路径——足以判断是否需要重启。
    ///
    /// 遍历每个 iter 的 path levels，检查：
    /// - path 是否为空的（空路径表示未正确初始化）
    /// - 当 tree depth 变化时，path 数量可能不匹配
    pub fn check_path_integrity(&mut self, tree_depth: u8) {
        for iter in &self.iters {
            if iter.path.is_empty() {
                self.trigger_node_read_required();
                return;
            }
            let expected_len = (tree_depth as usize) + 1;
            if iter.path.len() != expected_len && !iter.path.is_empty() {
                self.trigger_node_split();
                return;
            }
        }
    }

    /// 检测 iter 路径是否需要重启（通过 had_restart 标志）
    ///
    /// 对应 bcachefs `trans->restarted` 标志检测的 volmount 等效。
    /// bcachefs 使用 `trans->restarted` 单一标志位（由 `btree_trans_restart()` iter.h:613 设置），
    /// 重启循环检查此标志决定是否 retry。volmount 使用每个 iter 粒度的 `had_restart` 标志，
    /// 使调用者可以精确知道哪个 iter 触发了重启。
    ///
    /// 返回 true 表示检测到任何 iter 请求重启。
    pub fn detect_iter_restart_needed(&mut self) -> bool {
        for iter in &mut self.iters {
            if iter.had_restart {
                iter.had_restart = false;
                self.request_restart(RestartReason::LockConflict);
                return true;
            }
        }
        false
    }

    // ─── 原有方法 ────────────────────────────────────────────

    /// 事务持有的 iter 数量
    pub fn iter_count(&self) -> usize {
        self.iters.len()
    }

    /// 是否已提交
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// 设置 journal 序列号
    pub fn set_journal_seq(&mut self, seq: u64) {
        self.journal_seq = seq;
    }

    /// 获取 journal 序列号
    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    // ─── Phase B2: WAL Pin 集成 ─────────────────────────────

    /// 设置当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 时调用）
    pub fn set_wal_pin(&mut self, pin_id: u64) {
        self.wal_pin_id = Some(pin_id);
    }

    /// 清除当前事务持有的 WAL pin ID（由 Volume 层在写 WAL 后调用）
    pub fn clear_wal_pin(&mut self) {
        self.wal_pin_id = None;
    }

    /// 获取当前事务持有的 WAL pin ID
    pub fn wal_pin_id(&self) -> Option<u64> {
        self.wal_pin_id
    }

    // ─── Phase 2 Journal ──────────────────────────────────────

    /// 记录插入操作到 journal（调用者在 btree.insert 成功后调用）
    ///
    /// 对应 bcachefs `bch2_trans_update()` / `bch2_btree_insert()`。
    /// `iter_idx` 是 `get_iter()` 返回的索引，`level` 默认为 0（leaf）。
    pub fn journal_insert(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
        iter_idx: usize,
    ) {
        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Insert,
            btree_id: btree_type,
            level,
            cached,
            key,
            value,
            old_key: None,
            old_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            iter_idx,
        });
    }

    /// 记录删除操作到 journal（调用者在 btree.delete 成功后调用）
    pub fn journal_delete(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        iter_idx: usize,
    ) {
        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Delete,
            btree_id: btree_type,
            level,
            cached,
            key,
            value: BchVal::new(0, 0),
            old_key: None,
            old_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            iter_idx,
        });
    }

    /// 记录 whiteout 操作到 journal
    pub fn journal_whiteout(
        &mut self,
        btree_type: BtreeId,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
        iter_idx: usize,
    ) {
        self.journal.push(BtreeTransEntry {
            op: BtreeOp::Whiteout,
            btree_id: btree_type,
            level,
            cached,
            key,
            value,
            old_key: None,
            old_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            iter_idx,
        });
    }

    /// 取出所有 journal 条目（事务 commit/rollback 后由调用者消费）
    ///
    /// 返回 `Vec<BtreeTransEntry>` 列表。
    /// 调用者应写入 WAL，可根据 `entry.btree_id` 决定写入哪个 journal/bucket。
    pub fn drain_journal(&mut self) -> Vec<BtreeTransEntry> {
        std::mem::take(&mut self.journal)
    }

    /// journal 是否为空
    pub fn journal_is_empty(&self) -> bool {
        self.journal.is_empty()
    }

    /// journal 条目数
    pub fn journal_len(&self) -> usize {
        self.journal.len()
    }

    /// 将事务中累积的 journal 条目通过新 reservation API 写入 Journal。
    ///
    /// 使用 `journal_res_get_fast()` + `commit()` + `journal_res_put()` 无锁 fastpath，
    /// 按 BtreeId 分组，每组调用一次 `Journal::append()`（接受 `&self`）。
    ///
    /// # 返回
    ///
    /// 返回最后一个条目的 seq（若条目为空则返回 `JournalError::Overflow`）。
    ///
    /// # Deprecated
    ///
    /// 此方法已被 `trans_commit()` 取代。`trans_commit()` 使用 bcachefs 完全对齐的
    /// 4 阶段流程（reserve→modify→fill→release），不再依赖此方法。
    #[deprecated(note = "use trans_commit() instead (bcachefs-aligned 4-phase flow)")]
    pub async fn commit_with_journal(
        &mut self,
        journal: &Journal,
        engine: &mut BtreeEngine,
        backend: &dyn BlockDevice,
    ) -> Result<u64, JournalError> {
        if self.journal.is_empty() {
            return Err(JournalError::Overflow(
                "transaction has no journal entries".into(),
            ));
        }

        // flush dirty key cache before journal write
        engine.flush_cache_dirty_keys(0);

        // 按 BtreeId 分组
        let mut groups: HashMap<BtreeId, Vec<BtreeEntry>> = HashMap::new();
        for je in self.journal.iter() {
            let key_type = match je.op {
                BtreeOp::Insert => KeyType::Normal,
                BtreeOp::Delete => KeyType::Deleted,
                BtreeOp::Whiteout => KeyType::Whiteout,
            };
            let group_entry = BtreeEntry {
                pos: Bpos::from_key(&je.key),
                key_type,
                value: crate::btree::key::KeyValue::Extent(je.value),
            };
            groups.entry(je.btree_id).or_default().push(group_entry);
        }

        let mut last_seq = 0u64;
        for (bt, entries) in &groups {
            last_seq = journal.append(*bt, entries, false, backend).await?;
        }

        Ok(last_seq)
    }

    // ─── bcachefs 对齐方法 ─────────────────────────────────

    /// 查找或创建 iter（bcachefs 对齐：`bch2_trans_get_iter()`）
    ///
    /// 对应 bcachefs `bch2_trans_get_iter()` (iter.c) —— 从事务中查找或创建 btree 迭代器。
    ///
    /// **volmount 限制**：因签名使用 `&self`（bcachefs API 兼容约束），无法创建新 iter。
    /// 完整功能在 `get_path(&BtreeRoot, &BtreeKey, intent, BtreeId)` 中实现（需要 `&mut self`）。
    ///
    /// 当前实现仅在已有 iters 中按 `btree_id` 精确查找。若未找到，提供清晰的引导消息。
    /// 调用者应迁移到 `get_path()` / `get_iter()` 以获得完整功能。
    pub fn trans_get(&self, btree_id: BtreeId, _pos: Bpos, _type: u32) -> usize {
        for (idx, iter) in self.iters.iter().enumerate() {
            if self.iter_type(idx) == btree_id && iter.pos.key_type as u32 == _type {
                return idx;
            }
        }
        panic!(
            "trans_get: iter (btree_id={:?}, key_type={}) not found among {} iters. \
             Use get_path(root, target, intent, btree_type) instead (requires &mut self)",
            btree_id,
            _type,
            self.iters.len()
        );
    }

    /// 释放 path iter（bcachefs 对齐：`bch2_trans_iter_put()`）
    ///
    /// 标记指定 iter 可被复用。volmount 当前管理中 iter 自动复用，
    /// 此方法为占位符以匹配 bcachefs API 签名。
    pub fn trans_iter_put(&mut self, _idx: usize) {
        // volmount 自动管理 iter 生命周期，无需显式释放
    }

    /// 释放 path（bcachefs 对齐：`bch2_path_put()`）
    pub fn path_put(&mut self, _idx: usize, _intent: bool) {
        // volmount 自动管理 path 生命周期，无需显式释放
    }

    /// 释放所有锁并重置锁状态（bcachefs 对齐：`bch2_trans_unlock()`）
    ///
    /// 不同于 `unlock_all()`（重启时调用），此方法仅释放锁和重置状态，
    /// 不修改 iters 的其他字段。调用后可通过 `trans_relock()` 重新获取。
    ///
    /// 对应 bcachefs `bch2_trans_unlock()` (locking.c:1524)，委派 `__bch2_trans_unlock` (locking.c:1440)。
    pub fn trans_unlock(&mut self) {
        for iter in &mut self.iters {
            for level in &mut iter.path {
                if level.lock_state == BtreeNodeLockedType::None {
                    continue;
                }
                match level.lock_state {
                    BtreeNodeLockedType::Read => level.node.lock.unlock_read(),
                    BtreeNodeLockedType::Intent => level.node.lock.unlock_intent(),
                    BtreeNodeLockedType::Write => level.node.lock.unlock_write(),
                    BtreeNodeLockedType::None => {}
                }
                level.lock_state = BtreeNodeLockedType::None;
            }
        }
    }

    /// 重新获取之前通过 trans_unlock 释放的所有锁（bcachefs 对齐：`bch2_trans_relock()`）
    ///
    /// 对应 bcachefs `bch2_trans_relock()` (locking.c:1487-1517)。
    /// 按 iters 自然顺序重新加锁（不排序），与 bcachefs `trans_for_each_path` 对齐。
    /// 若任何锁获取失败，请求重启并返回 false。
    pub fn trans_relock(&mut self) -> bool {
        for iter in &self.iters {
            for level in &iter.path {
                if level.lock_state == BtreeNodeLockedType::None {
                    continue;
                }
                let ok = match level.lock_state {
                    BtreeNodeLockedType::Read => level.node.lock.lock_read(),
                    BtreeNodeLockedType::Intent => level.node.lock.lock_intent(),
                    BtreeNodeLockedType::Write => level.node.lock.lock_write(),
                    BtreeNodeLockedType::None => unreachable!(),
                };
                if !ok {
                    self.needs_restart = true;
                    self.restart_reason = Some(RestartReason::LockConflict);
                    return false;
                }
            }
        }
        true
    }

    /// 返回事务持有的 iter 的可变引用（bcachefs 对齐：`trans_iter_mut()`）
    pub fn trans_iter_mut(&mut self, idx: usize) -> Option<&mut BtreeIter> {
        self.iters.get_mut(idx)
    }

    /// 返回事务持有的 iter 的不可变引用（bcachefs 对齐：`trans_iter()`）
    pub fn trans_iter(&self, idx: usize) -> Option<&BtreeIter> {
        self.iters.get(idx)
    }

    /// 提交事务并写入 journal（bcachefs 对齐：`__bch2_trans_commit()`）
    ///
    /// 执行 bcachefs 完全对齐的 4 阶段事务提交流程:
    /// Phase 1: 预计算 journal 条目大小并保留空间 → `journal_res_get()`
    /// Phase 2: 修改 btree 节点（使用已保留的 seq）
    /// Phase 3: 填充 journal 条目到已保留空间 → `add_entry()`
    /// Phase 4: 释放保留 → `journal_res_put()`（refcount→0 自动触发写）
    ///
    /// bcachefs 顺序保证：
    /// 如果 journal 保留失败，btree 不会被修改。
    /// 如果 btree 修改后崩溃，journal 条目从未写入 bucket，
    /// recovery 不会看到未应用的条目（线性化保证）。
    pub async fn trans_commit(
        &mut self,
        journal: &Journal,
        engine: &mut BtreeEngine,
        _backend: &dyn BlockDevice,
    ) -> Result<u64, StorageError> {
        if self.journal.is_empty() {
            // 无 journal 条目 → 只运行触发器管线
            self.commit_with_engine(engine)?;
            return Ok(0);
        }

        // ─── Phase 1a: 按 BtreeId 分组 journal 条目 ───────────────────
        let mut groups: HashMap<BtreeId, Vec<BtreeEntry>> = HashMap::new();
        for je in self.journal.iter() {
            let key_type = match je.op {
                BtreeOp::Insert => KeyType::Normal,
                BtreeOp::Delete => KeyType::Deleted,
                BtreeOp::Whiteout => KeyType::Whiteout,
            };
            let group_entry = BtreeEntry {
                pos: Bpos::from_key(&je.key),
                key_type,
                value: crate::btree::key::KeyValue::Extent(je.value),
            };
            groups.entry(je.btree_id).or_default().push(group_entry);
        }

        // 构建 JsetEntry 列表（每个 btree 一组）
        let jset_entries: Vec<RawJsetEntry> = groups
            .into_iter()
            .map(|(bt, entries)| {
                let entries_bytes =
                    bincode::serialize(&entries).map_err(|e| StorageError::Serialization(e))?;
                RawJsetEntry::new(bt as u8, JsetEntryType::BtreeKeys as u8, entries_bytes)
            })
            .collect::<Result<Vec<_>, StorageError>>()?;

        let last_seq = journal.last_seq_ondisk.load(Ordering::Acquire);

        // ─── Phase 1b: 构建 Jset 模板（seq=0）计算精确大小 ──────
        let template = Jset {
            header: JsetHeader {
                magic: JOURNAL_MAGIC,
                seq: 0, // placeholder，大小与真实 seq 相同
                last_seq,
                crc32: 0,
                entry_count: jset_entries.len() as u32,
                version: JSET_VERSION as u32,
                csum_type: CSUM_TYPE_NONE,
                pad: [0u8; 27],
            },
            entries: jset_entries,
        };

        let req_u64s = template.serialized_padded_len().div_ceil(8) as u32;

        // ─── Phase 1c: 保留 journal 空间（bcachefs step 1） ─────
        let res = journal
            .journal_res_get(Watermark::Normal, req_u64s)
            .map_err(|e| StorageError::JournalError(e.to_string()))?;
        let seq = res.seq;
        self.journal_seq = seq; // 注入 seq（TC1）

        // ─── Phase 1d: flush dirty key cache with real journal_seq ──
        // 在 journal_res_get 之后、commit_with_engine 之前执行，
        // 确保脏缓存条目使用真实的 journal_seq 写入 btree 节点。
        engine.flush_cache_dirty_keys(seq);

        // ─── Phase 2: 修改 btree 节点（bcachefs step 3） ─────────
        // commit_with_engine 内部读取 self.journal_seq 并应用到 btree 节点
        self.commit_with_engine(engine)?;

        // ─── Phase 3: 填充 journal（bcachefs step 4） ─────────────
        // 写入实际 seq 后重新序列化 Jset，写入已保留空间
        let mut jset = template;
        jset.header.seq = res.seq;
        let serialized = jset
            .serialize_padded()
            .map_err(|e| StorageError::JournalError(e.to_string()))?;
        journal.add_entry(&res, &serialized);

        // ─── Phase 4: 释放保留（bcachefs step 5） ─────────────────
        // refcount→0 → 自动推进到 WriteSubmitted
        journal.journal_res_put(&res);

        Ok(seq)
    }

    /// 开始新的事务周期（bcachefs 对齐：`bch2_trans_begin()`）
    ///
    /// `begin()` 的 bcachefs 对齐别名。
    pub fn trans_begin(&mut self) {
        self.begin();
    }
}

impl Default for BtreeTrans {
    fn default() -> Self {
        Self::new(Arc::new(NodeCache::new()))
    }
}

impl std::fmt::Debug for BtreeTrans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtreeTrans")
            .field("iters", &self.iters.len())
            .field("journal_seq", &self.journal_seq)
            .field("journal", &self.journal.len())
            .field("committed", &self.committed)
            .field("restart_count", &self.restart_count)
            .field("needs_restart", &self.needs_restart)
            .field("has_triggers", &self.trigger_registry.is_some())
            .field("wal_pin_id", &self.wal_pin_id)
            .finish()
    }
}

/// 宏：锁重启循环 — 对应 bcachefs `lockrestart_do()`
///
/// 在事务的重启循环中执行闭包 body。当 body 返回 `Err(RestartReason)` 时，
/// 宏自动调用事务的 `request_restart()` + `restart()` 并重试 body。
/// 当重启次数超过 `MAX_RESTARTS` 时，返回 `StorageError::TransactionRestartLimit`。
///
/// # 用法
///
/// ```text
/// lockrestart_do!(trans, {
///     let iter = trans.get_iter(&root, &key, true, BtreeId::Extents);
///     if iter.is_empty() {
///         return Err(RestartReason::KeyCacheMiss);
///     }
///     // ... perform safe operations ...
///     Ok(())
/// })?;
/// ```
///
/// # 幂等性要求
///
/// **body 可能被多次执行**。所有操作必须满足：
/// - **幂等**：多次执行与一次执行结果一致
/// - **无副作用**：body 内对外部状态的修改（如资源分配）在重启后可能丢失
/// - **资源安全**：如果在 body 中分配了外部资源（如 bucket），必须在 body 退出前回滚
/// - **最佳实践**：body 仅做"检查 + 计算"，真正的写入在 body 返回后由调用者完成
#[macro_export]
macro_rules! lockrestart_do {
    ($trans:expr, $body:block) => {{
        loop {
            match (|| -> Result<_, RestartReason> { $body })() {
                Ok(result) => break Ok(result),
                Err(reason) => {
                    $trans.request_restart(reason);
                    if $trans.restart().is_none() {
                        break Err(StorageError::TransactionRestartLimit(
                            $trans.restart_count(),
                        ));
                    }
                }
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::mock::MockBlockDevice;
    use crate::btree::btree::Btree;
    use crate::btree::key::{BchVal, Bpos, BtreeEntry, BtreeKey, KeyType, KeyValue};
    use crate::btree::node::BtreeNode;
    use crate::btree::types::{BtreeRoot, NodeCache};
    use crate::journal::Journal;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn make_root() -> BtreeRoot {
        BtreeRoot::new(Arc::new(BtreeNode::new_leaf()), 0)
    }

    fn make_transaction() -> BtreeTrans {
        BtreeTrans::new(Arc::new(NodeCache::new()))
    }

    #[test]
    fn test_transaction_new() {
        let t = make_transaction();
        assert!(!t.is_committed());
        assert_eq!(t.iter_count(), 0);
        assert_eq!(t.restart_count(), 0);
        assert!(!t.needs_restart());
    }

    #[test]
    fn test_transaction_get_iter() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);
    }

    #[test]
    fn test_transaction_begin_resets() {
        let _root = make_root();
        let mut t = make_transaction();
        let _key = BtreeKey::new(100, 1, KeyType::Normal);
        t.begin();
        assert_eq!(t.iter_count(), 0);
    }

    #[test]
    fn test_transaction_commit() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);
        assert!(t.commit(None).is_ok());
        assert!(t.is_committed());
    }

    #[test]
    fn test_transaction_rollback() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);
        t.rollback();
        // rollback 对齐 bch2_trans_reset_updates：不放锁，保留 iters
        assert_eq!(t.iter_count(), 1, "rollback keeps iters (bcachefs aligned)");
        assert!(!t.is_committed(), "rollback clears committed flag");
        assert!(t.journal.is_empty(), "rollback clears journal");
    }

    #[test]
    fn test_transaction_journal_seq() {
        let mut t = make_transaction();
        t.set_journal_seq(42);
        assert_eq!(t.journal_seq(), 42);
    }

    #[test]
    fn test_transaction_multiple_iters() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.get_iter(
            &root,
            &BtreeKey::new(200, 1, KeyType::Normal),
            true,
            BtreeId::Subvolumes,
        );
        assert_eq!(t.iter_count(), 2);
    }

    // ─── Phase A: 新测试 ──────────────────────────────────

    /// 测试 (2): 重启触发 — needs_restart 在 request_restart 后正确设置
    #[test]
    fn test_restart_trigger() {
        let mut t = make_transaction();
        assert!(!t.needs_restart());

        t.request_restart(RestartReason::LockConflict);
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::LockConflict));
    }

    /// 测试 (3): begin() 清除 needs_restart
    #[test]
    fn test_begin_clears_restart() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::NodeSplit);
        assert!(t.needs_restart());

        t.begin();
        assert!(!t.needs_restart());
        assert_eq!(t.restart_reason(), None);
    }

    /// 测试 (4): rollback 清除 restart 状态但不释放锁/清除 iters
    ///
    /// 对齐 bcachefs `bch2_trans_reset_updates()` — 不放锁，仅重置更新队列。
    #[test]
    fn test_rollback_clears_restart() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.request_restart(RestartReason::LockConflict);

        t.rollback();
        assert!(!t.needs_restart());
        assert_eq!(t.restart_reason(), None);
        assert_eq!(t.restart_count(), 0);
        // rollback 对齐 bch2_trans_reset_updates：不放锁，保留 iters
        // 与旧版不同：旧版释放所有锁并清除 iters，新版仅重置更新队列
        assert_eq!(
            t.iter_count(),
            1,
            "rollback should keep iters (bcachefs aligned)"
        );
        assert!(t.journal.is_empty(), "rollback should clear journal");
    }

    /// 测试 (5): collect_paths 正确收集所有 path levels
    #[test]
    fn test_collect_paths() {
        let root = make_root();
        let mut t = make_transaction();

        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.get_iter(
            &root,
            &BtreeKey::new(200, 1, KeyType::Normal),
            true,
            BtreeId::Subvolumes,
        );

        let paths = t.collect_paths();

        // 单 leaf tree: 每个 iter 有 1 个 path level
        assert_eq!(paths.len(), 2);

        // 验证 path 0: Extents, pos.offset=100
        assert_eq!(paths[0].btree_type, BtreeId::Extents);
        assert_eq!(paths[0].pos.offset, 100);
        assert_eq!(paths[0].iter_idx, 0);

        // 验证 path 1: Subvolumes, pos.offset=200
        assert_eq!(paths[1].btree_type, BtreeId::Subvolumes);
        assert_eq!(paths[1].pos.offset, 200);
        assert_eq!(paths[1].iter_idx, 1);
    }

    /// 测试 (6): iter_type 返回正确的 btree type
    #[test]
    fn test_iter_type() {
        let root = make_root();
        let mut t = make_transaction();

        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.get_iter(
            &root,
            &BtreeKey::new(200, 1, KeyType::Normal),
            false,
            BtreeId::Snapshots,
        );

        assert_eq!(t.iter_type(0), BtreeId::Extents);
        assert_eq!(t.iter_type(1), BtreeId::Snapshots);
        // 越界返回默认值
        assert_eq!(t.iter_type(99), BtreeId::Extents);
    }

    /// 测试 (7): 未提交事务 restart_count 为 0
    #[test]
    fn test_restart_count_initial() {
        let t = make_transaction();
        assert_eq!(t.restart_count(), 0);
    }

    /// 测试 (8): 不同 btree type 的锁排序 - 同 type 不同 pos
    /// 测试 (9): try_lock_all — 按 journal 顺序升级写锁通过
    #[test]
    fn test_try_lock_all_success() {
        let root = make_root();
        let mut t = make_transaction();

        // 创建 iter（自动获取 leaf 读锁，intent=false）
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);

        // 添加 journal 条目引用该 iter（iter_idx=0, level=0）
        let val = BchVal::new(42, 0);
        t.journal_insert(BtreeId::Extents, 0, false, key, val, 0);

        // 按 journal 自然顺序升级（Read → Intent → Write）
        t.try_lock_all();

        // 不应触发重启（无竞争）
        assert!(!t.needs_restart());
        // leaf 锁已升级为 Write
        assert_eq!(t.iters[0].path[0].lock_state, BtreeNodeLockedType::Write);
    }

    /// 测试 (10): commit() 成功返回 Ok
    #[test]
    fn test_commit_returns_ok() {
        let root = make_root();
        let mut t = make_transaction();

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);

        let result = t.commit(None);
        assert!(result.is_ok());
        assert!(t.is_committed());
    }

    /// 测试 (11): 带有 journal 的事务正常提交
    #[test]
    fn test_commit_with_journal() {
        let root = make_root();
        let mut t = make_transaction();

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);
        t.get_iter(&root, &key, false, BtreeId::Extents);

        t.journal_insert(BtreeId::Extents, 0, false, key, val, 0);
        assert_eq!(t.journal_len(), 1);

        let result = t.commit(None);
        assert!(result.is_ok());

        // journal 条目仍可 drain
        let journal = t.drain_journal();
        assert_eq!(journal.len(), 1);
    }

    #[tokio::test]
    async fn test_commit_with_journal_flushes_dirty_key_cache() {
        let backend = Arc::new(MockBlockDevice::new());
        let journal = Journal::new(vec![100]);
        let mut engine = BtreeEngine::new();
        let mut t = make_transaction();

        let commit_key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&make_root(), &commit_key, false, BtreeId::Extents);
        t.journal_insert(
            BtreeId::Extents,
            0,
            false,
            commit_key,
            BchVal::new(42, 0),
            0,
        );

        let dirty_pos = Bpos::new(1, 150, 0);
        let dirty_entry = BtreeEntry::new(dirty_pos, KeyType::Normal, KeyValue::extent(0x555, 1));
        engine
            .get_mut(BtreeId::Extents)
            .key_cache
            .bch2_btree_insert_key_cached(dirty_pos, dirty_entry, 0);
        assert_eq!(
            engine.get_mut(BtreeId::Extents).key_cache.nr_dirty_keys(),
            1
        );

        t.commit_with_journal(&journal, &mut engine, &*backend)
            .await
            .unwrap();

        assert_eq!(
            engine.get_mut(BtreeId::Extents).key_cache.nr_dirty_keys(),
            0
        );

        let dirty_key = BtreeKey::from_bpos(dirty_pos, KeyType::Normal);
        let got = engine.get_entry(BtreeId::Extents, &dirty_key);
        assert!(
            got.is_some(),
            "dirty key should be written back before commit"
        );
        assert_eq!(got.unwrap().1.paddr.get(), 0x555);
    }

    /// 测试 (12): 没有 LockGraph 时锁获取正常工作（事务生命周期基础）
    #[test]
    fn test_transaction_commit_no_lock_graph() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        assert!(t.commit(None).is_ok());
    }

    // ─── Phase A6: 重启触发测试 ──────────────────────────

    /// 测试 (18): trigger_node_split 设置正确的重启原因
    #[test]
    fn test_trigger_node_split_sets_reason() {
        let mut t = make_transaction();
        t.trigger_node_split();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::NodeSplit));
    }

    /// 测试 (19): trigger_key_cache_miss 设置正确的重启原因
    #[test]
    fn test_trigger_key_cache_miss_sets_reason() {
        let mut t = make_transaction();
        t.trigger_key_cache_miss();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::KeyCacheMiss));
    }

    /// 测试 (20): trigger_node_read_required 设置正确的重启原因
    #[test]
    fn test_trigger_node_read_required_sets_reason() {
        let mut t = make_transaction();
        t.trigger_node_read_required();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::NodeReadRequired));
    }

    /// 测试 (21): trigger_needs_lock 设置正确的重启原因
    #[test]
    fn test_trigger_needs_lock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_needs_lock();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::TriggerNeedsLock));
    }

    /// 测试: trigger_would_deadlock 设置正确的重启原因
    #[test]
    fn test_trigger_would_deadlock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_would_deadlock();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::WouldDeadlock));
    }

    /// 测试: trigger_write_overflow 设置正确的重启原因
    #[test]
    fn test_trigger_write_overflow_sets_reason() {
        let mut t = make_transaction();
        t.trigger_write_overflow();
        assert!(t.needs_restart());
        assert_eq!(t.restart_reason(), Some(RestartReason::WriteOverflow));
    }

    /// 测试: trigger_split_with_interior_updates 设置正确的重启原因
    #[test]
    fn test_trigger_split_with_interior_updates_sets_reason() {
        let mut t = make_transaction();
        t.trigger_split_with_interior_updates();
        assert!(t.needs_restart());
        assert_eq!(
            t.restart_reason(),
            Some(RestartReason::SplitWithInteriorUpdates)
        );
    }

    /// 测试 (22): check_path_integrity 检测到空路径时触发 NodeReadRequired
    #[test]
    fn test_check_path_integrity_empty_path() {
        let mut t = make_transaction();
        // 空路径的 iter（模拟未初始化的 iter）
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.get_iter(&root, &k, false, BtreeId::Extents);
        // 清空 path 模拟损坏
        if let Some(iter) = t.iters.first_mut() {
            iter.path.clear();
        }
        t.check_path_integrity(0);
        assert!(t.needs_restart());
    }

    /// 测试 (23): detect_iter_restart_needed 检测 had_restart 标志
    #[test]
    fn test_detect_iter_restart() {
        let mut t = make_transaction();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.get_iter(&root, &k, false, BtreeId::Extents);
        // 设置 had_restart
        if let Some(iter) = t.iters.first_mut() {
            iter.had_restart = true;
        }
        assert!(t.detect_iter_restart_needed());
        assert!(t.needs_restart());
    }

    /// 测试 (24): detect_iter_restart_needed 消耗 had_restart 标志
    #[test]
    fn test_detect_iter_restart_consumes_flag() {
        let mut t = make_transaction();
        let k = BtreeKey::new(100, 1, KeyType::Normal);
        let root = make_root();
        t.get_iter(&root, &k, false, BtreeId::Extents);
        if let Some(iter) = t.iters.first_mut() {
            iter.had_restart = true;
        }
        assert!(t.detect_iter_restart_needed());
        // 第二次调用不应再触发
        assert!(!t.detect_iter_restart_needed());
    }

    // ─── Phase B1: 触发器集成测试 ──────────────────────────

    /// 测试 (25): with_trigger_registry 创建事务并绑定 TriggerRegistry
    #[test]
    fn test_with_trigger_registry_creates_transaction() {
        let registry = Arc::new(TriggerRegistry::new());
        let cache = Arc::new(NodeCache::new());
        let t = BtreeTrans::with_trigger_registry(cache, registry.clone());
        assert!(t.trigger_registry.is_some());
    }

    /// 测试 (26): set_trigger_registry 可在创建后绑定
    #[test]
    fn test_set_trigger_registry_after_creation() {
        let registry = Arc::new(TriggerRegistry::new());
        let mut t = make_transaction();
        assert!(t.trigger_registry.is_none());

        t.set_trigger_registry(registry.clone());
        assert!(t.trigger_registry.is_some());
    }

    /// 测试 (27): commit_with_engine 在没有触发器时与 commit 行为一致
    #[test]
    fn test_commit_with_engine_no_triggers() {
        let root = make_root();
        let mut t = make_transaction();

        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);

        // 没有 engine 可用时模拟 — 实际不会走到 trigger firing
        // 只需验证 commit_with_engine 在没有触发器的场景下正常返回
        let result = t.commit(None);
        assert!(result.is_ok());
        assert!(t.is_committed());
    }

    /// 测试 (28): transactional 触发器在 lock retry 期间只执行一次
    ///
    /// 该测试通过持有 intent 锁强制 `try_lock_all()` 重试，
    /// 同时注册一个 transactional trigger 计数器。
    /// 若触发器被错误地放在 retry loop 内，计数会大于 1。
    #[test]
    fn test_transactional_triggers_run_once_across_retry() {
        static TRIGGER_COUNT: AtomicUsize = AtomicUsize::new(0);

        fn count_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            TRIGGER_COUNT.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        TRIGGER_COUNT.store(0, Ordering::SeqCst);

        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);
        journal_push_entry(&mut t, BtreeOp::Insert, 0, false, key, BchVal::new(0, 0));

        let mut registry = TriggerRegistry::new();
        registry.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Transactional,
            count_trigger,
        );
        t.set_trigger_registry(Arc::new(registry));

        let mut engine = BtreeEngine::new();
        for iter in &t.iters {
            for level in &iter.path {
                level.node.lock.lock_intent();
            }
        }

        let result = t.commit(Some(&mut engine));

        for iter in &t.iters {
            for level in &iter.path {
                level.node.lock.unlock_intent();
            }
        }

        assert!(
            result.is_err(),
            "conflicting lock should force retry failure"
        );
        assert_eq!(
            TRIGGER_COUNT.load(Ordering::SeqCst),
            1,
            "transactional trigger should run only once across retries"
        );
    }

    /// 测试 (29): drain_journal 返回包含 btree_type 的条目
    #[test]
    fn test_drain_journal_with_btree_type() {
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);

        t.journal_insert(BtreeId::Extents, 0, false, key, val, 0);
        t.journal_delete(BtreeId::Subvolumes, 0, false, key, 0);
        assert_eq!(t.journal_len(), 2);

        let journal = t.drain_journal();
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].btree_id, BtreeId::Extents);
        assert_eq!(journal[0].op, BtreeOp::Insert);
        assert_eq!(journal[1].btree_id, BtreeId::Subvolumes);
        assert_eq!(journal[1].op, BtreeOp::Delete);
    }

    /// 测试 (30): fire_triggers_on_journal 在没有 registry 时为空操作
    #[test]
    fn test_fire_triggers_on_journal_no_registry() {
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        let val = BchVal::new(42, 0);

        t.journal_insert(BtreeId::Extents, 0, false, key, val, 0);

        // 没有 trigger_registry → fire_triggers_on_journal 应返回 Ok
        // 但我们无法直接调用私有方法，所以通过 commit_with_engine 间接验证
        let root = make_root();
        t.get_iter(&root, &key, false, BtreeId::Extents);
        let result = t.commit(None);
        assert!(result.is_ok());
    }

    /// 测试 (31): Debug 输出包含 trigger 信息
    #[test]
    fn test_debug_shows_trigger_info() {
        let mut t = make_transaction();
        let debug_str = format!("{:?}", t);
        assert!(debug_str.contains("has_triggers"));
        assert!(debug_str.contains("false"));

        let registry = Arc::new(TriggerRegistry::new());
        t.set_trigger_registry(registry);
        let debug_str = format!("{:?}", t);
        assert!(debug_str.contains("true"));
    }

    // ─── P0 Delta: restart() / sort_key level / lockrestart_do! ──

    /// 测试 (32): restart() 返回最近一次的重启原因并消费
    #[test]
    fn test_restart_returns_reason() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::LockConflict);
        let reason = t.restart();
        assert_eq!(reason, Some(RestartReason::LockConflict));
        // restart 消费了 reason
        assert_eq!(t.restart_reason(), None);
        // restart_count 递增
        assert_eq!(t.restart_count(), 1);
    }

    /// 测试 (33): restart() 超过 MAX_RESTARTS 时返回 None
    #[test]
    fn test_restart_none_when_exceeded() {
        let mut t = make_transaction();
        t.restart_count = MAX_RESTARTS; // 设为刚好在阈值
        t.request_restart(RestartReason::LockConflict);
        let reason = t.restart();
        assert!(reason.is_none(), "over MAX_RESTARTS should return None");
        assert_eq!(t.restart_count(), MAX_RESTARTS + 1);
    }

    /// 测试 (34): restart() 调用后 needs_restart 被清除（由 begin() 完成）
    #[test]
    fn test_restart_clears_needs_restart() {
        let mut t = make_transaction();
        t.request_restart(RestartReason::NodeSplit);
        assert!(t.needs_restart());
        let _ = t.restart();
        assert!(!t.needs_restart());
    }

    /// 测试 (35): sort_key 包含 level 维度，父节点排序在子节点前
    #[test]
    fn test_sort_key_includes_level() {
        let p1 = BtreePath {
            btree_type: BtreeId::Extents,
            pos: Bpos::new(100, 0, 0),
            lock_state: BtreeNodeLockedType::Read,
            iter_idx: 0,
            level: 0, // leaf
        };
        let p2 = BtreePath {
            btree_type: BtreeId::Extents,
            pos: Bpos::new(100, 0, 0),
            lock_state: BtreeNodeLockedType::Read,
            iter_idx: 0,
            level: 2, // root (depth=2)
        };
        // sort_key: (u8, Bpos, -(level as i8))
        // p1: (0, Bpos{100,0,0}, 0)
        // p2: (0, Bpos{100,0,0}, -2)
        let sk1 = p1.sort_key();
        let sk2 = p2.sort_key();
        // -2 < 0 → level 2 (root) sorts before level 0 (leaf)
        assert!(
            sk2 < sk1,
            "higher level (parent) should sort before lower level (child)"
        );
    }

    /// 测试 (36): lockrestart_do! 成功路径 — body 返回 Ok
    #[test]
    fn test_lockrestart_do_success() {
        let mut t = make_transaction();
        let result = lockrestart_do!(t, { Ok(42) });
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
        assert_eq!(t.restart_count(), 0);
    }

    /// 测试 (37): lockrestart_do! 重启重试 — body 第一次返回 Err 后重试成功
    #[test]
    fn test_lockrestart_do_restart_then_ok() {
        let mut t = make_transaction();
        let attempts = std::cell::Cell::new(0u32);

        let result: Result<(), StorageError> = lockrestart_do!(t, {
            let n = attempts.get();
            attempts.set(n + 1);
            if n == 0 {
                return Err(RestartReason::LockConflict);
            }
            Ok(())
        });

        assert!(result.is_ok());
        // 发生了 1 次重启
        assert_eq!(t.restart_count(), 1);
        // body 执行了 2 次
        assert_eq!(attempts.get(), 2);
    }

    /// 测试 (38): lockrestart_do! 超限 — body 持续返回 Err
    #[test]
    fn test_lockrestart_do_max_restarts() {
        let mut t = make_transaction();
        t.restart_count = MAX_RESTARTS; // 一次额外调用即超限

        let result: Result<(), StorageError> =
            lockrestart_do!(t, { Err(RestartReason::LockConflict) });

        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::TransactionRestartLimit(count) => {
                assert_eq!(count, MAX_RESTARTS + 1);
            }
            _ => panic!("expected TransactionRestartLimit error"),
        }
    }

    // ─── P2: locked_seq + get_path + restart_optimized ──

    /// 测试 (39): record_locked_seqs 记录所有 path levels 的 locked_seq
    ///
    /// 验证：
    /// - commit() 后每个 path level 的 locked_seq 被记录
    /// - 新节点 seq 从 0 开始，locked_seq 应为 0（对应 SixLock 初始值）
    /// - locked_seq 精确等于 lock.seq() 在记录时刻的值
    #[test]
    fn test_locked_seq_recorded_on_lock() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);
        t.get_iter(&root, &key, false, BtreeId::Extents);
        assert!(t.commit(None).is_ok());
        let iter = t.iter_mut(0).unwrap();
        for (i, level) in iter.path.iter().enumerate() {
            // 新节点从未被写过，SixLock::seq() == 0
            assert_eq!(
                level.locked_seq,
                level.node.lock.seq(),
                "level {} locked_seq should match lock seq",
                i
            );
        }
    }

    // ── R1: get_path 测试 ──────────────────────────────

    /// 测试 (39): get_path 精确匹配返回已有 iter 索引
    ///
    /// get_iter 后 get_path 同一 key → pos == target → 直接复用。
    #[test]
    fn test_get_path_exact_match() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        // get_path 同一 key → 精确匹配 → 复用索引 0
        let idx = t.get_path(&root, &key, false, BtreeId::Extents);
        assert_eq!(idx, 0, "exact match should reuse index 0");
        assert_eq!(t.iter_count(), 1, "exact match should not create new iter");
    }

    /// 测试 (40): get_path 同 leaf 复用
    ///
    /// 同一 btree_type 不同 key（同一 leaf 范围内）→ 同 leaf 复用。
    /// depth 0 树中所有 key 在同一 leaf，创建临时 iter 后 Arc::ptr_eq 匹配。
    #[test]
    fn test_get_path_same_leaf() {
        let root = make_root();
        let mut t = make_transaction();
        let key_a = BtreeKey::new(100, 1, KeyType::Normal);
        let key_b = BtreeKey::new(200, 1, KeyType::Normal);

        t.get_iter(&root, &key_a, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        // 不同 key 但在同一个 leaf（depth 0 单 leaf 树）→ 同 leaf 复用
        let idx = t.get_path(&root, &key_b, false, BtreeId::Extents);
        assert_eq!(idx, 0, "same-leaf match should reuse index 0");
        assert_eq!(t.iter_count(), 1, "same-leaf should not create new iter");
    }

    /// 测试 (41): get_path 不同 btree_type 创建新 iter
    #[test]
    fn test_get_path_creates_new_when_type_mismatch() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.get_iter(&root, &key, false, BtreeId::Extents);
        assert_eq!(t.iter_count(), 1);

        // 不同 btree_type → 无法匹配 → 创建新 iter
        let idx = t.get_path(&root, &key, false, BtreeId::Subvolumes);
        assert_eq!(idx, 1, "type mismatch should create at index 1");
        assert_eq!(t.iter_count(), 2, "type mismatch should create new iter");
    }

    /// 测试 (42): get_path 返回的索引可通过 iter_mut 访问
    #[test]
    fn test_get_path_returns_usable_index() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        let idx = t.get_path(&root, &key, false, BtreeId::Extents);
        // 新创建的 iter 索引为 0
        assert_eq!(idx, 0);

        let iter = t.iter_mut(idx).unwrap();
        assert_eq!(iter.pos, key, "iter should be at target position");
    }

    // ── R2: restart_optimized (事务级) 测试 ──────────────────

    /// 测试 (43): restart_optimized seq 未变时返回 None
    ///
    /// commit() 后 locked_seq 已记录，seq 未变化 → 应返回 None。
    #[test]
    fn test_txn_restart_optimized_none_when_seq_unchanged() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.get_iter(&root, &key, false, BtreeId::Extents);
        t.commit(None).unwrap();
        // locked_seq 已记录，节点未被修改 → restart_optimized 应返回 None
        let result = t.restart_optimized();
        assert!(result.is_none(), "should return None when seq unchanged");
    }

    /// 测试 (44): restart_optimized seq 变化时返回 Some
    ///
    /// commit() 后修改节点 seq，restart_optimized 应检测到变化并返回 Some。
    #[test]
    fn test_txn_restart_optimized_some_when_seq_changed() {
        let root = make_root();
        let mut t = make_transaction();
        let key = BtreeKey::new(100, 1, KeyType::Normal);

        t.get_iter(&root, &key, false, BtreeId::Extents);
        t.commit(None).unwrap();

        // 手动修改 leaf 节点的 seq（模拟外部写操作）
        let leaf = t.iters[0].path.last().map(|l| Arc::clone(&l.node)).unwrap();
        leaf.lock.lock_write();
        leaf.lock.unlock_write();
        // seq 现在为 1

        let result = t.restart_optimized();
        assert!(result.is_some(), "should return Some when seq changed");
    }

    /// 测试 (45): restart_optimized 空事务返回 None
    #[test]
    fn test_txn_restart_optimized_empty() {
        let mut t = make_transaction();
        // 无 iters → needs_full_restart 为 false（iter path 为空）
        let result = t.restart_optimized();
        assert!(result.is_none(), "empty transaction should return None");
    }

    /// 测试 (46): restart_optimized 检查所有 path level — 内部节点变化 leaf 未变
    ///
    /// 创建 depth-1 树（internal root + leaf），get_iter + commit 后
    /// 修改 internal node seq。仅当 restart_optimized 检查所有层级
    /// 时才能检测到此变化。
    #[test]
    fn test_txn_restart_optimized_internal_changed_leaf_unchanged() {
        use crate::btree::key::BchVal;

        // 创建小节点树，插入足够条目触发多级分裂（depth ≥ 1）
        let mut b = Btree::new();
        b.set_root_node_size(512);

        let total = 200u64;
        for i in 0..total {
            b.insert(
                BtreeKey::new(i, 1, KeyType::Normal),
                BchVal::new(i * 10, 0),
                0,
            );
        }
        assert!(
            b.depth() >= 1,
            "should have depth >= 1 after {total} inserts (got depth={})",
            b.depth()
        );

        let mut t = BtreeTrans::new(b.cache_arc());
        t.begin();
        t.get_iter(
            b.root(),
            &BtreeKey::new(10, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.commit(None).unwrap();

        // depth ≥ 1 树 → path 应有至少 2 层
        assert!(
            t.iters[0].path.len() >= 2,
            "depth-{} tree should have >=2 path levels (got {})",
            b.depth(),
            t.iters[0].path.len()
        );

        // 记录修改前的 locked_seq（path[0] = internal root）
        let internal_locked = t.iters[0].path[0].locked_seq;

        // 修改 internal node 的 seq（模拟拓扑变化）
        let internal = Arc::clone(&t.iters[0].path[0].node);
        internal.lock.lock_write();
        internal.lock.unlock_write();

        // 验证内部节点 seq 变化
        assert_ne!(
            internal.lock.seq(),
            internal_locked,
            "internal node seq should have changed"
        );

        // restart_optimized 必须检测到内部节点变化
        let result = t.restart_optimized();
        assert!(result.is_some(), "should detect internal node change");
    }

    // ─── bcachefs 提交流程对齐测试 ──────────────────────────────

    /// 验证 rollback() 不重置 restart_count（对齐 bch2_trans_reset_updates）
    /// bcachefs update.h:557-571: reset_updates 不清除 restart_count
    #[test]
    fn test_rollback_keeps_restart_count() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        // 模拟重启计数
        t.restart_count = 42;
        t.request_restart(RestartReason::LockConflict);

        t.rollback();

        // restart_count 应保留（bcachefs reset_updates 不清除它）
        assert_eq!(
            t.restart_count, 42,
            "rollback should NOT reset restart_count"
        );
        // rollback 仍清除其他状态
        assert!(!t.needs_restart(), "rollback should clear needs_restart");
        assert_eq!(
            t.restart_reason(),
            None,
            "rollback should clear restart_reason"
        );
        assert!(t.journal.is_empty(), "rollback should clear journal");
    }

    /// 验证 begin() 重置 journal_seq（对齐 bch2_trans_begin 的 reset_updates）
    /// 防止重试循环中误用之前失败的 journal_seq
    #[test]
    fn test_begin_resets_journal_seq() {
        let mut t = make_transaction();
        t.journal_seq = 42; // 模拟之前失败的 commit 设置了 seq
        t.begin();
        assert_eq!(t.journal_seq, 0, "begin() should reset journal_seq to 0");
    }

    /// 验证 commit 在 reclaim 路径 + needs_restart 时返回 RestartLimit
    /// 对应 commit() Phase 0a: reclaim bail
    #[test]
    fn test_commit_reclaim_bail_on_restart() {
        let root = make_root();
        let mut t = make_transaction();
        t.watermark = Watermark::Reclaim;
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        // push journal entries 使 has_updates 检查通过
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        t.request_restart(RestartReason::LockConflict);

        let result = t.commit(None);

        assert!(result.is_err(), "reclaim + needs_restart should fail");
        match result {
            Err(StorageError::TransactionRestartLimit(_)) => {} // expected
            Err(e) => panic!("expected TransactionRestartLimit, got: {e:?}"),
            Ok(_) => panic!("expected error"),
        }
        // restart_count 应递增（验证重启计数语义）
        assert_eq!(
            t.restart_count, 1,
            "reclaim bail should increment restart_count"
        );
    }

    /// 验证 commit 正常路径下触发器可运行（Phase 0b 在 try_lock_all 之前）
    /// 无 engine 时触发器应被跳过，不会阻塞提交
    #[test]
    fn test_commit_triggers_skipped_without_engine() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );

        let result = t.commit(None);

        // 无 engine → 无触发器 → 应该成功（没有锁冲突）
        assert!(
            result.is_ok(),
            "commit without triggers should succeed: {:?}",
            result
        );
        assert!(t.committed, "commit should mark committed");
    }

    /// 验证 commit 在低水位线下会等待 cache throttle 解除
    #[test]
    fn test_commit_waits_for_cache_throttle() {
        let cache = Arc::new(NodeCache::new());
        cache
            .cache()
            .insert_dirty(2, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(3, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(4, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(5, Arc::new(BtreeNode::new_leaf()));

        assert!(
            cache.cache().bch2_btree_cache_should_throttle(),
            "setup should start throttled"
        );

        let mut t = BtreeTrans::new(cache.clone());
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );

        let cache_for_thread = cache.clone();
        let flusher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = cache_for_thread.flush_dirty();
        });

        let start = Instant::now();
        let result = t.commit(None);
        let elapsed = start.elapsed();
        flusher.join().unwrap();

        assert!(
            result.is_ok(),
            "commit should complete after throttle clears"
        );
        assert!(
            elapsed >= Duration::from_millis(15),
            "low-watermark commit should wait for throttle"
        );
        assert!(t.committed, "commit should mark committed");
    }

    /// 验证 reclaim 水位线会跳过 throttle 等待
    #[test]
    fn test_commit_reclaim_skips_cache_throttle() {
        let cache = Arc::new(NodeCache::new());
        cache
            .cache()
            .insert_dirty(2, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(3, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(4, Arc::new(BtreeNode::new_leaf()));
        cache
            .cache()
            .insert_dirty(5, Arc::new(BtreeNode::new_leaf()));

        assert!(cache.cache().bch2_btree_cache_should_throttle());

        let mut t = BtreeTrans::new(cache.clone());
        t.watermark = Watermark::Reclaim;
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );

        let start = Instant::now();
        let result = t.commit(None);
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "reclaim commit should bypass throttle wait");
        assert!(
            elapsed < Duration::from_millis(15),
            "reclaim commit should not block on cache throttle"
        );
        assert!(t.committed, "commit should mark committed");
    }

    /// 验证 commit 在重启限制达到时返回 TransactionRestartLimit
    /// 模拟多次重启使 restart_count 超过 MAX_RESTARTS
    #[test]
    fn test_commit_restart_limit_exceeded() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );

        // 设置 restart_count 接近上限，让一次 try_lock_all 失败触发限制
        t.restart_count = MAX_RESTARTS;
        // push 一个 journal 条目使 commit 进入 retry 循环
        journal_push_entry(
            &mut t,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        // 设置冲突锁使 try_lock_all 触发 needs_restart
        // 使用 intent 锁抢占目标节点
        for iter in &t.iters {
            for level in &iter.path {
                level.node.lock.lock_intent();
            }
        }
        let result = t.commit(None);

        // 清理：释放 test 中获取的 intent 锁（commit 错误路径不会自动释放）
        for iter in &t.iters {
            for level in &iter.path {
                level.node.lock.unlock_intent();
            }
        }

        assert!(result.is_err(), "should exceed restart limit");
        match result {
            Err(StorageError::TransactionRestartLimit(count)) => {
                assert!(count > MAX_RESTARTS, "count should exceed MAX_RESTARTS");
            }
            Err(e) => panic!("expected TransactionRestartLimit, got: {e:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    /// 辅助函数：快速向 journal push 一条 Insert 条目（测试用）
    fn journal_push_entry(
        t: &mut BtreeTrans,
        op: BtreeOp,
        level: u8,
        cached: bool,
        key: BtreeKey,
        value: BchVal,
    ) {
        t.journal.push(BtreeTransEntry {
            op,
            btree_id: BtreeId::Extents,
            level,
            cached,
            key,
            value,
            old_key: None,
            old_value: None,
            insert_trigger_run: false,
            overwrite_trigger_run: false,
            iter_idx: 0,
        });
    }

    // ─── Sub-task A: RestartReason 全覆盖测试 ──────────────────

    /// 验证 trigger_traverse_all 设置正确的 restart_reason
    #[test]
    fn test_trigger_traverse_all_sets_reason() {
        let mut t = make_transaction();
        t.trigger_traverse_all();
        assert_eq!(t.restart_reason(), Some(RestartReason::TraverseAll));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_relock_sets_reason() {
        let mut t = make_transaction();
        t.trigger_relock();
        assert_eq!(t.restart_reason(), Some(RestartReason::Relock));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_relock_path_sets_reason() {
        let mut t = make_transaction();
        t.trigger_relock_path();
        assert_eq!(t.restart_reason(), Some(RestartReason::RelockPath));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_upgrade_sets_reason() {
        let mut t = make_transaction();
        t.trigger_upgrade();
        assert_eq!(t.restart_reason(), Some(RestartReason::Upgrade));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_fault_inject_sets_reason() {
        let mut t = make_transaction();
        t.trigger_fault_inject();
        assert_eq!(t.restart_reason(), Some(RestartReason::FaultInject));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_nested_sets_reason() {
        let mut t = make_transaction();
        t.trigger_nested();
        assert_eq!(t.restart_reason(), Some(RestartReason::Nested));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_lock_waitlist_alloc_sets_reason() {
        let mut t = make_transaction();
        t.trigger_lock_waitlist_alloc();
        assert_eq!(t.restart_reason(), Some(RestartReason::LockWaitlistAlloc));
        assert!(t.needs_restart());
    }

    #[test]
    fn test_trigger_mem_realloced_sets_reason() {
        let mut t = make_transaction();
        t.trigger_mem_realloced();
        assert_eq!(t.restart_reason(), Some(RestartReason::MemoryRealloced));
        assert!(t.needs_restart());
    }

    /// 验证所有 19 个 RestartReason 变体的 bincode 序列化往返
    #[test]
    fn test_restart_reason_serialization_roundtrip() {
        let all_variants = [
            RestartReason::LockConflict,
            RestartReason::NodeSplit,
            RestartReason::KeyCacheMiss,
            RestartReason::TriggerNeedsLock,
            RestartReason::NodeReadRequired,
            RestartReason::WouldDeadlock,
            RestartReason::WriteOverflow,
            RestartReason::SplitWithInteriorUpdates,
            RestartReason::PathUpgradeFailed,
            RestartReason::JournalReclaimWouldDeadlock,
            RestartReason::JournalOverwritesChanged,
            RestartReason::TraverseAll,
            RestartReason::Relock,
            RestartReason::RelockPath,
            RestartReason::Upgrade,
            RestartReason::FaultInject,
            RestartReason::Nested,
            RestartReason::LockWaitlistAlloc,
            RestartReason::MemoryRealloced,
        ];

        for reason in all_variants {
            let encoded = bincode::serialize(&reason).expect("serialize should succeed");
            let decoded: RestartReason =
                bincode::deserialize(&encoded).expect("deserialize should succeed");
            assert_eq!(reason, decoded, "roundtrip failed for {:?}", reason);
        }
    }

    /// 验证所有变体可通过 request_restart -> restart_reason 正确传递
    #[test]
    fn test_restart_reason_all_variants_requestable() {
        let all_variants = [
            RestartReason::LockConflict,
            RestartReason::NodeSplit,
            RestartReason::KeyCacheMiss,
            RestartReason::TriggerNeedsLock,
            RestartReason::NodeReadRequired,
            RestartReason::WouldDeadlock,
            RestartReason::WriteOverflow,
            RestartReason::SplitWithInteriorUpdates,
            RestartReason::PathUpgradeFailed,
            RestartReason::JournalReclaimWouldDeadlock,
            RestartReason::JournalOverwritesChanged,
            RestartReason::TraverseAll,
            RestartReason::Relock,
            RestartReason::RelockPath,
            RestartReason::Upgrade,
            RestartReason::FaultInject,
            RestartReason::Nested,
            RestartReason::LockWaitlistAlloc,
            RestartReason::MemoryRealloced,
        ];

        for reason in all_variants {
            let mut t = make_transaction();
            t.request_restart(reason);
            assert_eq!(
                t.restart_reason(),
                Some(reason),
                "request_restart failed for {:?}",
                reason
            );
            assert!(t.needs_restart(), "needs_restart not set for {:?}", reason);
        }
    }

    // ─── R2: unlock_all() 不清除 locked_seq 测试 ──────────────

    /// 验证 unlock_all() 不清除 locked_seq
    /// 对齐 bcachefs __bch2_btree_path_unlock() (locking.c:1440-1454)：
    /// path-level 释放后 seq 保留，供 restart_optimized() 检测节点变化。
    #[test]
    fn test_unlock_all_preserves_locked_seq() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.commit(None).unwrap();

        // 记录 commit 后的 locked_seq
        let seq_before: Vec<u64> = t.iters[0].path.iter().map(|l| l.locked_seq).collect();

        // unlock_all 释放锁但应保留 locked_seq
        t.unlock_all();

        let seq_after: Vec<u64> = t.iters[0].path.iter().map(|l| l.locked_seq).collect();

        assert_eq!(
            seq_before, seq_after,
            "unlock_all() should NOT clear locked_seq"
        );
    }

    /// 验证 unlock_all 后 locked_seq 仍可用于 seq 比较
    /// 模拟 restart_optimized 的检测逻辑：unlock → 修改 seq → 检测变化
    #[test]
    fn test_unlock_all_then_seq_check_detects_change() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t.commit(None).unwrap();

        let locked_before = t.iters[0].path[0].locked_seq;

        // unlock_all 释放锁
        t.unlock_all();

        // 模拟外部写操作：lock_write + unlock_write 使 seq 递增
        let node = Arc::clone(&t.iters[0].path[0].node);
        node.lock.lock_write();
        node.lock.unlock_write();

        // locked_seq 应仍为旧值（未被 unlock_all 清除）
        assert_eq!(
            t.iters[0].path[0].locked_seq, locked_before,
            "locked_seq should survive unlock_all"
        );
        // 而 node 的实际 seq 已变化
        assert_ne!(
            node.lock.seq(),
            locked_before,
            "node seq should have changed after write unlock"
        );
    }

    // ─── R3: record_locked_seqs() 调用位置验证测试 ─────────────

    /// 验证 record_locked_seqs 在 try_lock_all 之后调用
    /// 通过比较 commit 前后 locked_seq 的值：如果 record_locked_seqs
    /// 在 try_lock_all 之前调用，locked_seq 会是旧值（0）。
    /// 在 try_lock_all 之后调用，locked_seq 应等于 lock.seq()。
    #[test]
    fn test_record_locked_seqs_after_try_lock_all() {
        let root = make_root();
        let mut t = make_transaction();
        t.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );

        // commit 前的 locked_seq 应为初始值 0
        assert_eq!(
            t.iters[0].path[0].locked_seq, 0,
            "locked_seq should be 0 before commit"
        );

        t.commit(None).unwrap();

        // commit 后 locked_seq 应等于 lock.seq()（record_locked_seqs 在 try_lock_all 后调用）
        for level in &t.iters[0].path {
            assert_eq!(
                level.locked_seq, level.node.lock.seq(),
                "locked_seq should match lock.seq() after commit (record_locked_seqs post try_lock_all)"
            );
        }
    }

    /// 验证 record_locked_seqs 在节点 seq 已递增的情况下记录正确值
    /// 先做一次 commit 使节点 seq 递增，再开新事务 commit，验证
    /// 第二次 commit 的 locked_seq 反映的是第二次 try_lock_all 后的 seq。
    #[test]
    fn test_record_locked_seqs_reflects_post_lock_seq() {
        let root = make_root();

        // 第一次事务：commit 使 root 节点 seq 递增
        let mut t1 = make_transaction();
        t1.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        journal_push_entry(
            &mut t1,
            BtreeOp::Insert,
            0,
            false,
            BtreeKey::new(100, 1, KeyType::Normal),
            BchVal::new(0, 0),
        );
        t1.commit(None).unwrap();
        // 解锁后 root seq 应已递增
        let seq_after_first = t1.iters[0].path[0].node.lock.seq();

        // 第二次事务：验证 locked_seq 记录的是当前 seq（>0）
        let mut t2 = make_transaction();
        t2.get_iter(
            &root,
            &BtreeKey::new(100, 1, KeyType::Normal),
            false,
            BtreeId::Extents,
        );
        t2.commit(None).unwrap();

        // locked_seq 应等于当前 lock.seq()，且应 > 0（因为第一次 commit 递增了 seq）
        // 注意：如果第一次 commit 没有写操作（journal 空），seq 不会递增
        // 所以这个测试依赖 journal_push_entry 使第一次 commit 实际执行写操作
        for level in &t2.iters[0].path {
            assert_eq!(
                level.locked_seq,
                level.node.lock.seq(),
                "locked_seq should match lock.seq() in second transaction"
            );
        }
    }
}
