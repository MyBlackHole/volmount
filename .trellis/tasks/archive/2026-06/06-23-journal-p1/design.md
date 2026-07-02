# Journal 原子保留状态 + 多 Buffer 流水线 — 设计

## 1. 核心数据结构变更

### 1.1 `JournalReservation`（新增 — 对应 bcachefs `struct journal_res`）

```rust
/// Journal 保留结果（bcachefs struct journal_res）
/// uninit → reserved → committed/freed
pub struct JournalReservation {
    pub seq: u64,
    pub offset: u32,      // 在 buf.data 中的偏移（u64 单位）
    pub u64s: u32,        // 保留的 u64 数
}
```

### 1.2 `JournalBufState`（新增 — per-buffer 生命周期）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufState {
    Free,              // 可复用
    Accepting,         // 正在接收保留
    Closing,           // 关闭中（不再接收新保留）
    WriteSubmitted,    // 已提交写入
    WriteDone,         // 写入完成，等待回收
}
```

### 1.3 `JournalBuf`（新增 — 对应 bcachefs `struct journal_buf`）

```rust
pub struct JournalBuf {
    pub state: BufState,
    pub data: Vec<u8>,                          // 缓冲区数据
    pub size: usize,                            // 分配大小（u64 单位）
    pub seq: u64,                               // 此 buf 的起始 seq
    pub waiters: Mutex<Vec<oneshot::Sender<()>>>, // 等待者（线程安全）
}
```

### 1.4 `JournalResState`（新增 — 对应 bcachefs `union journal_res_state`）

```rust
/// 64-bit 原子保留状态（bcachefs journal_res_state）
/// 位域布局（与 bcachefs 一致）：
///   [0..22)  cur_entry_offset — 当前 entry 中已保留的 u64 数
///   [22..24) idx — 当前开放的 journal buffer 索引
///   [24..34) buf0_count — buf[0] 保留计数
///   [34..44) buf1_count
///   [44..54) buf2_count
///   [54..64) buf3_count
#[repr(C)]
pub struct JournalResState {
    bits: AtomicU64,
}
```

### 1.5 `Journal` 结构变更

```rust
pub struct Journal {
    // ★ 新增
    reservations: JournalResState,   // 原子保留状态（无锁 fastpath）
    buf: [JournalBuf; BUF_NR],       // 多 buffer
    seq: AtomicU64,                  // 无锁 seq 分配
    ring: [AtomicPtr<JournalBuf>; BUF_NR],  // seq → buf 映射缓存
    in_flight: VecDeque<u32>,        // 在途 buf 索引

    // ★ 保留（已有）
    pub buckets: Vec<JournalBucketState>,
    pub bucket_seq: Vec<u64>,
    pub current_bucket: usize,
    pub current_offset: u32,
    pub remaining_bytes: u32,
    pub last_seq_ondisk: u64,
    pub discard_idx: usize,
    pub dirty_idx: usize,
    pub dirty_idx_ondisk: usize,
    pub pin_count: u64,

    // ★ 移除
    // last_seq: u64        → 用 seq: AtomicU64 替代
    // pending: Vec<Jset>   → 用 buf[] 替代
}
```

## 2. 关键数据流

### 2.1 Fastpath: 无锁保留

```
journal_res_get_fast(res)
  │
  ├─ atomic64_read(&reservations)
  │
  ├─ CAS 循环:
  │    ├─ cur_entry_offset + u64s ≤ 当前 buf 容量?   ← 空间检查
  │    ├─ watermark 检查（Phase 2，当前跳过）
  │    ├─ 递增 bufN_count                            ← refcount
  │    └─ cur_entry_offset += u64s                    ← 分配空间
  │
  ├─ 获取 seq: journal_cur_seq(j)
  │   (seq 在 __journal_entry_open_one() 中分配，
  │    同一 buf 的所有 reservation 共享相同 seq)
  │
  ├─ res.seq = seq
  │  res.offset = old.cur_entry_offset (CAS 前的偏移)
  │  res.u64s = request_u64s
  │  res.buf_idx = old.idx
  │
  └─ 返回 success
```

> **重要**: seq 不是在 fastpath 中分配的。seq 在 `__journal_entry_open_one()` 打开新 buf 时通过 `atomic64_inc(&j->seq)` 递增。同一 buf 接受的所有 reservation 共享相同 seq（通过 `journal_cur_seq(j)` 读取）。对应 bcachefs `journal.h:511` `res->seq = journal_cur_seq(j)`。

### 2.2 Buffer 生命周期

```
buf[idx] Free ──→ buf[idx] Accepting（通过 __journal_entry_open_one）
                   │
                   ├─ journal_res_get_fast() 成功 → 保留空间
                   │  （多个线程可同时保留到此 buf）
                   │
                   ├─ 任何条件之一触发关闭:
                   │    ├─ buf 空间不足 (cur_entry_offset >= max)
                   │    ├─ 显式 close_entry
                   │    └─ 定时器到期
                   │
                   buf[idx] Closing → 不再接受新保留
                   │
                   ├─ 等待 buf_count 归零（所有保留者完成提交）
                   │
                   buf[idx] WriteSubmitted → bch2_journal_buf_put_final
                   │   触发写入到 block device
                   │
                   buf[idx] WriteDone
                   │
                   └─ 返回 Free（可被 __journal_entry_open_one 复用）
```

### 2.3 与 BtreeTransaction 集成

```rust
// 事务提交时:
fn commit(&mut self, journal: &Journal) -> Result<()> {
    // 1.  journal_res_get_fast() — 在全局 Journal 上保留空间
    //    返回 JournalReservation { seq, offset, u64s, buf_idx }
    let res = journal.journal_res_get_fast(self.estimated_u64s())?;

    // 2. commit(res, data) — 将 btree 修改写入 buf.data
    //    Journal 内部通过 res.buf_idx 定位 buf，
    //    UnsafeCell 保证 data 写入的并发安全
    //    （每个 reservation 的 offset 唯一，无写入冲突）
    journal.commit(&res, &self.journal);

    // 3. journal_res_put() — 释放 buf refcount
    //    → refcount 归零自动触发 buf 写入
    journal.journal_res_put(&res);

    Ok(())
}
```

> **并发安全**：`commit()` 通过 `JournalReservation.buf_idx` 定位目标 buf，再通过 `res.offset` 确定写入位置。由于每个 reservation 的 offset 由 CAS 保证全局唯一，写入无冲突。Journal 内部使用 `UnsafeCell<[JournalBuf; BUF_NR]>` 实现 buf 数组的共享可变访问，通过 `buf[idx].data[offset..]` 直接写入。

## 3. 配置

```rust
/// JOURNAL_STATE_BUF_NR = 4，与 bcachefs 一致。
/// 支持最多 4 个未完成写入同时存在。
pub const JOURNAL_STATE_BUF_NR: usize = 4;
pub const BUF_SIZE: usize = 32768; // 32KB per buf（4096 u64 × 8 bytes）
```

## 4. 接口设计

按新设计重构公开接口，不保留旧接口签名：

| 旧方法 | 新方法 |
|---------|--------|
| `reserve_seq()` | 删除 — 由 `seq.fetch_add(1, Relaxed)` 替代 |
| `append(ty, entries) → Result<u64>` | `reserve(req_u64s) → JournalReservation` + `commit(res, data)` |
| `append_btree_root(ty, root_addr)` | 同上 |
| `flush()` | `wait_all_done()` — 等待所有 buf 写入完成 |
| `read_bucket()` | ✅ 不变 |
| `iter_entries()` | ✅ 不变 |
| `reclaim()` | ✅ 不变 |
| `rotate_or_reclaim()` | ✅ 不变 |

`Journal` 接受 `&self` 在 fastpath 中（`journal_res_get_fast` 只读 atomic），慢路径用内部同步。

## 5. bcachefs 源码参考

| 概念 | bcachefs 文件:行号 |
|------|-------------------|
| `union journal_res_state` | `fs/journal/types.h:142-174` |
| `struct journal_res` | `fs/journal/types.h:134-140` |
| `journal_res_get_fast()` | `fs/journal/journal.h:475-518` |
| `journal_state_inc()/dec()` | `fs/journal/journal.h` inline |
| `JOURNAL_STATE_BUF_NR` | `fs/journal/types.h:20-22` |
| `struct journal_buf` | `fs/journal/types.h:37-76` |
| `struct journal` (f32) | `fs/journal/types.h:246-421` |
| `__journal_entry_open_one()` | `fs/journal/journal.c:391` |
| `__journal_entry_close_one()` | `fs/journal/journal.c:276` |
| `__bch2_journal_buf_put_final()` | `fs/journal/journal.c:240-256` |
| `bch2_journal_do_writes_locked()` | `fs/journal/journal.c` |
| `ring[seq & mask]` | `fs/journal/types.h:293` |

## 6. 不变式

- `buf[idx].state == Accepting` → `buf[idx]` 可接受新保留，其他 buf 在 Closing/WriteSubmitted/Free 状态
- 同一时刻只有一个 buf 在 Accepting 状态
- `buf[idx].state == Closing` → 不再接受新保留，等待 refcount 归零
- `buf[idx].state == Free` → 可被 `open_entry` 复用
- `idx` = 当前 Accepting buf 的索引（从 `reservations.idx` 读取）
- `buf[0..BUF_NR-1]` 循环轮换（round-robin），4 个 buffer 支持最多 4 个未完成写入
