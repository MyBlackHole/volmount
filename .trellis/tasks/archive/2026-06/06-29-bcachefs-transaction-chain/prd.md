# bcachefs 事务全链路整合

## 目标

当前 volmount 的事务链各组件已基本实现（atomic reservation、multi-buffer pipeline、journal pin、key cache writeback、btree flush_dirty_nodes），但这些组件之间的连接存在断裂点：journal_seq 没有自动传递到 btree 修改路径、btree 状态更新在事务提交的外部手动完成、pin 管理两条路径并存。本任务的目标是将这些已实现的组件连接为一条完整的 bcachefs 对齐事务链路。

## 确认事实（代码审查）

### 已实现组件（全链路基础）

| 组件 | 状态 | 文件 |
|------|------|------|
| JournalResState (AtomicU64 CAS) | ✅ 完整实现 | `types.rs:386` |
| journal_res_get_fast + slowpath | ✅ 完整实现 | `types.rs:1039,2232` |
| 4-buffer 流水线 | ✅ 完整实现 | `types.rs:275-299` |
| journal_entry_open / close | ✅ 完整实现 | `types.rs:1262,1300` |
| add_entry / journal_res_put | ✅ 完整实现 | `types.rs:1149,1170` |
| bch2_journal_flush (10 步) | ✅ 完整实现 | `types.rs:1624` |
| PinListFifo + 6-way unflushed | ✅ 完整实现 | `reclaim.rs:124-196` |
| Full pin API (set/drop/put/add/update/copy/flush) | ✅ 完整实现 | `reclaim.rs:782-1147` |
| key cache writeback (dirty + pin + flush_dirty) | ✅ 完整实现 | `key_cache.rs:239-355` |
| key cache sync points (batch_write/insert_guarded) | ✅ 完整实现 | `mod.rs:353-477` |
| background reclaim + auto-flush | ✅ 完整实现 | `types.rs:2367-2412` |
| BtreeTrans journal 条目 | ✅ 完整对齐 | `transaction.rs:221`（a56c033） |
| btree flush_dirty_nodes | ✅ 完整实现 | `volume/mod.rs:674` |
| Recovery passes | ✅ bcachefs 对齐 | `recovery/passes/` |

### 组件间断裂点

#### TC1: journal_seq 不传递到 btree 修改路径

`trans_commit()`（`transaction.rs:1167`）调用流程：

```
trans_commit(journal, engine, backend):
  1. flush_cache_dirty_keys()              ← key cache → btree
  2. commit_with_journal() → returns seq   ← journal WAL 写入
  3. commit_with_engine(engine)            ← 只运行触发器管线，不修改 btree 节点
  → seq 返回给调用者，但未被 set_journal_seq() 注入
```

`BtreeTrans::journal_seq` 字段（`transaction.rs:144`）和 `set_journal_seq()` 方法（`transaction.rs:903`）**存在但从未在 `trans_commit()` 流程中使用**。

Volume 层在 `trans_commit()` 返回后手动 `drain_journal()` + `engine.insert_entry(..., journal_seq)`。这意味着 journal_seq 的手动传递是唯一的路径——如果调用方忘记传递，btree 节点就不知道 journal_seq。

#### TC2: btree 状态更新在事务提交之外

`commit_with_engine()`（`transaction.rs:468`）只做：
1. 锁排序 + try_lock_all
2. Phase B1 Transactional 触发器
3. 标记 committed + 降级写锁
4. Phase B1 Atomic 触发器
5. Phase B1 Gc 触发器

**它不调用 `engine.insert_entry()` 修改 btree 节点。** 实际的 btree 修改在 Volume 的 `write_extent`/`delete_extent` 中通过手动 drain 循环完成（`volume/mod.rs:577-585, 620-631`）。

这产生了时间窗口：WAL 写入后、btree 节点修改前，崩溃可能导致 WAL 中的条目尚未应用到 btree。

#### TC3: 两种 pin 管理机制并存

| 机制 | 注册时机 | 释放时机 | 位置 |
|------|----------|----------|------|
| Volume 级 pin | `write_extent`/`delete_extent` 后 `bch2_journal_pin_add()` | `flush_dirty_nodes` 末尾 `bch2_journal_pin_drop()` | `volume/mod.rs:570,614,715` |
| 节点级 pin | `bch2_btree_node_write*()` 时 `bch2_journal_pin_add()` | cache eviction 时 `bch2_journal_pin_drop()` | `io.rs:777-862`, `cache.rs` |

Volume 级 pin 是 volmount 特有的（bcachefs 无对应），粒度为整个 flush 批次。节点级 pin 对应 bcachefs 的 per-node pin。两者并存造成 pin 计数加倍，可能导致 last_seq 推进延迟。

#### TC4: trigger_key_cache_miss 未连接

`trigger_key_cache_miss()`（`transaction.rs:843`）定义了 `RestartReason::KeyCacheMiss` 设置逻辑，但**生产代码中从未被调用**——仅在 `#[cfg(test)]` 测试中使用。Bcachefs 中，当 key cache miss 需要从磁盘读取 btree node 时，事务重启触发读入后再重试。

#### TC5: bch2_btree_insert_key_cached 生产代码未使用

`bch2_btree_insert_key_cached()`（`key_cache.rs:239`）能创建 dirty cache 条目并注册 journal pin，但**只被单元测试调用**。现有写入路径（`btree.rs:insert_entry`/`insert`/`delete`）对所有写入调用 `key_cache.invalidate()` 击穿缓存，不走脏缓存路径。

这意味着 key cache 只能通过 `find()` → btree miss → `insert()` 读路径获得干净条目，写路径永远是写穿（write-through）而非写回（write-back）。

#### TC6: 部分写入路径绕过 journal

以下操作直接调用 engine 方法，不走 BtreeTrans + journal WAL：

| 方法 | 位置 | 绕过方式 |
|------|------|----------|
| `Volume::btree_insert()` | `volume/mod.rs:520` | 直接 `insert_guarded()` |
| `bch2_snapshot_node_create()` | `snap/mod.rs` | 直接 engine 操作 |
| `bch2_subvolume_create()` | `subvol/ops.rs` | 直接 engine 操作 |

在 bcachefs 中，所有 btree 修改都通过 journal 进行。

## 需求

### TC1: journal_seq 自动传播到 btree 修改（P0 — 必须）

- `trans_commit()` 内部分 4 阶段：reserve → modify → fill → release，见 design.md
- Phase 1 通过 `journal_res_get()` 保留空间并获取 seq，注入 `self.journal_seq`
- Phase 2 `commit_with_engine()` 使用 `self.journal_seq` 修改 btree 节点
- Phase 3 `add_entry()` 填充 journal 到已保留空间
- Phase 4 `journal_res_put()` 释放保留（refcount→0 自动触发写）
- 消除 Volume 层的手动 `drain_journal()` + `engine.insert_entry()` 循环

**验收**：
- [ ] `trans_commit()` 按 reserve→modify→fill→release 顺序执行
- [ ] `BtreeTrans::journal_seq` 在 Phase 1 的 `journal_res_get()` 后被正确设置
- [ ] `commit_with_engine()` 在 committed 后使用 `journal_seq` 调用 `insert_entry_raw()` / `delete_entry_raw()` 修改 btree 节点
- [ ] Phase 3 `add_entry()` 将序列化后的 journal 条目写入已保留空间
- [ ] Phase 4 `journal_res_put()` 正确释放保留（refcount→0 触发写）
- [ ] Volume 的 `write_extent`/`delete_extent` 移除手动 drain 循环
- [ ] 所有现有测试保持 pass

### TC2: btree 状态更新集成到 trans_commit 内（P0 — 必须）

- `commit_with_engine()` 新增 Phase 2：遍历 `self.journal` 条目 → 调用 `engine.insert_entry_raw()`
- 每个条目使用 `self.journal_seq` 作为 journal_seq 参数
- Phase 2 放在 Atomic 触发器之后（不可回滚阶段）
- 保持现有的 `trigger_registry` 管线不变
- `commit_with_journal()` 退役（不再由 `trans_commit()` 调用），序列化逻辑内联到 Phase 3

**验收**：
- [ ] `commit_with_engine()` 在 Phase B1 Gc 后执行 Phase 2（应用 journal）
- [ ] 每个 engine.insert_entry_raw 使用正确的 journal_seq
- [ ] 现有触发器测试全部 pass

### TC3: 统一 pin 管理（P0 — 必须）

- 消除 Volume 级显式 pin 管理
- 依赖节点级 embedded pin（`BtreeNode.journal_pin`）完成全部 pin 语义
- `volume/mod.rs` 中移除 `bch2_journal_pin_add` 和 `bch2_journal_pin_drop` 调用
- `flush_dirty_nodes()` 不再管理 pin，只需 flush 节点

**验收**：
- [ ] `volume/mod.rs` 中移除全部 `self.journal_pin` 引用
- [ ] 节点级 pin 在 `bch2_btree_node_write*()` 时正确注册
- [ ] 节点级 pin 在 cache eviction 时正确释放
- [ ] last_seq 推进不受影响

### TC4: trigger_key_cache_miss 连接（P1 — 推荐）

- `Btree::get()` / `Btree::get_entry()` 缓存未命中时调用 `trigger_key_cache_miss()`
- 当需要从磁盘读取 btree node 时，设置 `RestartReason::KeyCacheMiss` 触发事务重启
- 重启后 `trans_commit()` 重新执行（从磁盘读入 node 后再试）

**验收**：
- [ ] `btree.rs` 的 `get()`/`get_entry()` 缓存未命中时调用 `trigger_key_cache_miss()`
- [ ] 事务重启后 cache 被正确填充

### TC5: bch2_btree_insert_key_cached 连接（P1 — 推荐）

- BtreeTrans 的写入路径（`trans_commit` Phase 2）调用 `bch2_btree_insert_key_cached()` 而非 `invalidate()` 将脏条目放入 key cache
- 同步点（`batch_write`/`insert_guarded`/`flush_cache_dirty_keys`）时 flush 脏 cache 条目到 btree
- 现有 `invalidate()` 路径保留作为干净回退

**验收**：
- [ ] `trans_commit` Phase 2 中 key cache 操作使用 `insert_key_cached` 而非 `invalidate`
- [ ] `flush_cache_dirty_keys()` 在 trans_commit Phase 0 中正常 flush 脏条目
- [ ] journal pin callback 触发 write-back

### TC6: 缺失的 journal 集成（P2 — 可选）

- `Volume::btree_insert()` 改为通过 `BtreeTrans` + `trans_commit()` 走 journal
- snapshot/subvol 操作的 journal 集成（评估是否必要，考虑兼容性）

**验收**：
- [ ] `btree_insert()` 走 journal WAL
- [ ] 部分 snapshot/subvol 操作增加 journal 记录

## 排除范围

- 多线程并发控制（`&mut self` → 内部锁）—— 架构设计决策，不在本任务内
- noflush journal 优化（SQLite 模式）—— journal-design.md §5.3 已排除
- 动态 buf 大小调整——同上
- 多设备 journal——同上
- Journal watermark 7 级完整实现——保留当前简化实现
- 完整的 bcachefs reclaim thread 模型——已有 bg reclaim task

## 验收标准

### 功能验收

- [ ] **TC1**：journal_seq 自动传播到 btree 修改路径
- [ ] **TC2**：btree 状态更新集成到 trans_commit 内
- [ ] **TC3**：pin 管理统一（Volume 级 pin 消除）
- [ ] **TC4**：trigger_key_cache_miss 连接（如果有需要）
- [ ] **TC5**：bch2_btree_insert_key_cached 连接（如果有需要）
- [ ] **TC6**：缺失的 journal 集成（如果有需要）

### 质量验收

- [ ] `cargo build` 0 errors（全部 crate）
- [ ] `cargo test -p volmount-core --lib` 763 passed, 5 pre-existing（0 新增失败）
- [ ] `cargo clippy --all-targets` 无新增 warning
- [ ] `cargo fmt --check` clean

### 集成验证

- [ ] 全链路集成测试：journal_res_get → add_entry → journal_res_put → bch2_journal_flush → flush_pins → btree modify → mark_clean
- [ ] 事务提交后 journal seq 正确反映在 btree node 的 journal_seq 字段中
- [ ] flush_dirty_nodes 后 journal pin 正确释放，last_seq 正常推进

## 开放问题

1. **TC1+TC2 的范围**：Volume 层移除手动 drain 循环后，`write_extent` 和 `delete_extent` 的结构会大幅简化——journal pin add 也一并移除（TC3）。这是否意味着 `self.journal_pin` 字段可以从 Volume 中完全移除？
2. **TC4+TC5 是否选做**：key cache writeback 的脏条目路径是 read-write 工作负载的关键优化，但当前测试仅覆盖了直写（write-through）路径。如果 `insert_key_cached` 连接后发现无法恢复旧 invalidate 语义，是否保留回退方案？
3. **TC6 的优先级**：btree_insert、snapshot、subvol 操作不走 journal——这是否是已知问题（数据一致性受影响）？

## 实施顺序建议

1. **TC1 + TC2**（合并实施）—— journal_seq 传播 + btree 状态集成到 trans_commit -> 消除手动 drain
2. **TC3**（pin 管理统一）—— 消除 Volume 级 pin，依赖节点级 pin
3. **TC4 + TC5**（key cache 连接）—— trigger_key_cache_miss + insert_key_cached
4. **TC6**（journal 集成扩展）—— 补全遗漏的写入路径
