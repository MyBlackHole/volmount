# bcachefs 事务全链路整合 — 技术设计

## 架构概览

当前事务链的组件已就位但连接断裂，且 journal ↔ btree 的顺序与 bcachefs 相反。本设计将整个事务流重构为 bcachefs 完全对齐的顺序：**先保留(Reserve)→再修改(Modify)→最后填充(Fill)**。

---

## 核心顺序重构：bcachefs vs 当前 vs 目标

### bcachefs 事务顺序

```
bch2_trans_commit():
  1. journal_res_get(&res)           保留空间（CAS, &self, 无锁）
  2. bch2_trans_commit_get_locks()   获取锁
  3. btree insert/modify key         修改 btree 节点
     → node.journal_seq = res.seq    使用保留的 seq
  4. bch2_trans_journal_add_entry()  填充 journal 条目到保留空间
  5. bch2_journal_res_put(&res)      释放 refcount→0 → 自动触发写入
```

Journal 保留(step 1)在 btree 修改(step 3)和填充(step 4)之前。

### 当前 volmount 顺序

```
trans_commit():
  1. flush_cache_dirty_keys()
  2. commit_with_journal()           先完整写入 WAL（reserve+fill+release 打包）
  3. commit_with_engine()            再运行触发器管线（不改 btree 节点）
  4. seq → Volume 手动 drain → btree insert（操作在事务外完成）
```

Journal 在 btree 修改之前完整写入——与 bcachefs 相反。btree 修改不在事务内部。

### 目标顺序（bcachefs 完全对齐）

```
trans_commit():
  1. flush_cache_dirty_keys()
  2. calc_journal_u64s()             预计算 journal 条目大小
  3. journal_res_get(&res)           先保留空间 → 获得 seq  ←★ 移至最前
  4. self.journal_seq = res.seq      注入 seq
  5. commit_with_engine(engine)      修改 btree 节点（使用 seq）
     → 节点.journal_seq = seq
  6. add_entry(&res, serialized)     填充 journal 到保留空间  ←★ 移到最后
  7. journal_res_put(&res)           释放 refcount（→0 自动触发写）
```

---

## TC1+TC2: trans_commit 到 bcachefs 顺序

### 新 trans_commit 流程

```rust
pub async fn trans_commit(
    &mut self,
    journal: &Journal,
    engine: &mut BtreeEngine,
    backend: &dyn BlockDevice,
) -> Result<u64, JournalError> {
    // Phase 0: flush key cache（sync point）
    engine.flush_cache_dirty_keys();

    // Phase 1: 预先计算并保留 journal 空间（bcachefs step 1）
    // 此时还不知道确切序列化大小，用预估值
    let total_u64s = self.calc_journal_u64s();
    if total_u64s > 0 {
        let res = journal.journal_res_get(Watermark::Normal, total_u64s)?;
        let seq = res.seq;
        self.journal_seq = seq;          // ← 注入 seq

        // Phase 2: btree 修改（bcachefs step 3）
        // 使用 self.journal_seq 作为所有 insert_entry 的 seq
        self.commit_with_engine(engine);

        // Phase 3: 填充 journal（bcachefs step 4）
        // 将 journal 条目写入已保留的 buf 空间
        for (bt, entries) in self.group_journal_by_btree() {
            let jset_entry = build_jset_entry(bt, &entries);
            let payload = bincode::serialize(&jset_entry)
                .map_err(|e| JournalError::Io(StorageError::Serialization(e)))?;
            journal.add_entry(&res, &payload);
        }

        // Phase 4: 释放保留（bcachefs step 5）
        // refcount→0 → 自动推进到 WriteSubmitted
        journal.journal_res_put(&res);

        Ok(seq)
    } else {
        // 无 journal 条目 → 只运行触发器
        self.commit_with_engine(engine);
        Ok(0)
    }
}
```

### 序列化拆分

`commit_with_journal()` 不再作为独立方法由 `trans_commit()` 内部调用。其序列化逻辑（分组、构建 JsetEntry）被内联到 Phase 3。

### calc_journal_u64s()

新的辅助方法，在 Phase 1 中预计算所需 journal 空间：

```rust
impl BtreeTrans {
    fn calc_journal_u64s(&self) -> u32 {
        // 每个 jset_entry 的 overhead:
        //   JsetEntry header (btree_type + entry_type + has_last + has_prev) ≈ 8 u64s
        //   序列化后的 btree_keys ≈ num_keys * avg_key_size
        // 粗略计算: 每个条目 ≈ 16 u64s（后续可精确）
        let per_entry_overhead = 16u32;
        let total = self.journal.len() as u32 * per_entry_overhead;
        total.max(64) // 至少 64 u64s（避免频繁 slowpath）
    }
}
```

> **注意**：当前使用粗略计算（16 u64s/entry），因为 pre-size 不需要精确——`journal_res_get` 如果预留不足会通过 slowpath 补充。后续可优化为精确计算。

### commit_with_engine 改造

Phase 2 复用现有的 `commit_with_engine()`，在其中新增 btree 修改步骤：

```rust
fn commit_with_engine(&mut self, engine: &mut BtreeEngine) {
    // 现有代码: sort_locks → try_lock_all → triggers → commit
    // ... (不变) ...

    // ★ NEW Phase 2: 应用 journal 条目到 btree
    if self.journal_seq > 0 {
        for entry in self.journal.iter() {
            match entry.op {
                BtreeOp::Insert | BtreeOp::Delete => {
                    engine.insert_entry_raw(
                        entry.btree,
                        entry.key.clone(),
                        entry.val.clone(),
                        self.journal_seq,
                    );
                }
                BtreeOp::Whiteout => {}
            }
        }
    }
}
```

Phase 2 放在 committed 标记之后（Atomic 触发器之后），因为此时事务不可回滚。这与 bcachefs 语义一致：journal 保留已完成，btree 修改虽然发生，但如果后续填充失败，journal 写入不会触发（res_put 不释放/refcount 未归零），recovery 时不会看到未填充的条目。

> **线性化论证**：btree 修改在填充之前——如果填充失败，journal 条目从未写入 bucket。崩溃后 journal replay 看不到部分填充的条目，因此不会有"WAL 已记录但 btree 未修改"的情况。这是 bcachefs 顺序的正确性保证。

### 波及文件

| 文件 | 改动 |
|------|------|
| `btree/transaction.rs` | `trans_commit` 重构为 Phase 1-4；`commit_with_journal` 退役（逻辑内联）；`calc_journal_u64s` 新增；`commit_with_engine` 新增 Phase 2 |
| `volume/mod.rs` | `write_extent`/`delete_extent` 移除手动 drain 循环 |
| `btree/mod.rs` | `insert_entry_raw` 可能需要改为 `pub(crate)` 或保持现有可见性 |

---

## TC3: 统一 pin 管理

### 当前问题

Volume 级 pin（在 `write_extent`/`delete_extent` 中 add，在 `flush_dirty_nodes` 末尾 drop）与节点级 embedded pin（在 `bch2_btree_node_write` 时 add，在 cache eviction 时 drop）两条路径并存。Volume 级 pin 粒度为整个 flush 批次，导致 pin 计数加倍、last_seq 推进延迟。

### 目标设计

移除 Volume 级显式 pin 管理，只保留节点级 embedded pin。

**移除**（`volume/mod.rs`）：
- `Volume.journal_pin` 字段
- `write_extent()` 中的 `journal.bch2_journal_pin_add(journal_seq, &self.journal_pin, None)`
- `delete_extent()` 中的同上
- `flush_dirty_nodes()` 末尾的 `journal.bch2_journal_pin_drop(&self.journal_pin)`

**保持**：
- `BtreeNode.journal_pin` 字段（`node.rs:262`）
- `bch2_btree_node_write*()` 中的 `bch2_journal_pin_add()`（`io.rs:777-862`）
- cache eviction 路径中的 `bch2_journal_pin_drop()`（`cache.rs` 多处）

**安全性论证**：
- 节点级 pin 已覆盖语义：每个脏节点在首次写回后端时注册 pin，cache eviction 时释放。与 bcachefs 完全一致。
- Volume 级 pin 是冗余：它 pin 住整个 journal entry（可能包含多个 btree 修改），而非具体节点。当该 entry 中所有节点都已刷回时，节点级 pin 已全部释放。
- `last_seq` 推进不受影响：`bch2_journal_maybe_update_last_seq()` 检查 pin FIFO 前端 `count==0` 来推进。消除冗余 pin 后推进更快。

### 波及文件

| 文件 | 改动 |
|------|------|
| `volume/mod.rs` | 移除 `journal_pin` 字段 + 3 处 pin 调用 |

---

## TC4: trigger_key_cache_miss 连接

### 设计

在 `Btree::get_entry()` 的缓存未命中路径中调用 `trigger_key_cache_miss()`：

```rust
// get_entry 缓存未命中时触发事务重启
fn get_entry_with_trans(
    &self,
    trans: &mut BtreeTrans,
    key: &BtreeKey,
    pos: &Bpos,
    snapshot: u32,
) -> Option<BtreeEntry> {
    // 查缓存
    if let Some(entry) = self.key_cache.find(pos) {
        return Some(entry);
    }
    // ★ 缓存未命中 → 触发事务重启
    trans.trigger_key_cache_miss(self.id);
    // 走 btree 搜索
    let entry = self.search_btree(key, pos, snapshot)?;
    self.key_cache.insert(*pos, entry.clone());
    Some(entry)
}
```

新增 `get_entry_with_trans()` / `get_with_trans()` 变体（不改变现有签名）。`trigger_key_cache_miss` 仅在 Phase B1 Transactional 触发器之前调用，此时事务尚未 committed，重启可安全回退。

### 波及文件

| 文件 | 改动 |
|------|------|
| `btree/btree.rs` | 新增 `get_entry_with_trans()`、`get_with_trans()` |

---

## TC5: bch2_btree_insert_key_cached 连接

### 设计

`trans_commit` Phase 2 中，btree 修改完成后，将脏条目写入 key cache：

```rust
// Phase 2 — btree 修改
for entry in self.journal.iter() {
    match entry.op {
        BtreeOp::Insert | BtreeOp::Delete => {
            engine.insert_entry_raw(ty, key, val, self.journal_seq);
            // ★ 脏缓存（替代 invalidate）
            if self.journal_seq > 0 && entry.cached {
                engine.get(ty).key_cache.bch2_btree_insert_key_cached(
                    entry.pos, entry.val.clone().into(), self.journal_seq,
                );
            }
        }
    }
}
```

`entry.cached` 是 `BtreeTransEntry` 的字段（Commit a56c033 已添加），指示该条目是否应写入 key cache。

脏缓存条目通过 `flush_cache_dirty_keys()` 写回 btree（sync point 时触发）。

### 波及文件

| 文件 | 改动 |
|------|------|
| `btree/transaction.rs` | Phase 2 增加 `insert_key_cached` 调用 |

---

## TC6: Journal 集成扩展

### 设计

**`Volume::btree_insert()`** → 新增 async 版本：

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

保留旧 `btree_insert()` 作为轻量同步版本。

### 波及文件

| 文件 | 改动 |
|------|------|
| `volume/mod.rs` | 新增 `btree_insert_with_journal()` |

---

## 依赖关系

```
Child-A (TC1+TC2)     trans_commit 顺序重构
  │
  ├─→ Child-B (TC3)   统一 pin 管理
  │                    依赖：必须等 Child-A 合并后才可安全移除 Volume 级 pin
  │
  ├─→ Child-C (TC4+TC5)  key cache 连接
  │                    依赖：需 Phase 2 架构确定
  │
  └─→ Child-D (TC6)    journal 集成扩展
                      依赖：无（独立）
```

---

## 回滚策略

每个 TC 独立提交。回滚为纯代码还原（无数据迁移、无 schema 变更）。

| TC | 回滚方式 |
|----|---------|
| TC1+TC2 | 恢复 `commit_with_journal()` + Volume 手动 drain |
| TC3 | 恢复 `Volume.journal_pin` 字段 + 3 处 pin 调用 |
| TC4 | 移除 `get_entry_with_trans()` |
| TC5 | 恢复 `invalidate()` |
| TC6 | 移除 `btree_insert_with_journal()` |

