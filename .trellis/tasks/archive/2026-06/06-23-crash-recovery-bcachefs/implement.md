# Implementation Plan: 崩溃恢复对齐 bcachefs

## 阶段概述

| 阶段 | 内容 | 文件变更 | 前置 |
|------|------|----------|------|
| P1 | Recovery passes 框架 + 迁移 | recovery/ (新) + engine.rs (改) | — |
| P2 | Journal flush + blacklist + clean section | volume.rs (改) + superblock.rs (改) + journal/ (改) | P1 |
| P3 | Recovery 持久化 | superblock.rs (改) | P1 |
| P4 | Journal overlay + set_may_go_rw | overlay.rs (新) + engine.rs (改) | P1 |
| P5 ✅ | Gap 检测 + 动态 buckets | journal/types.rs (改) + superblock.rs (改) + service.rs (改) + volume.rs (改) | 独立 |

> **注意**：P2、P3、P4 在 P1 完成后可**并行**实施，它们之间无共享依赖。
> P5 完全独立，可在任何阶段并行进行。

---

## Phase 1: Recovery Passes 框架 + 迁移

### 1.1 创建模块骨架

创建 `crates/volmount-core/src/recovery/mod.rs`：

```rust
pub mod passes;

use crate::btree::engine::BtreeEngine;
use crate::journal::{self, Journal, JournalEntry, JournalSuperblockState, ReplayEntry};
use crate::storage::superblock::{Superblock, CleanSection, BlacklistEntry};
use std::sync::Arc;
use crate::block::BlockBackend;

// --- Pass system types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryPass {
    JournalRead,
    BtreeRoots,
    AllocRead,
    SetMayGoRw,
    JournalReplay,
}

bitflags! {
    #[derive(Default)]
    pub struct PassFlags: u64 {
        const ALWAYS  = 1 << 0;
        const UNCLEAN = 1 << 1;
        const SILENT  = 1 << 2;
        const NODEFER = 1 << 3;
    }
}

/// bcachefs 对齐：位掩码 deps
pub struct PassDescriptor {
    pub pass: RecoveryPass,
    pub flags: PassFlags,
    pub deps: u64,  // 位掩码，对应 bcachefs 的 deps 字段
    pub name: &'static str,
    pub run: fn(&mut RecoveryState) -> Result<(), crate::Error>,
}

/// PASS_BITS 常量
pub const PASS_BITS: [u64; 5] = [1 << 0, 1 << 1, 1 << 2, 1 << 3, 1 << 4];

/// Bound the pass list
const ALL_PASSES: &[PassDescriptor] = &[
    PassDescriptor { pass: RecoveryPass::JournalRead, flags: PassFlags::ALWAYS, deps: 0, name: "journal_read", run: passes::journal_read::run },
    PassDescriptor { pass: RecoveryPass::BtreeRoots, flags: PassFlags::ALWAYS, deps: PASS_BITS[0], name: "btree_roots", run: passes::btree_roots::run },
    PassDescriptor { pass: RecoveryPass::AllocRead, flags: PassFlags::ALWAYS, deps: PASS_BITS[1], name: "alloc_read", run: passes::alloc_read::run },
    PassDescriptor { pass: RecoveryPass::SetMayGoRw, flags: PassFlags::ALWAYS | PassFlags::SILENT, deps: PASS_BITS[1] | PASS_BITS[2], name: "set_may_go_rw", run: passes::set_may_go_rw::run },
    PassDescriptor { pass: RecoveryPass::JournalReplay, flags: PassFlags::ALWAYS, deps: PASS_BITS[3], name: "journal_replay", run: passes::journal_replay::run },
];

/// bcachefs 对齐：包含 passes_complete（位掩码）和 pass_done（标量最大值）
pub struct RecoveryState {
    pub engine: BtreeEngine,
    pub journal: Journal,
    pub backend: Arc<dyn BlockBackend>,
    pub superblock: Superblock,

    // === bcachefs 对齐的 passes 跟踪 ===

    /// 已完成 pass 的位掩码（对应 bcachefs passes_complete）
    pub passes_complete: u64,
    /// 已完成的最高 pass 序号（对应 bcachefs pass_done，标量）
    pub pass_done: usize,

    // === pass 间共享数据 ===

    pub journal_entries: Vec<JournalEntry>,
    pub recovered_roots: Vec<(Vec<u8>, u64)>,  // (btree_id, root_addr)
    pub replayed_seqs: Vec<u64>,
    pub overlay: Option<JournalOverlay>,  // Phase 4 启用
}

impl RecoveryState {
    pub fn new(
        engine: BtreeEngine,
        journal: Journal,
        backend: Arc<dyn BlockBackend>,
        sb: Superblock,
    ) -> Self {
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

/// bcachefs 对齐的 pass 调度器
///
/// 对应 bcachefs `bch2_run_recovery_passes()`：
/// 1. 按 flags 组装 passes_to_run 位掩码（ALWAYS + UNCLEAN if !clean）
/// 2. 用 trailing_zeros()（对应 __ffs64）迭代每个 pass
/// 3. pass 成功后设置 passes_complete（位掩码）+ pass_done（标量 max）
pub fn run_passes(state: &mut RecoveryState) -> Result<(), crate::Error> {
    // Step 1: 组装 passes_to_run
    let mut passes_to_run: u64 = 0;
    for pd in ALL_PASSES {
        let should_run = if pd.flags.contains(PassFlags::ALWAYS) {
            true  // ALWAYS: 每次崩溃后重跑
        } else if pd.flags.contains(PassFlags::UNCLEAN) {
            !state.superblock.clean_shutdown
        } else {
            false
        };
        if should_run {
            passes_to_run |= PASS_BITS[pd.pass as usize];
        }
    }

    // Step 2: 迭代执行（对应 bcachefs while(r->current_passes) { __ffs64 + run }）
    while passes_to_run != 0 {
        let pass_idx = passes_to_run.trailing_zeros() as usize;
        let pd = &ALL_PASSES[pass_idx];
        passes_to_run &= !(1 << pass_idx);

        (pd.run)(state)?;

        // bcachefs 对齐
        state.passes_complete |= 1 << pass_idx;
        state.pass_done = state.pass_done.max(pass_idx);
    }

    Ok(())
}
```

### 1.2 创建 pass 模块

创建 `crates/volmount-core/src/recovery/passes/mod.rs`：

```rust
pub mod journal_read;
pub mod btree_roots;
pub mod alloc_read;
pub mod set_may_go_rw;
pub mod journal_replay;
```

### 1.3 Pass 实现（初始迁移）

**journal_read.rs**（从 `recover_from_journal()` Phase 1 提取）:
```rust
use crate::journal;
use crate::recovery::{RecoveryState, RecoveryPass};

pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    let sb_state = crate::journal::JournalSuperblockState::from_superblock(&state.superblock);
    let mut entries = Vec::new();

    for bucket in sb_state.dirty_iter()? {
        let bucket_entries = journal::read_bucket(state.backend.clone(), bucket)?;
        entries.extend(bucket_entries);
    }

    // 跳过 blacklisted seqs
    let blacklist = crate::journal::read_blacklist(&mut entries)?;
    if let Some(bl) = blacklist {
        entries.retain(|e| e.seq() > bl.end_seq);
    }

    state.journal_entries = entries;
    Ok(())
}
```

**btree_roots.rs**（从 `recover_from_journal()` Phase 2 提取）:
```rust
use crate::journal::JournalEntry;
use crate::recovery::RecoveryState;

pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    let mut all_roots = Vec::new();
    for entry in &state.journal_entries {
        if let JournalEntry::BtreeRoot { btree_id, addr, .. } = entry {
            all_roots.push((btree_id.clone(), *addr));
        }
    }
    // 去重取最新
    all_roots.sort_by(|a, b| a.0.cmp(&b.0));
    all_roots.dedup_by(|a, b| a.0 == b.0);

    for (btree_id, addr) in &all_roots {
        state.engine.load_root(btree_id, *addr, state.backend.clone())?;
    }
    state.recovered_roots = all_roots;
    Ok(())
}
```

**alloc_read.rs**（Phase 1 为 stub，Phase 5 启用）:
```rust
use crate::recovery::RecoveryState;

pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    // ⚠️ 当前 JournalEntry 可能没有 Alloc 变体
    // Phase 1 保持 no-op，等独立任务添加 Alloc journal 后启用
    // 见 design.md §2.3
    Ok(())
}
```

**set_may_go_rw.rs**:
```rust
use crate::recovery::RecoveryState;

pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    // 启用 overlay 的标志位
    // 实际 overlay 模块在 Phase 4 实现
    Ok(())
}
```

**journal_replay.rs**（从 `recover_from_journal()` Phase 3 提取）:
```rust
use crate::recovery::RecoveryState;

pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    let sb_state = crate::journal::JournalSuperblockState::from_superblock(&state.superblock);
    // ⚠️ replay_all_to_engine 内部调用 insert_raw()，而非 insert_guarded()
    // 这确保 replay 写入直接落地 btree，不经过 overlay
    let replayed_seqs = state.engine.replay_all_to_engine(state.backend.clone(), &sb_state)?;
    state.replayed_seqs = replayed_seqs;

    // Phase 4+: Drain overlay（set_may_go_rw 后新写入的 keys）
    // if let Some(ref mut overlay) = state.engine.journal_overlay {
    //     overlay.drain_all(&mut state.engine)?;
    // }

    Ok(())
}
```

### 1.4 修改 Engine

从 `BtreeEngine` 移除 `recover_from_journal()`。该方法的三个阶段已迁移到 pass 实现中。

同时删除 engine.rs 中对 `recover_from_journal` 的所有引用。

### 1.5 修改 volume.rs

`init_volume()` 中的 recovery 逻辑改为：

```rust
pub fn init_volume(...) -> Result<Volume> {
    let mut engine = BtreeEngine::new(...);
    // ... 现有初始化 ...

    if sb.clean_shutdown {
        // Clean section 快路径（Phase 2 后启用）
        if let Some(ref cs) = sb.clean_section {
            for (btree_id, addr) in &cs.root_addrs {
                engine.load_root(btree_id, *addr, backend.clone())?;
            }
        }
    } else {
        // Recovery passes
        let journal = Journal::from_superblock(&sb);
        let mut state = recovery::RecoveryState::new(engine, journal, backend.clone(), sb.clone());
        recovery::run_passes(&mut state)?;

        // 持久化
        sb.replayed_seqs = state.replayed_seqs.clone();
        sb.pass_done = state.pass_done;
        // Phase 3: JournalSuperblockState 写回

        engine = state.engine;
    }

    let volume = Volume::new(engine, ...);
    Ok(volume)
}
```

### 1.6 修改 lib.rs

```rust
pub mod recovery;
```

### 1.7 前置条件：Superblock 实现 Clone

`RecoveryState::new()` 需要 `sb` 传值（恢复后 caller 仍需保留一份 sb 用于 volume 初始化）。确保 `Superblock` 实现 `Clone`：

```rust
#[derive(Clone)]
pub struct Superblock { ... }
```

如果当前没有，添加 `#[derive(Clone)]` 或手动实现。

### 1.8 编译验证

- [ ] `cargo build -p volmount-core` 通过
- [ ] `cargo build` 通过

---

## Phase 2: Journal Flush + Blacklist + Clean Section

### 2.1 Journal flush

在 `journal/types.rs` 或 `journal/mod.rs` 添加 `write_pending()` 方法：

```rust
impl Journal {
    /// 将 engine 中所有 pending journal entries flush 到磁盘
    pub fn write_pending(&mut self, engine: &BtreeEngine, backend: Arc<dyn BlockBackend>) -> Result<()> {
        let pending = engine.drain_pending_journal_entries();
        for entry in pending {
            self.write_journal(&entry, backend.clone())?;
        }
        Ok(())
    }
}
```

### 2.2 Blacklist entry

在 `journal/types.rs` 添加 blacklist entry 类型和写入：

```rust
#[derive(Debug, Clone)]
pub struct BlacklistEntry {
    pub start_seq: u64,
    pub end_seq: u64,
}

impl Journal {
    pub fn write_blacklist(&mut self, end_seq: u64, backend: Arc<dyn BlockBackend>) -> Result<()> {
        let entry = JournalEntry::Blacklist {
            seq: self.current_seq(),
            start_seq: self.last_seq(),
            end_seq,
        };
        self.write_journal(&entry, backend)
    }
}
```

解析 blacklist（在 `read_bucket` 后或 `journal_read` pass 中）：

```rust
pub fn read_blacklist(entries: &mut Vec<JournalEntry>) -> Result<Option<BlacklistEntry>> {
    let blacklists: Vec<_> = entries.iter()
        .filter_map(|e| {
            if let JournalEntry::Blacklist { start_seq, end_seq, .. } = e {
                Some(BlacklistEntry { start_seq: *start_seq, end_seq: *end_seq })
            } else {
                None
            }
        })
        .collect();
    // 取最新的 blacklist
    Ok(blacklists.into_iter().max_by_key(|bl| bl.end_seq))
}
```

### 2.3 Clean section

在 `superblock.rs` 添加 CleanSection 类型：

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanSection {
    /// Btree root addresses from checkpoint
    pub root_addrs: Vec<(Vec<u8>, u64)>,  // (btree_id, addr)
    /// Journal seq captured in this checkpoint
    pub journal_seq: u64,
}

// Superblock 新增字段：
pub struct Superblock {
    // ... 现有字段 ...
    pub clean_section: Option<CleanSection>,
}
```

### 2.4 checkpoint_volume 重排序

```rust
pub fn checkpoint_volume(...) -> Result<()> {
    // 1. Flush journal
    journal.write_pending(&engine, backend.clone())?;

    // 2. Checkpoint
    let root_addrs = engine.checkpoint(backend.clone())?;

    // 3. Blacklist
    journal.write_blacklist(journal.current_seq(), backend.clone())?;

    // 4. Clean section
    let seq = journal.current_seq();
    sb.journal_seq = seq;
    sb.last_seq = seq;
    sb.clean_section = Some(CleanSection {
        root_addrs: root_addrs.clone(),
        journal_seq: seq,
    });
    sb.clean_shutdown = true;

    // 5. Persist superblock
    sb.persist(backend)?;
    Ok(())
}
```

---

## Phase 3: Recovery 持久化

### 3.1 replayed_seqs 写回

在 `init_volume()` 的 recovery 分支中，recovery 完成后：

```rust
// After recovery::run_passes()
sb.replayed_seqs = state.replayed_seqs.clone();
sb.journal_seq = state.journal.current_seq();
sb.last_seq = state.journal.last_seq();
sb.pass_done = state.pass_done as u64;
```

### 3.2 JournalSuperblockState 写回

```rust
let jss = JournalSuperblockState {
    dirty_idx: state.journal.dirty_idx,
    discard_idx: state.journal.discard_idx,
    bucket_seq: state.journal.bucket_seq.clone(),
    seq: state.journal.seq,
    last_seq: state.journal.last_seq,
};
```

需要确定 JournalSuperblockState 如何序列化到 Superblock。当前 Superblock 已经有 journal_seq, journal_last_seq, journal_buckets 字段。可以使它们与 JournalSuperblockState 保持一致。

---

## Phase 4: Journal Overlay

### 4.1 JournalOverlay 结构

```rust
// recovery/overlay.rs

use std::collections::VecDeque;

#[derive(Debug)]
pub struct OverlayEntry {
    pub journal_seq: u64,
    pub btree_id: Vec<u8>,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// bcachefs journal_keys overlay
pub struct JournalOverlay {
    entries: VecDeque<OverlayEntry>,
    pub active: bool,
    pub draining: bool,
}

impl JournalOverlay {
    pub fn new() -> Self {
        Self { entries: VecDeque::new(), active: true, draining: false }
    }

    pub fn push(&mut self, seq: u64, btree_id: Vec<u8>, key: Vec<u8>, value: Vec<u8>) {
        self.entries.push_back(OverlayEntry { journal_seq: seq, btree_id, key, value });
    }

    pub fn drain_all(&mut self, engine: &mut BtreeEngine) -> Result<(), crate::Error> {
        self.draining = true;
        while let Some(entry) = self.entries.pop_front() {
            // Apply to btree
            engine.insert_raw(entry.btree_id, entry.key, entry.value)?;
        }
        self.active = false;
        Ok(())
    }
}
```

### 4.2 Engine 修改

```rust
pub struct BtreeEngine {
    // ... 现有字段 ...
    pub journal_overlay: Option<JournalOverlay>,
}

impl BtreeEngine {
    pub fn insert_guarded(&mut self, btree_id: Vec<u8>, key: Vec<u8>, value: Vec<u8>, journal_seq: u64) -> Result<()> {
        if let Some(ref overlay) = self.journal_overlay {
            if overlay.active && !overlay.draining {
                overlay.push(self.journal_seq, ...);
                return Ok(());
            }
        }
        self.insert_raw(btree_id, key, value)
    }
}
```

### 4.3 set_may_go_rw pass

```rust
pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    let overlay = JournalOverlay::new();
    state.engine.journal_overlay = Some(overlay);
    Ok(())
}
```

### 4.4 journal_replay pass 修改

在 replay 完成后 drain overlay：

```rust
pub fn run(state: &mut RecoveryState) -> Result<(), crate::Error> {
    let sb_state = ...;
    let replayed_seqs = state.engine.replay_all_to_engine(...)?;
    state.replayed_seqs = replayed_seqs;

    // Drain overlay
    if let Some(ref mut overlay) = state.engine.journal_overlay {
        overlay.drain_all(&mut state.engine)?;
    }

    Ok(())
}
```

---

## Phase 5: Gap 检测 + 动态 Buckets

### 5.1 Gap 检测

修改 `crates/volmount-core/src/journal/mod.rs` 中的 `read_bucket()`：

```rust
pub fn read_bucket(backend: Arc<dyn BlockBackend>, bucket: u64) -> Result<Vec<JournalEntry>> {
    let data = backend.read_block(bucket)?;
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + HEADER_SIZE <= data.len() {
        let header = match parse_header(&data[offset..]) {
            Ok(h) => h,
            Err(_) => break,  // 无效 header，终止
        };

        if !verify_crc(&data[offset..offset + header.total_size]) {
            // Gap: CRC 失败，尝试跳过
            offset += header.total_size;
            continue;
        }

        match deserialize_entry(&data[offset..offset + header.total_size]) {
            Ok(entry) => entries.push(entry),
            Err(_) => {
                offset += header.total_size;
                continue;
            }
        }
        offset += header.total_size;
    }

    Ok(entries)
}
```

### 5.2 动态 Buckets

```rust
// Superblock journal_buckets 改为 Vec<u64>
// 序列化时需要处理向后兼容

impl Superblock {
    pub fn journal_buckets(&self) -> &[u64] {
        &self.journal_buckets
    }

    pub fn set_journal_buckets(&mut self, buckets: Vec<u64>) {
        self.journal_buckets = buckets;
    }
}
```

---

## 执行顺序

```
Phase 1: Framework + Migration  ──►  recovery/ 模块 + engine/volume 修改
         │
         ├──► Phase 2: Flush + Blacklist + Clean     (P1 后可并行)
         ├──► Phase 3: Persistence                    (P1 后可并行)
         ├──► Phase 4: Overlay + set_may_go_rw        (P1 后可并行)
         │
Phase 5: Gap + dynamic              (完全独立，可随时进行)
```

各 Phase 在编译验证通过后进入下一阶段。P2、P3、P4 在 P1 完成后可并行实施。

---

## 测试策略

### 单元测试（每个 pass）

| 测试 | 内容 | 位置 |
|------|------|------|
| `test_pass_journal_read_filters_blacklist` | 模拟 entries，验证 blacklist 过滤 | `recovery/tests/` |
| `test_pass_journal_read_gap_detection` | CRC 损坏数据，验证跳过 | `recovery/tests/` |
| `test_pass_btree_roots_dedup` | 多个 BtreeRoot entry，验证取最新 | `recovery/tests/` |
| `test_pass_set_may_go_rw_enables_overlay` | 验证 overlay 被创建并 active | `recovery/tests/` |
| `test_pass_journal_replay_inserts_directly` | 验证 replay 调用 insert_raw 而非 insert_guarded | `recovery/tests/` |

### 集成测试

| 测试 | 内容 |
|------|------|
| `test_clean_shutdown_restart` | checkpoint → clean shutdown → restart → clean section 快路径 |
| `test_unclean_shutdown_recovery` | 写数据 → 模拟崩溃 → restart → recovery passes → 数据完整 |
| `test_double_crash` | 恢复后 checkpoint 前再次崩溃 → 重新 passes 恢复 |
| `test_partial_journal` | 写部分 journal bucket → crash → gap 检测跳过坏块 |
| `test_blacklist_after_checkpoint` | checkpoint → 写新数据 → crash → 仅回放 checkpoint 后的 entries |
| `test_overlay_new_writes_during_replay` | set_may_go_rw 后新写入 → overlay 捕获 → drain → 数据正确 |
| `test_multiple_crash_cycles` | 3+ 轮 crash/recovery 验证数据一致性 |

### 验证方法

```rust
// 模拟崩溃的模式：
// 1. 正常创建并写入数据
// 2. 不调用 checkpoint_volume，直接丢弃 Volume（模拟掉电）
// 3. 重新 init_volume（此时 superblock.clean_shutdown = false）
// 4. 断言 recovery passes 运行，数据与预期一致

fn simulate_crash_and_recover(backend: Arc<TestBackend>, expected_keys: &[KeyValue]) {
    let sb = Superblock::read_from_backend(backend.clone()).unwrap();
    assert!(!sb.clean_shutdown);  // 未正常 shutdown
    
    let volume = init_volume(backend.clone(), ...).unwrap();
    for kv in expected_keys {
        assert_eq!(volume.read(kv.key).unwrap(), kv.value);
    }
}
```

### 回归测试
- 所有现有 `cargo test -p volmount-core` 和 `cargo test -p volmountd` 必须通过
- Phase 1 后预存失败（`test_skip_list_ordered`、`test_create_multiple`）保持不变

---

## 文件变更清单

### 新增
- `crates/volmount-core/src/recovery/mod.rs` — passes 框架
- `crates/volmount-core/src/recovery/passes/mod.rs` — pass 模块
- `crates/volmount-core/src/recovery/passes/journal_read.rs`
- `crates/volmount-core/src/recovery/passes/btree_roots.rs`
- `crates/volmount-core/src/recovery/passes/alloc_read.rs`
- `crates/volmount-core/src/recovery/passes/set_may_go_rw.rs`
- `crates/volmount-core/src/recovery/passes/journal_replay.rs`
- `crates/volmount-core/src/recovery/overlay.rs`

### 修改
- `crates/volmount-core/src/btree/engine.rs` — 移除 recover_from_journal，加 overlay 支持、insert_guarded()、insert_raw() 公开
- `crates/volmount-core/src/btree/mod.rs` — 导出新类型
- `crates/volmount-core/src/journal/types.rs` — BlacklistEntry, flush()
- `crates/volmount-core/src/journal/types.rs` — gap 检测（CRC 失败 continue 而非 break）
- `crates/volmount-core/src/storage/service.rs` — journal_buckets → Vec&lt;u64&gt; 支持
- `crates/volmount-core/src/storage/superblock.rs` — CleanSection, pass_done: u64, replayed_seqs, **journal_buckets: [u64; 32] → Vec&lt;u64&gt;**。**新字段用 Option/T，Clone 必选**
- `crates/volmount-core/src/lib.rs` — pub mod recovery
- `crates/volmountd/src/volume.rs` — 新 recovery 流程；移除 journal_bucket_count / [..n] slicing

### 删除
- (无)
