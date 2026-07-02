//! TriggerRegistry — bcachefs 对齐的 btree 触发器系统
//!
//! 触发器是 btree 操作（insert/delete/whiteout）的副作用回调，用于维护
//! 跨 btree 的一致性（如 alloc 记帐、backpointer 更新等）。
//!
//! ## 三阶段执行（Phase B1）
//!
//! 受 bcachefs `bkey_ops.trigger` 三阶段设计启发：
//!
//! | 阶段 | 时机 | 失败处理 |
//! |------|------|---------|
//! | `Transactional` | 锁获取后、commit 前 | 可回滚：触发重启 |
//! | `Atomic` | commit 中 | 不可回滚：向调用者传播错误 |
//! | `Gc` | commit 完成后 | best-effort：错误仅日志记录 |
//!
//! ## 注册方式
//!
//! 触发器按 `(BtreeId, KeyType)` 注册。例如 alloc 记帐触发器：
//!
//! ```text
//! registry.register(
//!     BtreeId::Extents,
//!     KeyType::Normal as u8,
//!     TriggerPhase::Atomic,
//!     alloc_extent_trigger,
//! );
//! ```

use std::collections::HashMap;

use crate::btree::BtreeEngine;
use crate::btree::BtreeId;
use crate::StorageError;

/// 触发器执行阶段 — 对应 bcachefs 的 run_one_mem_trigger/trans_trigger/gc_trigger
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerPhase {
    /// 事务提交前执行 — 失败可触发重启（可回滚）
    Transactional,
    /// 事务提交中执行 — 失败向调用者传播错误（不可回滚）
    Atomic,
    /// GC 阶段延迟执行 — 失败仅日志记录（best-effort）
    Gc,
}

impl TriggerPhase {
    /// 所有阶段的有序列表（执行顺序）
    pub const ORDERED: [TriggerPhase; 3] = [
        TriggerPhase::Transactional,
        TriggerPhase::Atomic,
        TriggerPhase::Gc,
    ];

    /// 阶段在有序列表中的索引
    pub fn index(self) -> usize {
        match self {
            TriggerPhase::Transactional => 0,
            TriggerPhase::Atomic => 1,
            TriggerPhase::Gc => 2,
        }
    }
}

/// 触发器函数签名
///
/// 参数：
/// - `engine`: 可变的 BtreeEngine 引用（触发器可修改其他 btree 实例）
/// - `btree_type`: 触发操作发生的 btree 类型
/// - `key`: 操作 key 的序列化字节（bincode 格式的 BtreeKey）
/// - `old_val`: 操作前的值（None = 新插入）
/// - `new_val`: 操作后的值（None = 删除）
pub type TriggerFn = fn(
    engine: &mut BtreeEngine,
    btree_type: BtreeId,
    key: &[u8],
    old_val: Option<&[u8]>,
    new_val: Option<&[u8]>,
) -> Result<(), StorageError>;

/// 触发器注册表 — 按 `(BtreeId, KeyType)` 索引
///
/// 每个 `(BtreeId, KeyType)` 键对应一个或多个 `(TriggerPhase, TriggerFn)` 对。
/// 同阶段内多个触发器的执行顺序与注册顺序一致。
#[derive(Debug, Clone)]
pub struct TriggerRegistry {
    /// (BtreeId, key_type_byte) → 按注册顺序的 (phase, fn) 列表
    triggers: HashMap<(BtreeId, u8), Vec<(TriggerPhase, TriggerFn)>>,
}

impl Default for TriggerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerRegistry {
    /// 创建一个空的触发器注册表
    pub fn new() -> Self {
        Self {
            triggers: HashMap::new(),
        }
    }

    /// 注册一个触发器
    ///
    /// 同一 `(BtreeId, key_type)` + `TriggerPhase` 可注册多个触发器，
    /// 它们按注册顺序依次执行。
    ///
    /// # 参数
    ///
    /// * `ty` - 要监视的 btree 类型
    /// * `key_type` - 要监视的 KeyType（如 `KeyType::Normal as u8`）
    /// * `phase` - 触发器执行阶段
    /// * `func` - 触发器函数
    pub fn register(&mut self, ty: BtreeId, key_type: u8, phase: TriggerPhase, func: TriggerFn) {
        self.triggers
            .entry((ty, key_type))
            .or_default()
            .push((phase, func));
    }

    /// 注销指定 `(BtreeId, key_type, phase)` 的所有触发器
    ///
    /// 返回被移除的触发器数量。
    pub fn unregister(&mut self, ty: BtreeId, key_type: u8, phase: TriggerPhase) -> usize {
        let key = (ty, key_type);
        if let Some(entries) = self.triggers.get_mut(&key) {
            let before = entries.len();
            entries.retain(|(p, _)| *p != phase);
            before - entries.len()
        } else {
            0
        }
    }

    /// 判断指定 `(BtreeId, key_type)` 是否有注册的触发器
    pub fn has_triggers(&self, ty: BtreeId, key_type: u8) -> bool {
        self.triggers
            .get(&(ty, key_type))
            .map(|entries| !entries.is_empty())
            .unwrap_or(false)
    }

    /// 判断是否有任何触发器被注册
    pub fn is_empty(&self) -> bool {
        self.triggers.is_empty()
    }

    /// 获取已注册的触发器总数（跨所有键、所有阶段）
    pub fn len(&self) -> usize {
        self.triggers.values().map(|v| v.len()).sum()
    }

    /// 触发指定 `(BtreeId, key_type)` 的所有触发器（仅限指定阶段）
    ///
    /// 按注册顺序执行。如果任何触发器返回错误，立即停止并返回该错误。
    pub fn fire(
        &self,
        engine: &mut BtreeEngine,
        ty: BtreeId,
        key_type: u8,
        phase: TriggerPhase,
        key: &[u8],
        old_val: Option<&[u8]>,
        new_val: Option<&[u8]>,
    ) -> Result<(), StorageError> {
        if let Some(entries) = self.triggers.get(&(ty, key_type)) {
            for (p, func) in entries {
                if *p == phase {
                    func(engine, ty, key, old_val, new_val)?;
                }
            }
        }
        Ok(())
    }

    /// 触发指定 `(BtreeId, key_type)` 的所有指定阶段的触发器
    ///
    /// 按 phase 顺序执行：Transactional → Atomic → Gc。
    /// 按注册顺序执行同一 phase 内的触发器。
    /// 如果任何触发器返回错误，立即停止并返回该错误。
    pub fn fire_all_phases(
        &self,
        engine: &mut BtreeEngine,
        ty: BtreeId,
        key_type: u8,
        key: &[u8],
        old_val: Option<&[u8]>,
        new_val: Option<&[u8]>,
    ) -> Result<(), StorageError> {
        if let Some(entries) = self.triggers.get(&(ty, key_type)) {
            for phase in &TriggerPhase::ORDERED {
                for (p, func) in entries {
                    if *p == *phase {
                        func(engine, ty, key, old_val, new_val)?;
                    }
                }
            }
        }
        Ok(())
    }
}

// ─── 测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::key::{BtreeKey, KeyType};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 辅助：创建一个测试错误触发器（不捕获变量，可作为 fn 指针）
    fn error_trigger() -> TriggerFn {
        |_engine: &mut BtreeEngine,
         _ty: BtreeId,
         _key: &[u8],
         _old_val: Option<&[u8]>,
         _new_val: Option<&[u8]>| {
            Err(StorageError::Transaction("trigger error".to_string()))
        }
    }

    #[test]
    fn test_trigger_registry_new() {
        let reg = TriggerRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn test_register_and_has_triggers() {
        let mut reg = TriggerRegistry::new();
        assert!(!reg.has_triggers(BtreeId::Extents, KeyType::Normal as u8));

        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            error_trigger(),
        );
        assert!(reg.has_triggers(BtreeId::Extents, KeyType::Normal as u8));
        assert!(!reg.has_triggers(BtreeId::Extents, KeyType::Deleted as u8));
        assert!(!reg.has_triggers(BtreeId::Subvolumes, KeyType::Normal as u8));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_register_multiple_same_key() {
        static C0: AtomicUsize = AtomicUsize::new(0);
        fn c0_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let mut reg = TriggerRegistry::new();

        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c0_trigger,
        );
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c0_trigger,
        );
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn test_unregister() {
        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            error_trigger(),
        );

        let removed = reg.unregister(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
        );
        assert_eq!(removed, 1);
        assert!(!reg.has_triggers(BtreeId::Extents, KeyType::Normal as u8));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn test_unregister_wrong_key() {
        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            error_trigger(),
        );

        let removed = reg.unregister(
            BtreeId::Extents,
            KeyType::Deleted as u8,
            TriggerPhase::Atomic,
        );
        assert_eq!(removed, 0);
        assert!(reg.has_triggers(BtreeId::Extents, KeyType::Normal as u8));
    }

    #[test]
    fn test_fire_triggers_success() {
        static C1: AtomicUsize = AtomicUsize::new(0);
        fn c1_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C1.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c1_trigger,
        );

        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let result = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            &key_bytes,
            None,
            None,
        );
        assert!(result.is_ok());
        assert_eq!(C1.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_fire_triggers_error_propagation() {
        static C2: AtomicUsize = AtomicUsize::new(0);
        fn c2_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c2_trigger,
        );
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            error_trigger(),
        );

        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let result = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            &key_bytes,
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(C2.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_fire_only_matching_phase() {
        static C3: AtomicUsize = AtomicUsize::new(0);
        static C4: AtomicUsize = AtomicUsize::new(0);
        fn c3_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C3.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn c4_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C4.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c3_trigger,
        );
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            c4_trigger,
        );

        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let _ = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            &key_bytes,
            None,
            None,
        );
        assert_eq!(C3.load(Ordering::SeqCst), 1);
        assert_eq!(C4.load(Ordering::SeqCst), 0);

        let _ = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            &key_bytes,
            None,
            None,
        );
        assert_eq!(C3.load(Ordering::SeqCst), 1);
        assert_eq!(C4.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_fire_all_phases_orders_correctly() {
        static ORDER: AtomicUsize = AtomicUsize::new(0);
        static T_SEEN: AtomicUsize = AtomicUsize::new(0);
        static A_SEEN: AtomicUsize = AtomicUsize::new(0);
        static G_SEEN: AtomicUsize = AtomicUsize::new(0);

        fn t_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            T_SEEN.store(ORDER.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        }
        fn a_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            A_SEEN.store(ORDER.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        }
        fn g_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            G_SEEN.store(ORDER.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
            Ok(())
        }

        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Gc,
            g_trigger,
        );
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            a_trigger,
        );
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Transactional,
            t_trigger,
        );

        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let _ = reg.fire_all_phases(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            &key_bytes,
            None,
            None,
        );

        assert_eq!(
            T_SEEN.load(Ordering::SeqCst),
            0,
            "Transactional should be first"
        );
        assert_eq!(A_SEEN.load(Ordering::SeqCst), 1, "Atomic should be second");
        assert_eq!(G_SEEN.load(Ordering::SeqCst), 2, "Gc should be third");
    }

    #[test]
    fn test_fire_wrong_phase_noop() {
        static C5: AtomicUsize = AtomicUsize::new(0);
        fn c5_trigger(
            _engine: &mut BtreeEngine,
            _ty: BtreeId,
            _key: &[u8],
            _old_val: Option<&[u8]>,
            _new_val: Option<&[u8]>,
        ) -> Result<(), StorageError> {
            C5.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let mut reg = TriggerRegistry::new();
        reg.register(
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            c5_trigger,
        );

        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let result = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Transactional,
            &key_bytes,
            None,
            None,
        );
        assert!(result.is_ok());
        assert_eq!(C5.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_fire_no_triggers_noop() {
        let reg = TriggerRegistry::new();
        let mut engine = BtreeEngine::new();
        let key_bytes = bincode::serialize(&BtreeKey::new(100, 1, KeyType::Normal)).unwrap();

        let result = reg.fire(
            &mut engine,
            BtreeId::Extents,
            KeyType::Normal as u8,
            TriggerPhase::Atomic,
            &key_bytes,
            None,
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_trigger_phase_ordering() {
        assert_eq!(TriggerPhase::ORDERED.len(), 3);
        assert_eq!(TriggerPhase::Transactional.index(), 0);
        assert_eq!(TriggerPhase::Atomic.index(), 1);
        assert_eq!(TriggerPhase::Gc.index(), 2);
    }
}
