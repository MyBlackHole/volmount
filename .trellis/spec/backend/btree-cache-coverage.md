# BtreeCache — 缓存模块覆盖地图

> 生成日期: 2026-06-30 (P2 更新)
> 源文件: `crates/volmount-core/src/btree/cache.rs` (1824 行)
> 参考实现: bcachefs `fs/btree/cache.c` + `fs/btree/cache.h`

## 覆盖统计

| 状态 | 数量 | 说明 |
|------|------|------|
| ✅ | 32 | 完全对齐 |
| ⚠️ | 8 | 部分对齐 |
| ❓ | 3 | bcachefs 有但 volmount 无 |
| ➖ | 13 | volmount 特有 |
| **总计** | **56 (volmount)** | + 3 bcachefs 未实现 |

## 函数状态表

### 生命周期
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `new()` | `bch2_fs_btree_cache_init_early` | ⚠️ 合并初始化 |
| `with_journal()` | — | ✅ volmount 扩展 |
| `bch2_btree_node_mem_free()` | `bch2_btree_node_mem_free()` | ✅ Arc 生命周期收口 |
| `bch2_btree_node_transition_state()` | `bch2_btree_node_transition_state()` | ✅ cache-side thin wrapper |
| `bch2_btree_node_transition_state_locked()` | `bch2_btree_node_transition_state_locked()` | ✅ cache-side thin wrapper |
| `bch2_btree_node_write_done_clean()` | `bch2_btree_node_write_done_clean()` | ✅ write 完成收口 |
| `bch2_fs_btree_evicted_size_init()` | `bch2_fs_btree_evicted_size_init()` | ⚠️ HashMap 预留而非 kernel table |
| `bch2_fs_btree_evicted_size_exit()` | `bch2_fs_btree_evicted_size_exit()` | ✅ 清空 evicted size 生命周期 |
| `len()` / `is_empty()` | `btree_cache_list_nr` | ✅ |

### 节点查找/加载
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `get_or_load()` | `bch2_btree_node_get` (部分) | ⚠️ 缺 trans/iter; ✅ 支持 InFlight 等待与 accessed 刷新 |
| `bch2_btree_node_get()` | `bch2_btree_node_get` | ⚠️ 缺 mem_ptr; 命中时刷新 accessed |
| `bch2_btree_node_evict()` | `bch2_btree_node_evict` | ✅ 先等待 read/write in-flight 再移除 |
| `get()` | `btree_cache_find` | ✅ |

### 脏节点管理
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `mark_dirty()` | — | ✅ auto-flush 不丢数据 |
| `bch2_btree_node_set_dirty()` | `bch2_btree_node_set_dirty` | ✅ 使用 `NODE_NEED_REWRITE` 表示需要写回，写完成入口清理 |
| `flush_dirty()` | — | ✅ 拓扑排序 P0-2 |
| `insert_dirty()` / `insert()` | — | ✅ |

### Shrinker
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `shrink()` | `bch2_btree_cache_scan` | ⚠️ 两阶段 clock ✅，缺 MM 集成；已由 alloc 前 self-reclaim 调用 |
| `shrink_one()` | — | ✅ |

### Debug / text
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_btree_cache_to_text()` | `bch2_btree_cache_to_text` | ⚠️ 输出使用 volmount 当前可得统计段，已补 clean/requested/freed/self reclaim 计数 |

### Cannibalize
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_btree_cache_cannibalize_lock()` | 同名 | ⚠️ Mutex vs cmpxchg |
| `bch2_btree_cache_cannibalize_unlock()` | 同名 | ⚠️ |
| `try_cannibalize_phase1/phase2()` | `btree_node_cannibalize` | ✅ |

### 节流控制
| 函数 | bcachefs 对应 | 状态 |
|------|---------------|------|
| `bch2_recalc_btree_reserve()` | 同名 | ⚠️ 简化 |
| `bch2_btree_cache_should_throttle()` | 同名 | ✅ |
| `bch2_btree_cache_update_throttle()` | 同名 | ✅ |

## P1/P2 差距（已关闭）
- P1: 5 态状态机 — ✅ 已实现 (`bch2_btree_node_transition_state`)
- P1: pinned 节点保护 — ✅ 已实现 (`bch2_node_pin` / `bch2_btree_cache_unpin`)
- P1: transition_state 原语 — ✅ 已实现 (NODE_ACCESSED 等位标志)
- P1: reclaim 锁协议 — ✅ 已实现 (`btree_node_reclaim`)
- P2: prefetch — ✅ 已实现 (`bch2_btree_node_prefetch` + iter 集成)
- P2: 异步 fill — ✅ 已实现 (`bch2_btree_node_fill`, sync=true/false)
- P2: eviction 等待 IO — ✅ 已实现 (`bch2_btree_node_evict` 先等 read/write in-flight)
- 2026-07-02: `system_memory_usage_high()` 已接到 `alloc_node_for_key()` / `bch2_btree_node_fill()`，在系统内存高压时先做 `shrink_one()` 再分配；Rust 侧已补上显式 `freeable` 池，helper 现在读取真实池大小而不再是占位 0。
- 2026-07-02: `bch2_btree_cache_should_throttle()` 现在会按当前 counters 刷新状态；`btree/transaction.rs::commit()` 已在低水位线下接入这条 gate，并通过 polling wait 等待 throttle 解除。

## Volmount 特有 (➖)
- `shrink_one()` — 简化版 shrinker
- `try_cannibalize_phase1/phase2()` — 两阶段 cannibalize
- `insert_dirty()` — 直接插入 dirty 列表
- `evict_one_leaf()` — leaf 优先驱逐辅助
- `alloc_node_for_key()` — 基于 BtreeKey 分配新节点
- `read_node_data()` — 后端读取节点数据，供 sync/async fill 复用
- `bch2_btree_node_prefetch_id()` — 基于 node_id 的预取变体
- `prefetch_node()` — NodeCache 预取委托

## src/cache/mod.rs — 遗留 stub
`crates/volmount-core/src/cache/mod.rs` (28 行) 为遗留占位，所有 cache 实现已在 `btree/cache.rs` 中。建议清理合并。

## P2 实现总结 (2026-06-30)

### 新增公开 API

| 函数 | 签名 | 用途 |
|------|------|------|
| `bch2_btree_node_prefetch_id` | `(node_id: u64, level: u8, _btree_id: BtreeId) -> bool` | 基于 node_id 的预取，用于 BtreeIter 下降路径 |
| `prefetch_node` | `(block_addr: u64, level: u8, btree_id: BtreeId) -> bool` | NodeCache 预取委托 |

### BtreeIter 集成

- `BtreeIter::init()` descent 循环：加载子节点后，预取下一个兄弟节点
- `back_up_and_advance()`：预取再下一个兄弟节点（readahead 风格）

### 🔥 关键学习：竞态条件与修复

**竞态 1：`bch2_btree_node_fill` InFlight 状态设置顺序**

```rust
// 🚫 错误：先 insert，再设置 InFlight（另一个线程可能获取到空节点）
let node = self.alloc_node_for_key(key, level, btree_id);
// 节点已在缓存中（state=Alive），下一个线程的 get_or_load 可见
node.transition_state(InFlight);   // ⚠️ 太晚了
node.set_read_in_flight();

// ✅ 正确：先设置 InFlight，再 insert（节点可见时即处于保护状态）
let node = Arc::new(BtreeNode::new(level));
node.transition_state(InFlight);
node.set_read_in_flight();
self.insert(node_id, node.clone());
```

**竞态 2：`get_or_load` 持有 inner.lock 期间等待**

```rust
// 🚫 错误：持有缓存全局锁（inner）时等待 Condvar
let mut inner = self.inner.lock().unwrap();
inner.dirty.get(&node_id).unwrap().wait_on_read(None);
// 整个缓存被锁住，其他线程无法操作

// ✅ 正确：先克隆 Arc 节点，释放 inner，再等待
let mut inner = self.inner.lock().unwrap();
let node = inner.dirty.get(&node_id).unwrap().clone();
drop(inner);
node.wait_on_read(None);
```

**一致性：`bch2_btree_node_get` 必须与 `get_or_load` 行为一致**

两个方法都返回缓存节点，都必须在命中路径中检查 `read_in_flight` 标志。`bch2_btree_node_get` 的三个命中路径（dirty/pending_flush/clean）已同步添加 `wait_on_read`。
