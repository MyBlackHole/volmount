# PRD: btree-cache P2 — prefetch / async fill / MM shrinker

## Goal
补齐 volmount 中 `btree/cache.rs` 的三个 bcachefs P2 功能缺口，使 BtreeCache 达到 90%+ 的 bcachefs 功能覆盖。

## 确认的事实（来自代码审查）

### 已实现的前置功能（无需再做）
- ✅ `bch2_btree_node_transition_state` — `node.rs:375`
- ✅ `bch2_node_pin` / `bch2_btree_cache_unpin` — `cache.rs:544,554`
- ✅ `btree_node_reclaim` — `cache.rs:701`（委托 shrink_one）
- ✅ `system_memory_usage_high` — `cache.rs:710`（阈值判断）
- ✅ `bch2_fs_btree_cache_init` / `exit` — `cache.rs:719,725`（空操作）
- ✅ 两阶段 clock shrinker — `cache.rs:370`（`shrink()`）
- ✅ cannibalize lock — `cache.rs:572-654`
- ✅ 拓扑排序 flush_dirty — `cache.rs:266`
- ✅ Root/leaf 热冷分离 — `cache.rs` 设计内置

### 实际缺失的 P2 功能（3 项）

| 功能 | bcachefs 函数 | 说明 |
|------|--------------|------|
| Prefetch | `bch2_btree_node_prefetch` | 树下降时预读子节点（fire-and-forget `fill(sync=false)`） |
| Async fill | `bch2_btree_node_fill` | 异步从磁盘加载 btree 节点 |
| MM shrinker | ➖ (userspace 不适用) | 已有 `shrink()` + `cannibalize` + `system_memory_usage_high()` 足够 |

## 需求

### 1. Prefetch (`bch2_btree_node_prefetch`)
- **功能**：在 `BtreeIter` 路径遍历时，检测缺失子节点并发起 fire-and-forget 填充
- **bcachefs-tools 参考**：`bch2_btree_node_prefetch()` (cache.c:1575-1595) → `bch2_btree_node_fill(sync=false)` + immediate `unlock_read`
- **触发时机**：`BtreeIter::descend()` 或等价路径遍历中，发现下级节点不在 cache 时
- **验收标准**：
  - API 存在：`BtreeCache::bch2_btree_node_prefetch(key, btree_id, level)`
  - 发起 fill 后立即返回（fire-and-forget），不阻塞当前操作
  - 节点已在 cache 中时，prefetch 是空操作

### 2. Async fill (`bch2_btree_node_fill`)
- **功能**：`BtreeNode` 数据从磁盘加载的异步版本
- **bcachefs-tools 参考**：`bch2_btree_node_fill()` (cache.c:1098-1573)
- **核心流程**：alloc node → set key/level/btree_id → transition_state → set read_in_flight → read data → clear read_in_flight
- **验收标准**：
  - 填充后节点状态正确（`InFlight` → `Alive`）
  - 不泄漏锁
  - `sync=true`：同步等待读完成
  - `sync=false`：fire-and-forget，读完成后自动释放锁

## 范围外
- rhashtable 替换 `Mutex<HashMap>`（P3）
- 多队列 IO scheduler
- MM shrinker 集成（userspace 无内核 shrinker 机制）
- 系统级 memory pressure 监控

## 待定问题

问题 #1：**prefetch 的触发时机？** — 已确认 bcachefs-tools 行为：
- `bch2_btree_node_prefetch()` 在 `bch2_btree_path_traverse_all()` 中调用
- 每次路径下降/上升时，若下级节点不在 cache 中则发起 prefetch
- 对应 `BtreeIter::init()` 或 `descend()` 时机
- **结论**：在 `BtreeIter::init()` 的内部 descent 循环中调用

问题 #2：**MM shrinker 在 userspace 的含义？**
- bcachefs 内核使用 `register_shrinker()` 注册系统内存压力回调
- volmount 是 userspace Rust 库，无内核 shrinker 机制
- 等价方案：已有 `shrink()`, `system_memory_usage_high()` + 周期性/显式调用
- 需要决定：是否添加一个"建议收缩"的外部回调注册接口
