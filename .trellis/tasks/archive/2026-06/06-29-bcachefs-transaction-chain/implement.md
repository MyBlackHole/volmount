# bcachefs 事务全链路整合 — 实施计划

## 核心顺序

本实施将事务流从 journal-WAL 先于 btree 的旧顺序重构为 **bcachefs 完全对齐的 reserve→modify→fill** 顺序：

```
旧顺序:  flush → journal_write(WAL) → triggers → seq → Volume 手动 drain → btree insert
新顺序:  flush → calc_size → journal_reserve → set_seq → triggers+btree_modify → journal_fill → release
                                                                  ↓
                                                     (btree 修改使用保留的 seq)
```

## 子任务结构

```
parent: 06-29-bcachefs-transaction-chain
├── Child-A: TC1+TC2  trans_commit 集成       [核心, TC3 的前置]
├── Child-B: TC3      统一 pin 管理            [依赖 Child-A]
├── Child-C: TC4+TC5  key cache 连接          [部分依赖 Child-A]
└── Child-D: TC6      journal 集成扩展          [独立]
```

**依赖关系**：
- Child-A → Child-B（必须先完成 journal_seq 传播 + btree 集成，才能安全移除 Volume 级 pin）
- Child-A 对 Child-C 是弱依赖（Phase 2 架构确定后可并行）
- Child-D 完全独立

---

## Child-A: trans_commit 集成 (TC1+TC2)

### 文件变更

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `btree/transaction.rs` | **修改** | `trans_commit` 重构为 4 阶段（reserve→modify→fill→release）；新增 `calc_journal_u64s`；`commit_with_engine` 新增 Phase 2 btree 修改；`commit_with_journal` 退役（序列化逻辑内联到 Phase 3） |
| `volume/mod.rs` | **修改** | `write_extent`/`delete_extent` 移除手动 drain 循环 + insert_entry 调用 |
| `btree/mod.rs` | **可能修改** | `insert_entry_raw` 可见性如需调整 |

### Step A1: trans_commit 核心流程 — reserve→modify→fill→release

**位置**: `btree/transaction.rs` `trans_commit()` 方法（行 ~1167）

将原有 `commit_with_journal()` 打包写法替换为 bcachefs 完全对齐的 4 阶段流程：

```rust
pub async fn trans_commit(
    &mut self,
    journal: &Journal,
    engine: &mut BtreeEngine,
    backend: &dyn BlockDevice,
) -> Result<u64, JournalError> {
    // Phase 0: flush key cache（sync point）
    engine.flush_cache_dirty_keys();

    // ★ Phase 1: 预计算并保留 journal 空间（bcachefs step 1）
    let total_u64s = self.calc_journal_u64s();
    if total_u64s > 0 {
        let res = journal.journal_res_get(Watermark::Normal, total_u64s)?;
        let seq = res.seq;
        self.journal_seq = seq;          // 注入 seq（TC1）
        
        // ★ Phase 2: btree 修改（bcachefs step 3）
        // 使用 self.journal_seq 作为所有 btree 节点修改的 seq
        // commit_with_engine 内部读取 self.journal_seq 并应用到 engine
        self.commit_with_engine(engine);

        // ★ Phase 3: 填充 journal（bcachefs step 4）
        for (bt, entries) in self.group_journal_by_btree() {
            let jset_entry = build_jset_entry(bt, &entries);
            let payload = bincode::serialize(&jset_entry)
                .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
            journal.add_entry(&res, &payload);
        }

        // ★ Phase 4: 释放保留（bcachefs step 5）
        journal.journal_res_put(&res);

        Ok(seq)
    } else {
        // 无 journal 条目 → 只运行触发器管线
        self.commit_with_engine(engine);
        Ok(0)
    }
}
```

**新增 calc_journal_u64s()**：
```rust
fn calc_journal_u64s(&self) -> u32 {
    // 粗略计算: 每个 journal 条目 ≈ 16 u64s
    let per_entry_overhead = 16u32;
    let total = self.journal.len() as u32 * per_entry_overhead;
    total.max(64)  // 至少 64 u64s（避免频繁 slowpath）
}
```

**关于 Phase 3 序列化的具体实现**：

Phase 3（`add_entry`）复用 `commit_with_journal()` 中现有的 `JsetEntry` 构建和 `bincode::serialize` 路径：

1. 提取 `commit_with_journal()` 中遍历 `self.journal`、按 btree 分组、构建 `JsetEntry`、`bincode::serialize` 的代码块
2. 将其移至 `trans_commit()` Phase 3 位置（内联到循环中）
3. `commit_with_journal()` 方法体保留但不被 `trans_commit()` 调用（标记 `#[deprecated]` 或保留给外部旧调用者）

**关于 `commit_with_journal()` 的处理**：
- 将 `commit_with_journal()` 标记为 `#[deprecated]` 并保持方法体不变
- 新的 `trans_commit()` 不再调用它（序列化逻辑已内联到 Phase 3）
- 外部有调用 `commit_with_journal()` 的地方不受影响（保持向后兼容）

**关于 btree 节点分裂的安全性**：
- 当前 volmount 的 `insert_entry_raw` 调用 `alloc`（bucket 分配器）而非 journal 来分配分裂节点空间
- 因此 btree 节点分裂**不消耗 journal 保留空间**，Phase 1 的粗略预计算足够安全
- 这是与 bcachefs 的关键差异点：bcachefs 中 btree split 需要额外 journal 保留（`bch2_trans_journal_res`），volmount 不需要

**验证**: `cargo build -p volmount-core` 通过

### Step A2: commit_with_engine 新增 Phase 2 — btree 修改

**位置**: `btree/transaction.rs` `commit_with_engine()`（行 ~468）

在 Gc 触发器之后、函数返回之前，新增 Phase 2（btree 节点修改）：

```rust
fn commit_with_engine(&mut self, engine: &mut BtreeEngine) -> bool {
    // 现有代码: sort_locks → try_lock_all → triggers → commit
    // ... (不变) ...

    // ★ NEW: Phase 2 — 应用 journal 条目到 btree 节点
    // 放在 committed 标记之后（不可回滚阶段）
    if self.journal_seq > 0 {
        for entry in self.journal.iter() {
            match entry.op {
                BtreeOp::Insert => {
                    engine.insert_entry_raw(
                        entry.btree,
                        entry.key.clone(),
                        entry.val.clone(),
                        self.journal_seq,
                    );
                }
                BtreeOp::Delete => {
                    engine.delete_entry_raw(entry.btree, &entry.key, self.journal_seq);
                }
                BtreeOp::Whiteout => {}  // whiteout 已在之前的路径中处理
            }
        }
    }
    true
}
```

关键设计决策：
- Phase 2 放在 committed 标记之后（不可回滚阶段），与 bcachefs 语义一致
- 使用 `insert_entry_raw`（绕过 key cache——TC5 会调整）
- journal 填充（Phase 3）在 Phase 2 之后——如果 btree 修改后崩溃，journal 条目从未写入 bucket，recovery 不会看到未应用的条目（线性化保证）

**验证**: `cargo build -p volmount-core` 通过

### Step A3: Volume 移除手动 drain

**位置**: `volume/mod.rs` `write_extent()`（行 ~534-590）和 `delete_extent()`（行 ~593-641）

**`write_extent()`** 现有 drain 代码（约行 572-585）：

```rust
// ★ 移除此段
// let jentries = trans.drain_journal();
// for entry in jentries {
//     match entry.op {
//         BtreeOp::Insert => {
//             self.engine.insert_entry(
//                 BtreeId::Extents,
//                 entry.key,
//                 entry.value,
//                 journal_seq,
//             );
//         }
//         ...
//     }
// }
```

替换为：

```rust
// trans_commit 内部完成全部 4 阶段（reserve→modify→fill→release）
let journal_seq = trans.trans_commit(journal, &mut self.engine, &*self.backend).await?;
// (TC3 移除 pin_add，暂时保留)
```

**`delete_extent()`** 同样移除 drain 循环。

**验证**: `cargo test -p volmount-core --lib` 通过（关键回归验证）

---

## Child-B: 统一 pin 管理 (TC3)

### 文件变更

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `volume/mod.rs` | **修改** | 移除 `journal_pin` 字段 + 3 处 pin 调用 |

**依赖**: 必须等 Child-A 合并后才可实施（drain 循环移除后 pin 管理语义变化）。

### Step B1: 移除 Volume.journal_pin 字段

搜索 `self.journal_pin` 的所有引用：

```bash
grep -n "journal_pin" crates/volmount-core/src/volume/mod.rs
```

预期 4 处：
1. 字段定义（`journal_pin: JournalEntryPin`）
2. `write_extent` 中 `journal.bch2_journal_pin_add(journal_seq, &self.journal_pin, None)`
3. `delete_extent` 中同上
4. `flush_dirty_nodes` 末尾 `journal.bch2_journal_pin_drop(&self.journal_pin)`

移除全部 4 处。如果 `journal_pin` 是 `Journal` 的构造函数参数或初始化字段，一并移除。

### Step B2: 清理初始化代码

如果 `Volume::new()` 或 `Volume::create()` 中初始化了 `journal_pin`，移除相关代码。

**验证**: `cargo build -p volmount-core` 通过 + `cargo test -p volmount-core --lib` 通过

---

## Child-C: key cache 连接 (TC4+TC5)

### 文件变更

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `btree/btree.rs` | **修改** | `get()`/`get_entry()` 增加 trigger_key_cache_miss；写入路径增加 insert_key_cached |
| `btree/transaction.rs` | **修改** | Phase 2 中 journal 条目应用到 key cache |
| `btree/key_cache.rs` | **修改** | 如需要，调整 insert_key_cached 接口 |

### Step C1: trigger_key_cache_miss 连接 (TC4)

**位置**: `btree/btree.rs` `get_entry()`（行 ~186-228）和 `get()`（行 ~162-198）

在缓存未命中、走 btree 搜索之前：

```rust
fn get_entry(
    &self,
    trans: &mut BtreeTrans,
    key: &BtreeKey,
    pos: &Bpos,
    snapshot: u32,
) -> Option<BtreeEntry> {
    // 先查缓存
    let cached = self.key_cache.find(pos);
    if let Some(entry) = cached {
        return Some(entry);
    }

    // ★ TC4: 缓存未命中 → 触发事务重启
    trans.trigger_key_cache_miss(self.id);

    // 走 btree 搜索
    let entry = self.search_btree(key, pos, snapshot)?;
    self.key_cache.insert(*pos, entry.clone());
    Some(entry)
}
```

**注意**: `trigger_key_cache_miss` 只在 BtreeTrans 可用时合理。`get()`/`get_entry()` 当前签名需要改为接受 `Option<&mut BtreeTrans>` 参数，或新增一个带 trans 的重载。

**设计决策**: 不改现有 `get()`/`get_entry()` 签名（保持向后兼容），新增 `get_with_trans()` / `get_entry_with_trans()` 变体，在事务上下文内调用时使用。

**验证**: `cargo test -p volmount-core --lib` 通过

### Step C2: Phase 2 使用 insert_key_cached (TC5)

**位置**: `btree/transaction.rs` `commit_with_engine()` Phase 2

将 `engine.insert_entry_raw(...)` 改为两阶段：

```rust
// Phase 2 — 应用 journal 条目
let seq = self.journal_seq;
for entry in self.journal.iter() {
    match entry.op {
        BtreeOp::Insert => {
            // 1. 写入 btree 节点
            engine.insert_entry_raw(ty, key.clone(), val.clone(), seq);
            // 2. 脏缓存（替代 invalidate）
            if seq > 0 {
                let kc = engine.get(ty).key_cache.bch2_btree_insert_key_cached(
                    entry.pos, entry.val.clone().into(), seq,
                );
            }
        }
        BtreeOp::Delete => {
            engine.delete_entry_raw(ty, key, seq);
            // key cache 方面，delete 也是脏操作
        }
        BtreeOp::Whiteout => {}
    }
}
```

**验证**: `cargo test -p volmount-core --lib` 通过

---

## Child-D: journal 集成扩展 (TC6)

### 文件变更

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `volume/mod.rs` | **修改** | `btree_insert()` 改为 async + BtreeTrans |
| `volume/mod.rs` + 调用方 | **修改** | 调用 `btree_insert` 的地方适配 await |

### Step D1: btree_insert 改为走 journal

**位置**: `volume/mod.rs` `btree_insert()`（行 ~520-524）

当前：
```rust
pub fn btree_insert(&mut self, key: BtreeKey, value: BchVal) -> bool {
    self.engine.insert_guarded(BtreeId::Extents, key, value, 0)
}
```

目标：
```rust
pub async fn btree_insert_with_journal(
    &mut self,
    key: BtreeKey,
    value: BchVal,
    journal: &Journal,
) -> Result<u64, StorageError> {
    let mut trans = BtreeTrans::default();
    trans.set_trigger_registry(self.trigger_registry.clone());
    trans.begin();
    trans.journal_insert(BtreeId::Extents, 0, false, key, value, 0);
    let seq = trans
        .trans_commit(journal, &mut self.engine, &*self.backend)
        .await
        .map_err(|e| StorageError::JournalError(e.to_string()))?;
    Ok(seq)
}
```

保留旧 `btree_insert()` 作为轻量同步版本（不写 journal），新方法供需要 crash-safe 的调用方使用。

**验证**: `cargo build` 全部 crate 通过

---

## 验证清单

### 增量验证

```bash
# Child-A Step A1 后
cargo build -p volmount-core

# Child-A Step A2 后
cargo build -p volmount-core

# Child-A Step A3 后
cargo test -p volmount-core --lib   # 关键回归：763 pass, 5 pre-existing
cargo test -p volmount-core --lib -- write_extent delete_extent  # 定向回归

# Child-B 后
cargo test -p volmount-core --lib
cargo test -p volmount-core --lib -- flush_dirty_nodes  # pin 相关定向测试

# Child-C 后
cargo test -p volmount-core --lib
cargo test -p volmount-core --lib -- key_cache  # key cache 定向测试

# Child-D 后
cargo build
```

### 最终全量验证

```bash
cargo build
cargo test -p volmount-core --lib
cargo test -p volmount-nbd --lib
cargo clippy --all-targets
cargo fmt --check
```

### 验收项

- [ ] **Child-A**: `trans_commit` 按 reserve→modify→fill→release 顺序执行；Volume 无手动 drain
- [ ] **Child-B**: Volume 无 journal_pin 字段；全部 pin 由节点级管理
- [ ] **Child-C**: `trigger_key_cache_miss` 在缓存未命中时调用；Phase 2 使用 insert_key_cached
- [ ] **Child-D**: `btree_insert` 可选走 journal
- [ ] 回归：763 test pass, 5 pre-existing, 0 new failures
- [ ] Clippy 无新增 warning

## 风险与回滚

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Phase 2 insert 失败导致 btree 不一致 | 低 | 高 | insert_entry_raw 在 COW 语义下幂等；WAL 已写入，recovery 可补齐 |
| trigger_key_cache_miss 引入事务死循环 | 中 | 中 | 重启循环有次数限制；测试覆盖无限重试防护 |
| key cache writeback 时序导致读取旧值 | 低 | 中 | mark_clean 在写回成功后清除 dirty；flush_cache_dirty_keys 在 sync point 触发 |

回滚策略：每个 Child 对应一次独立提交，可单独 revert。
