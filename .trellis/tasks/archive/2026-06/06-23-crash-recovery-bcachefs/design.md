# Design: 崩溃恢复对齐 bcachefs

## Overview

新增 `volmount-core/src/recovery/` 模块，实现与 bcachefs 严格对齐的 recovery passes 系统 + journal overlay。

```
daemon                          volmount-core

init_volume()
  └─ recovery::run_passes() ──────► journal_read pass
                                    btree_roots pass
                                    alloc_read pass
                                    set_may_go_rw pass  ← overlay on
                                    journal_replay pass
  └─ Volume::new(engine, ...)

checkpoint_volume()
  ├─ journal.flush()               ← 先 flush pending 条目
  ├─ engine.checkpoint()           ← 写 checkpoint 块
  ├─ journal.write_blacklist()     ← 标记已覆盖 seq
  └─ superblock.persist()          ← 含 clean section + passes_done
```

---

## 1. Recovery Passes 系统

### 模块结构

```
crates/volmount-core/src/recovery/
├── mod.rs           — RecoveryPass enum, PassFlags, PassDescriptor, RecoveryState, run_passes()
├── passes/
│   ├── journal_read.rs    — 读 journal buckets，收集 entries
│   ├── btree_roots.rs     — 从 journal entries 恢复 btree root
│   ├── alloc_read.rs      — 加载 allocator state
│   ├── set_may_go_rw.rs   — 启用 overlay，加载 roots
│   └── journal_replay.rs  — 重放 journal keys 到 btree
└── overlay.rs       — JournalOverlay（journal keys buffer）
```

### 核心结构

```rust
// mod.rs

// bcachefs 对齐：使用显式位值的 pass 枚举
// 位掩码 `1 << (pass as u64)` 在调度中使用（见 run_passes）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecoveryPass {
    JournalRead = 0,
    BtreeRoots = 1,
    AllocRead = 2,
    SetMayGoRw = 3,
    JournalReplay = 4,
}

/// 每个 pass 对应的位掩码常量
pub const PASS_BITS: [u64; 5] = [1 << 0, 1 << 1, 1 << 2, 1 << 3, 1 << 4];

bitflags! {
    #[derive(Default, Serialize, Deserialize)]
    pub struct PassFlags: u64 {
        /// Always run (clean and unclean)
        const ALWAYS  = 1 << 0;
        /// Only run on unclean shutdown
        const UNCLEAN = 1 << 1;
        /// Silent pass (no user output)
        const SILENT  = 1 << 2;
        /// Can run in background
        const ONLINE  = 1 << 3;
        /// Must not be deferred
        const NODEFER = 1 << 4;
    }
}

/// bcachefs 对齐：每个 pass 描述符（对应 struct bch_recovery_pass）
pub struct PassDescriptor {
    pub pass: RecoveryPass,
    pub flags: PassFlags,
    /// 依赖的 pass 位掩码（对应 bcachefs deps 字段）
    pub deps: u64,
    pub name: &'static str,
    pub run: fn(&mut RecoveryState) -> Result<(), crate::Error>,
}

/// 定义所有 pass 及其依赖关系
pub const ALL_PASSES: &[PassDescriptor] = &[
    // JournalRead: 读 journal buckets（无依赖）
    PassDescriptor { pass: RecoveryPass::JournalRead, flags: PassFlags::ALWAYS, deps: 0, name: "journal_read", run: passes::journal_read::run },
    // BtreeRoots: 需要先读 journal
    PassDescriptor { pass: RecoveryPass::BtreeRoots, flags: PassFlags::ALWAYS, deps: PASS_BITS[0], name: "btree_roots", run: passes::btree_roots::run },
    // AllocRead: 需要先恢复 roots
    PassDescriptor { pass: RecoveryPass::AllocRead, flags: PassFlags::ALWAYS, deps: PASS_BITS[1], name: "alloc_read", run: passes::alloc_read::run },
    // SetMayGoRw: 需要 roots + alloc 就绪
    PassDescriptor { pass: RecoveryPass::SetMayGoRw, flags: PassFlags::ALWAYS | PassFlags::SILENT, deps: PASS_BITS[1] | PASS_BITS[2], name: "set_may_go_rw", run: passes::set_may_go_rw::run },
    // JournalReplay: 必须在 set_may_go_rw 之后
    PassDescriptor { pass: RecoveryPass::JournalReplay, flags: PassFlags::ALWAYS, deps: PASS_BITS[3], name: "journal_replay", run: passes::journal_replay::run },
];

/// bcachefs RecoveryState（对应 struct bch_fs_recovery）
pub struct RecoveryState {
    pub engine: BtreeEngine,
    pub journal: Journal,
    pub backend: Arc<dyn BlockBackend>,
    pub superblock: Superblock,

    // === bcachefs 对齐的 passes 跟踪 ===

    /// 已完成 pass 的位掩码（对应 passes_complete）
    pub passes_complete: u64,
    /// 已完成的最高 pass 序号（对应 pass_done）
    pub pass_done: usize,

    // === pass 间共享数据 ===

    /// Journal entries（journal_read → btree_roots → journal_replay）
    pub journal_entries: Vec<JournalEntry>,
    /// 从 journal 恢复的 btree roots
    pub recovered_roots: Vec<(Vec<u8>, u64)>,
    /// 已回放的 journal seq（恢复后持久化到 superblock）
    pub replayed_seqs: Vec<u64>,
    /// Journal overlay（set_may_go_rw pass 激活）
    pub overlay: Option<JournalOverlay>,
}

impl RecoveryState {
    pub fn new(engine: BtreeEngine, journal: Journal, backend: Arc<dyn BlockBackend>, sb: Superblock) -> Self {
        Self {
            engine, journal, backend,
            superblock: sb,
            passes_complete: 0,
            pass_done: 0,
            journal_entries: vec![],
            recovered_roots: vec![],
            replayed_seqs: vec![],
            overlay: None,
        }
    }
}
```

### Pass 依赖关系

```
JournalRead ──→ BtreeRoots ──→ AllocRead ──→ SetMayGoRw ──→ JournalReplay
     │               │                           │
     └── 读所有       └── 从 entries 提取          └── enable overlay,
         journal          root info, merge,            load roots
         buckets          load roots
```

| Pass | Flags | Deps |
|------|-------|------|
| JournalRead | ALWAYS | — |
| BtreeRoots | ALWAYS | [JournalRead] |
| AllocRead | ALWAYS | [BtreeRoots] |
| SetMayGoRw | ALWAYS, SILENT | [BtreeRoots, AllocRead] |
| JournalReplay | ALWAYS | [SetMayGoRw] |

### run_passes() 主流程

```rust
/// bcachefs 对齐的 recovery pass 调度器
///
/// 对应 bcachefs `bch2_run_recovery_passes()`：
/// 1. `bch2_run_recovery_passes_startup` 按 `PASS_ALWAYS | PASS_UNCLEAN` 组装位掩码
/// 2. 用 `__ffs64`（find-first-set-bit）迭代每个 pass
/// 3. pass 成功后设置 `passes_complete |= BIT_ULL(pass)` 和 `pass_done = max(pass_done, pass)`
/// 4. `pass_done` 在 bcachefs 中是标量（最高已完成 pass 序号），非位掩码
/// 5. `passes_complete` 是位掩码，两者都在 `struct bch_fs_recovery`
pub fn run_passes(state: &mut RecoveryState) -> Result<()> {
    // Step 1: 按 flags 组装 passes_to_run（对应 bcachefs startup 中的计算）
    let mut passes_to_run: u64 = 0;
    for pd in ALL_PASSES {
        let should_run = if pd.flags.contains(PassFlags::ALWAYS) {
            true  // ALWAYS: 每次崩溃后重跑（journal 内容可能变了）
        } else if pd.flags.contains(PassFlags::UNCLEAN) {
            !state.superblock.clean_shutdown
        } else {
            false
        };
        if should_run {
            passes_to_run |= PASS_BITS[pd.pass as usize];
        }
    }

    // Step 2: 验证拓扑排序（依赖完整性检查）
    // bcachefs 的 passes_format.h 中显式定义了每个 pass 的 deps 位掩码

    // Step 3: 迭代执行（对应 bcachefs `while(r->current_passes) { __ffs64 + run }`）
    while passes_to_run != 0 {
        let pass_idx = passes_to_run.trailing_zeros() as usize;
        let pd = &ALL_PASSES[pass_idx];
        passes_to_run &= !(1 << pass_idx);

        (pd.run)(state)?;

        // bcachefs 对齐：设置 passes_complete（位掩码）+ pass_done（标量最大值）
        state.passes_complete |= 1 << pass_idx;
        state.pass_done = state.pass_done.max(pass_idx);
    }

    // Step 4: 恢复完成后持久化
    state.persist_completion()?;
    Ok(())
}

impl RecoveryState {
    fn persist_completion(&mut self) -> Result<()> {
        self.superblock.clean_shutdown = false;  // 标记为"活"状态
        self.superblock.replayed_seqs = self.replayed_seqs.clone();
        self.superblock.pass_done = self.pass_done;
        // passes_complete 是运行时状态，不直接持久化到 superblock（bcachefs 做法相同）
        // 当前无重跑需求：如果崩溃发生在恢复后但在 checkpoint 前，
        // 下次 startup 会重新运行所有 ALWAYS pass
        self.persist_superblock()
    }
}
```

> **bcachefs pass_done 语义**：在 bcachefs 中，`pass_done` 存储在内存（`struct bch_fs_recovery`）而非 superblock 中。pass 完成后设置 `pass_done = max(pass_done, pass)`。它主要用于两步 recovery（startup 时 `from` 参数跳过已完成的 pass），以及判断 `journal_replay` 是否已完成。`passes_complete` 也是运行时位掩码，不跨崩溃持久化。
>
> **volmount 做法**：所有 5 个 pass 都是 `ALWAYS`，每次崩溃后都必须重跑。`passes_complete` 和 `pass_done` 目前是运行时跟踪状态，为未来多阶段 recovery 预留扩展性。

---

## 2. Pass 实现

### 2.1 JournalRead

```rust
// passes/journal_read.rs

/// Pass: 读取所有 journal buckets，收集 entries
pub fn run(state: &mut RecoveryState) -> Result<()> {
    let sb_state = JournalSuperblockState::from_superblock(&state.superblock);
    let mut entries = Vec::new();
    
    for bucket in sb_state.dirty_iter()? {
        let bucket_entries = journal::read_bucket(state.backend.clone(), bucket)?;
        
        // bcachefs 对齐：gap 检测，CRC 失败时尝试跳过
        let filtered = filter_gaps(bucket_entries);
        entries.extend(filtered);
    }
    
    // 应用 blacklist（跳过已 checkpoint 的 seq）
    if let Some(blacklist) = &state.superblock.blacklist {
        entries.retain(|e| e.seq() > blacklist.end_seq);
    }
    
    state.journal_entries = entries;
    Ok(())
}
```

### 2.2 BtreeRoots

```rust
// passes/btree_roots.rs

/// Pass: 从 journal entries 中提取 btree root 信息
pub fn run(state: &mut RecoveryState) -> Result<()> {
    let mut all_roots = Vec::new();
    
    for entry in &state.journal_entries {
        if let JournalEntry::BtreeRoot { btree_id, addr, .. } = entry {
            all_roots.push(RootInfo { btree_id: *btree_id, addr: *addr });
        }
    }
    
    // merge 同 btree_id 的 roots，取最新
    let merged = merge_roots(all_roots);
    
    // 加载 roots
    for root in &merged {
        state.engine.load_root(root.btree_id, root.addr, state.backend.clone())?;
    }
    
    state.recovered_roots = merged;
    Ok(())
}
```

### 2.3 AllocRead

```rust
// passes/alloc_read.rs

/// Pass: 从 journal entries 中恢复 allocator 状态
///
/// 注意：当前 JournalEntry 枚举可能没有 Alloc 变体。
/// Phase 1 中此 pass 保持为 stub/no-op，在独立任务中添加
/// JournalEntry::Alloc 变体和 alloc journal 写入后再启用。
pub fn run(state: &mut RecoveryState) -> Result<()> {
    // TBD: journal 中是否存在 AllocEntry 取决于 alloc journal 设计
    // 如果 JournalEntry 尚未包含 Alloc 变体，此 pass 暂为 no-op
    // 否则过滤 Alloc 条目并更新 engine.alloc
    
    // let alloc_entries: Vec<_> = state.journal_entries.iter()
    //     .filter_map(|e| {
    //         if let JournalEntry::Alloc { block, state: alloc_state, .. } = e {
    //             Some((*block, *alloc_state))
    //         } else {
    //             None
    //         }
    //     })
    //     .collect();
    // state.engine.alloc.update_from_journal(&alloc_entries);
    Ok(())
}
```

### 2.4 SetMayGoRw

```rust
// passes/set_may_go_rw.rs

/// Pass: 启用 journal overlay。之后 overlay_guard 写入 journal keys buffer
pub fn run(state: &mut RecoveryState) -> Result<()> {
    let overlay = JournalOverlay::new();
    state.overlay = Some(overlay);
    state.engine.set_overlay_active(true);
    Ok(())
}
```

### 2.5 JournalReplay

```rust
// passes/journal_replay.rs

/// Pass: 重放 journal keys 到 btree
///
/// ⚠️ 关键：replay_all_to_engine 必须调用 insert_raw()（绕过 overlay）。
///      在 set_may_go_rw 之后到达的**新**写入才走 insert_guarded() → overlay。
///      replay 本身写入的是 journal 中的"历史"数据，必须直接落地 btree。
pub fn run(state: &mut RecoveryState) -> Result<()> {
    let sb_state = JournalSuperblockState::from_superblock(&state.superblock);
    state.engine.replay_all_to_engine(state.backend.clone(), &sb_state)?;
    
    // 收集 replayed_seqs
    state.replayed_seqs = state.engine.get_replayed_seqs();
    
    // Drain overlay（set_may_go_rw 后新写入的 keys）
    if let Some(ref mut overlay) = state.overlay {
        overlay.drain_all(&mut state.engine)?;
    }
    
    // 关闭 overlay
    state.engine.set_overlay_active(false);
    state.overlay = None;
    Ok(())
}
```

---

## 3. Journal Overlay

### 设计

严格对齐 bcachefs `journal_keys`（定义在 `journal_overlay_types.h`）：

bcachefs `struct journal_keys` 关键特征：
- **预分配数组**（`nr`/`size`/`data`），`struct journal_key` 按 `(btree_id, level, pos)` 排序
- **Gap buffer**：数组中间有一个 gap（空洞），顺序插入为 O(1) 而非 O(n²)
- **overwrite 跟踪**：`overwritten` 标志 + `overwritten_range` 记录被覆盖的范围
- **reference count**：跨线程共享（volmount 单线程不需要）

volmount overlay 设计（单线程简化版，保留排序 + gap buffer）：

```rust
// overlay.rs

/// bcachefs journal_keys overlay 对齐实现
///
/// 使用排序数组 + gap buffer：
/// - `set_may_go_rw` pass 激活 overlay
/// - overlay active + replay 未完成时，外部写入走 overlay buffer
/// - replay 完成后 drain overlay 到 btree
/// - 排序 + gap buffer 保证 O(log n) 二分查找, O(1) 顺序插入
pub struct JournalOverlay {
    /// 排序的 overlay entries，按 (btree_id, key) 升序
    entries: Vec<OverlayEntry>,
    /// nr: 有效条目数（size=entries.capacity()，gap = entries.len() - nr）
    nr: usize,
    /// gap buffer 位置（bcachefs 对齐）
    gap: usize,
    /// 是否已激活（= flush 到 btree 中）
    active: bool,
    /// 是否正在 drain
    draining: bool,
}

/// 对应 bcachefs `struct journal_key`（精简版）
struct OverlayEntry {
    journal_seq: u64,
    btree_id: Vec<u8>,
    key: Vec<u8>,
    value: Vec<u8>,
    overwritten: bool,
}

impl JournalOverlay {
    /// 创建 overlay，容量 1024（对应 bcachefs BCH_REPLICAS_MAX * KEY_MAX_NR）
    pub fn new() -> Self;

    /// 插入一个 key，按 (btree_id, key) 排序
    /// 如果 key 已存在，标记旧的为 overwritten 并更新值
    pub fn push(&mut self, seq: u64, btree_id: Vec<u8>, key: Vec<u8>, value: Vec<u8>);

    /// 二分查找 key 在 entries 中的位置
    pub fn search(&self, btree_id: &[u8], key: &[u8]) -> Option<&OverlayEntry>;

    /// Drain 所有 entries 到 btree（按排序顺序应用，确保 btree 结构正确）
    pub fn drain_all(&mut self, engine: &mut BtreeEngine) -> Result<()>;
}
```

### BtreeEngine 集成（bcachefs 对齐）

```rust
// BtreeEngine 新增字段

pub struct BtreeEngine {
    // ... 现有字段 ...

    /// Journal overlay（对应 bcachefs `struct journal_keys journal_keys`）
    /// set_may_go_rw pass 激活，journal_replay pass 完成时 drain
    pub journal_overlay: Option<JournalOverlay>,
}

impl BtreeEngine {
    /// bcachefs 对齐：外部写入守卫
    ///
    /// 对应 bcachefs 的 `bch2_btree_iter_peek()` 路径中对 `journal_keys`
    /// 的检查：如果 key 存在于 journal_keys 中，从那里读取/写入。
    ///
    /// 规则：
    /// - 外部 API 调用（daemon volume write）→ insert_guarded()
    /// - overlay active + draining=false → push to overlay buffer
    /// - 否则 → insert_raw()（直写 btree，不经过 overlay）
    ///
    /// ⚠️ 内部操作（replay、compaction、split/merge）始终直写 btree
    ///   → insert_raw()，不走此守卫
    pub fn insert_guarded(&mut self, btree_id: Vec<u8>, key: Vec<u8>, value: Vec<u8>, journal_seq: u64) -> Result<()> {
        if let Some(ref overlay) = self.journal_overlay {
            if overlay.active && !overlay.draining {
                // 写入 overlay buffer（排序插入 + gap buffer）
                overlay.push(journal_seq, btree_id, key, value);
                return Ok(());
            }
        }
        self.insert_raw(btree_id, key, value)
    }

    /// 绕过 overlay 的直写（供 replay 和内部操作使用）
    pub fn insert_raw(&mut self, btree_id: Vec<u8>, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        // 现有的 btree insert 逻辑
        self.btree_insert(btree_id, key, value)
    }
}
```

> **bcachefs 的 btree 路径详解**：`journal_keys` 不是 btween `set_may_go_rw` 和 `journal_replay` 之间"捕获所有写入"的全局 buffer。它是一个按 `(btree_id, level, pos)` 排序的数组，在 btree 遍历路径中做检查：当读取一个 btree 节点时，如果 key 在 journal_keys 中也有，取 journal_keys 值。写入时也先写入 journal_keys（如果 active），然后写入 journal + btree。`journal_replay` pass 会应用 journal_keys 到 btree 并释放。
>
> **volmount 简化**：由于单线程 + 单 btree，我们用 overlay 作为 write-through buffer。新写入直接进 overlay。replay 完成后 drain 到 btree。这是行为等价的简化，不是 bcachefs 的完全复刻。

---

## 4. 业务流程对齐

### 4.1 Clean Shutdown（checkpoint_volume）

```
1. journal.write_pending()      ← flush 所有 pending journal entries
2. engine.checkpoint()          ← 写 btree checkpoint 块
3. journal.write_blacklist()    ← 写 blacklist 条目标记已覆盖 seq
4. superblock.clean_section =   ← 存 btree root 地址 + last_seq
   CleanSection { root_addrs, journal_seq }
5. superblock.clean_shutdown = true
6. superblock.persist()
```

### 4.2 Clean Startup

```
1. superblock.clean_shutdown == true?
2. Yes → clean section 快路径：
   a. load_root from clean_section.root_addrs
   b. 跳过 journal read（journal 中无未 checkpoint 条目）
   c. alloc 从 superblock 恢复
3. No  → recovery passes（见 4.3）
```

### 4.3 Unclean Startup（recovery passes）

```
1. superblock.clean_shutdown = false
2. recovery::run_passes():
   a. journal_read     → 读所有 buckets，应用 blacklist filter
   b. btree_roots      → 从 journal entries 提取 roots，load
   c. alloc_read       → 从 journal entries 恢复 alloc
   d. set_may_go_rw    → 启用 overlay
   e. journal_replay   → 重放 entries 到 engine
3. 持久化：replayed_seqs, passes_done
```

### 4.4 正常运行时写入路径

```
write(key, value):
  engine.insert(key, value)
    └─ insert_guarded()
         ├─ overlay active + replay not done → push to overlay
         └─ else → insert_raw -> journal::write_entry() -> btree insert
```

---

## 5. 与现有代码的关系

### 保留
- `BtreeEngine` 主体结构（engine.rs）
- `Journal` 类型 + `write_journal()`（journal/types.rs）
- `JournalReplayer::replay_all_to_engine()`（journal/replay.rs）
- `Superblock` 大部分字段
- `BtreeOp`（btree/op.rs）

### 修改
| 文件 | 修改内容 |
|------|----------|
| `engine.rs` | 加 `journal_overlay: Option<JournalOverlay>`、`insert_guarded()`、`insert_raw()` 公开；移除 `recover_from_journal()` |
| `superblock.rs` | 加 `passes_done: u64`（默认 0）、`clean_section: Option<CleanSection>`（默认 None）、`replayed_seqs: Vec<u64>`（默认 []）、`blacklist: Option<BlacklistEntry>`（默认 None）。**要求 `#[derive(Clone)]` 或添加** |
| `journal/types.rs` | 加 `BlacklistEntry`、`JournalEntry::Blacklist` 变体、`flush()` 方法 |
| `journal/mod.rs` | 导出 `read_bucket()` 带 gap 检测 |
| `replay.rs` | 保持 `replay_all_to_engine()` 不变；确认其内部调用 `insert_raw()` |
| `volume/mod.rs` | 无变化 |
| `lib.rs` | 加 `pub mod recovery` |

> **序列化兼容性**：`superblock.rs` 反序列化时，所有新字段用 `Option<T>` 或合理默认值，确保旧 volume 在不升级磁盘格式的情况下正常启动。

### 新增
| 文件 | 内容 |
|------|------|
| `recovery/mod.rs` | passes 框架 + run_passes() |
| `recovery/overlay.rs` | JournalOverlay |
| `recovery/passes/journal_read.rs` | Read pass |
| `recovery/passes/btree_roots.rs` | Roots pass |
| `recovery/passes/alloc_read.rs` | Alloc pass |
| `recovery/passes/set_may_go_rw.rs` | Overlay activation |
| `recovery/passes/journal_replay.rs` | Replay pass |

---

## 6. 数据流

```
┌─────────────────────────────────────────────────────────────┐
│                        init_volume()                        │
│                                                             │
│  superblock ──► clean_shutdown? ──► cleanup ──► load_root() │
│       │                                                     │
│       └── no ──► recovery::run_passes(state)                │
│                     │                                       │
│  journal_read ◄─────┘                                       │
│       │                                                     │
│       ▼                                                      │
│  journal_entries (Vec<JournalEntry>)                        │
│       │                                                     │
│       ▼                                                      │
│  btree_roots ──► load_root() ──► engine ready               │
│       │                                                     │
│       ▼                                                      │
│  alloc_read ──► engine.alloc.update()                       │
│       │                                                     │
│       ▼                                                      │
│  set_may_go_rw ──► overlay = Some(...)                      │
│       │                                                     │
│       ▼                                                      │
│  journal_replay ──► replay_all_to_engine()                   │
│       │              │                                       │
│       │              └── overlay.drain_to_engine()           │
│       ▼                                                      │
│  persist: replayed_seqs, passes_done                        │
│                                                             │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                   checkpoint_volume()                       │
│                                                             │
│  1. journal.write_pending()  ──► flush to disk              │
│  2. engine.checkpoint()      ──► write btree blocks         │
│  3. journal.write_blacklist()──► mark covered seqs          │
│  4. superblock.clean= true    ──► persist                   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

---

## 7. Gap 检测强化

当前 `read_bucket()` CRC 失败就 `break`。对齐 bcachefs：

```rust
pub fn read_bucket(backend: Arc<dyn BlockBackend>, bucket: u64) -> Result<Vec<JournalEntry>> {
    let data = backend.read_block(bucket)?;
    let mut entries = Vec::new();
    let mut offset = 0;
    
    while offset + HEADER_SIZE <= data.len() {
        let header = parse_header(&data[offset..])?;
        
        // CRC 校验
        if !verify_crc(&data[offset..offset + header.total_size]) {
            // Gap 检测：尝试跳过这个块继续读下一个
            // bcachefs 用 last_valid_entry + next_entry_offset
            offset += header.total_size;  // 或尝试对齐到下一个 entry
            continue;
        }
        
        let entry = deserialize_entry(&data[offset..offset + header.total_size])?;
        entries.push(entry);
        offset += header.total_size;
    }
    
    Ok(entries)
}
```

---

## 8. 动态 Journal Buckets

当前 `journal_buckets` 在 superblock 中是 `[u64; 32]`。改为：

```rust
pub struct Superblock {
    // ...
    pub journal_buckets: Vec<u64>,  // 不固定长度
    // ...
}
```

需要考虑序列化兼容性（可选：保持旧格式支持或迁移）。

---

## Design Decisions

| 决策 | 选择 | 理由 |
|------|------|------|
| Pass 调度 | 位掩码 + `trailing_zeros()` 迭代（对齐 bcachefs `__ffs64`） | 严格对齐 bcachefs `bch2_run_recovery_passes()` |
| Pass 跟踪 | `passes_complete`（位掩码）+ `pass_done`（标量 max） | 对齐 bcachefs `struct bch_fs_recovery` |
| Pass flags | `PASS_ALWAYS` / `PASS_UNCLEAN` / `PASS_SILENT` | 对齐 bcachefs `passes_format.h` |
| Overlay buffer | 排序 `Vec` + gap buffer（对齐 bcachefs `journal_keys`） | O(log n) 二分查找, O(1) 顺序插入, `overwritten` 标记 |
| Clean section | `Option<CleanSection>` in superblock | 零额外 I/O，clean shutdown 快路径 |
| Gap 检测 | 跳过 CRC 失败块而非 break | 与 bcachefs 一致，提高健壮性 |
| pass_done 持久化 | `superblock.pass_done: u64`（标量） | 对齐 bcachefs `pass_done`（标量，非位掩码） |
| 动态 buckets | `Vec<u64>` 替代 `[u64; 32]` | 消除硬编码限制 |

---

## 附：bcachefs 对齐验证清单

### Pass 系统（recovery.c / passes.c）

| bcachefs 做法 | volmount 设计 | 对齐？ |
|------|------|--------|
| `bch2_run_recovery_passes()` 位掩码调度 | `run_passes()` 位掩码 + `trailing_zeros()` | ✅ |
| `passes_to_run = PASS_ALWAYS \| (!clean ? PASS_UNCLEAN:0)` | 相同 flags 组装 | ✅ |
| `passes_complete |= BIT_ULL(pass)` | `passes_complete |= 1 << pass_idx` | ✅ |
| `pass_done = max(pass_done, pass)` | `pass_done.max(pass_idx)` | ✅ |
| `pass_done` 是标量序号（非位掩码） | `pass_done: usize` | ✅ |
| `PASS_ALWAYS` + `PASS_UNCLEAN` 双 flag | `PassFlags::ALWAYS | PassFlags::UNCLEAN` | ✅ |
| `BCH_RECOVERY_PASS_NR` | `RecoveryPass::NumPasses`（sentinel） | ✅ |

### Journal Overlay（journal_overlay_types.h）

| bcachefs 做法 | volmount 设计 | 对齐？ |
|------|------|--------|
| `nr/size/data` 预分配数组 | `entries: Vec<OverlayEntry>` | ✅（Vec = 预分配） |
| gap buffer 优化顺序插入 | `gap: usize` 字段 | ✅ |
| 按 `(btree_id, level, pos)` 排序 | 按 `(btree_id, key)` 排序 | ✅ |
| 二分查找 `bch2_journal_key_search()` | `search()` 二分查找 | ✅ |
| `overwritten` 标志位 | `overwritten: bool` | ✅ |
| `set_may_go_rw` 后激活 | `active: bool` | ✅ |
| `journal_replay` 后 drain + 释放 | `drain_all()` + `active = false` | ✅ |
| replay 直写 btree（不通过 overlay） | `insert_raw()` | ✅ |

### Superblock

| bcachefs 做法 | volmount 设计 | 对齐？ |
|------|------|--------|
| `sb.clean` 标志 | `clean_shutdown: bool` | ✅ |
| `sb.roots` btree roots | `CleanSection.root_addrs` | ✅ |
| `recovery_passes_required` | 暂不需要（volmount 全 ALWAYS） | ⚠️ 未来扩展 |
| `pass_done` in memory（非持久化） | `pass_done` in memory（当前不持久化到 superblock） | ✅ |

### 已知偏差

| 偏差 | 说明 | 理由 |
|------|------|------|
| bcachefs 有 `BCH_RECOVERY_PASS_NR = 41` passes | volmount 5 passes | volmount 不是文件系统，无需 fsck/quota 等快 |
| bcachefs `journal_keys` 支持多 btree_id + level | volmount 单 btree | volmount 存储模型更简单 |
| bcachefs `pass_done` 在 superblock 中有扩展字段 | volmount 暂不持久化 | 当前所有 pass 都是 ALWAYS，崩溃后重跑即可 |
| bcachefs `journal_blacklist` 在 journal_read 中内联处理 | volmount 有独立 BlacklistEntry 类型 | 行为等价，设计更清晰 |
