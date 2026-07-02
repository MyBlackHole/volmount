# Journal P1 — 实施计划

## Phase 0: 规划（当前）
- [x] PRD（需求 + AC）
- [x] design.md（技术设计）
- [ ] implement.md（实施计划）← 当前
- [ ] implement.jsonl（子代理上下文清单）
- [ ] check.jsonl（验证清单）
- [ ] 用户确认 → `task.py start`

## Phase 1: 实现步骤

### Step 1: 新增数据结构（journal/types.rs）
- 新增 `JournalResState`（AtomicU64 位域封装）
- 新增 `JournalBuf` + `BufState`
- 新增 `JournalReservation`
- `JOURNAL_STATE_BUF_NR = 4`（与 bcachefs 一致）
- `BUF_SIZE = 4096 * 8` (32KB u64s = 256KB)

### Step 2: 重构 `Journal` 结构（journal/types.rs）
- 替换 `pending: Vec<Jset>` → `buf: [JournalBuf; BUF_NR]`
- 替换 `last_seq: u64` → `seq: AtomicU64`
- 添加 `reservations: JournalResState`、`ring: [AtomicPtr; BUF_NR]`、`in_flight: VecDeque<u32>`
- 保留所有 buckets/bucket_seq/四索引字段

### Step 3: 实现核心 fastpath（journal/types.rs）
- `journal_res_get_fast()` — CAS 循环
- `journal_res_put()` — atomic_dec buf_count → 归零触发写入
- `__journal_entry_open_one()` — 切换 buf
- `__journal_entry_close_one()` — 关闭当前 buf
- `buf_write()` — 填充 buf.data 数据
- `buf_flush()` — 将 buf 写入 block device

### Step 4: 更新 BtreeTransaction 集成（btree/transaction.rs）
- 从 `drain_journal()` → `journal_res_get_fast()` + `buf_write()` + `journal_res_put()`
- BtreeTransaction 接受 `&Journal` 引用（共享引用，不再需要 `&mut`）

### Step 5: 重构公开接口
- 按 design.md §4 设计新接口
- `append()` → `reserve()` + `commit()` 分离
- `flush()` → `wait_all_done()`
- 更新所有调用者（JournalReplayer、daemon 层等）

### Step 6: 测试
- 单元测试保留和 refcount 管理
- 多 buffer 写入并发测试
- 现有测试全部通过

## Phase 2: 审查
- trellis-check 验证 bcachefs 引用行号
- 验证并发安全性

## Phase 3: trellis-update-spec
- 更新 quality-guidelines.md Journal 设计规范

## Phase 4: 提交

## 风险文件
- `crates/volmount-core/src/journal/types.rs` — 核心重构（900+ 行）
- `crates/volmount-core/src/journal/mod.rs` — 公开接口变更
- `crates/volmount-core/src/journal/jset.rs` — 可能需更新序列化
- `crates/volmount-core/src/recovery/passes/journal_replay.rs` — 如果 replay 依赖 pending

## 验证命令
- `cargo test -p volmount-core` — 全部测试通过
- `cargo clippy -p volmount-core` — 无新警告
