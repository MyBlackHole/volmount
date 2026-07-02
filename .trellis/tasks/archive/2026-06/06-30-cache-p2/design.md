# Design: btree-cache P2 — prefetch + async fill

## 架构决策

### AD-1: fill 作为内部原语，prefetch 作为封装
- `bch2_btree_node_fill` 是底层异步加载原语，signature 与 bcachefs-tools 对齐
- `bch2_btree_node_prefetch` 是 `fill(sync=false)` 的 fire-and-forget 封装
- 同步填充 (`sync=true`) 不会立即实现；当前 `get_or_load()` + `load_fn` 已覆盖

### AD-2: fill 的异步模型使用闭包而非线程池
- volmount 当前是同步库（无 tokio/async runtime）
- fill 的"异步"指：发起读后不等待，标记节点为 InFlight
- 调用方通过 `get_or_load()` 遇到 InFlight 节点时等待（条件变量或轮询）
- 读完成回调：transition_state → Alive → unlock

### AD-3: 不需要独立的新状态机
- `NodeState` 已有 `Alive | Deleting | InFlight | Reclaim`（node.rs:370）
- fill 复用 InFlight 状态
- 不需要 `BtreeNodeCacheState` 5 态（volmount 简化版）

## API 设计

### `BtreeCache` 新增方法

```rust
/// bcachefs 对齐: bch2_btree_node_fill — 异步或同步加载 btree 节点
///
/// 对应 bcachefs-tools `bch2_btree_node_fill()` (cache.c:1098):
/// - alloc node → set key/level/btree_id → transition_state(CLEAN) →
///   set read_in_flight → read data → clear read_in_flight
/// - sync=true: 同步等待读完成
/// - sync=false: fire-and-forget，读完成后 unlock + notify
pub fn bch2_btree_node_fill(
    &self,
    key: &BtreeKey,
    btree_id: BtreeId,
    level: u8,
    sync: bool,
) -> Result<Arc<BtreeNode>, Error>;

/// bcachefs 对齐: bch2_btree_node_prefetch — 预读 btree 节点
///
/// 对应 bcachefs-tools `bch2_btree_node_prefetch()` (cache.c:1575-1595):
/// - 若节点已在 cache 中 → 空操作
/// - 否则 → bch2_btree_node_fill(sync=false) → unlock_read
pub fn bch2_btree_node_prefetch(
    &self,
    key: &BtreeKey,
    btree_id: BtreeId,
    level: u8,
) -> bool;
```

### 内部辅助

```rust
/// 内部：从 BtreeKey（btree_ptr 格式）分配并初始化新节点
fn alloc_node_for_key(&self, key: &BtreeKey, level: u8, btree_id: BtreeId) -> Arc<BtreeNode>;

/// 内部：读取节点数据并完成初始化
fn read_node_data(node: &BtreeNode, key: &BtreeKey) -> Result<(), Error>;
```

## Fill 流程

```
bch2_btree_node_fill(key, btree_id, level, sync):
  1. alloc_node_for_key(key, level, btree_id)  // 分配 + 设置 key/level/btree_id
  2. transition_state(InFlight)                 // 标记为正在加载
  3. set_read_in_flight 标志                     // bcachefs 对齐
  4. if sync:
        read_node_data(node, key)               // 同步等待
        transition_state(Alive)
        clear_read_in_flight
        return node
     else:
        spawn_reader(node, key)                 // 后台线程/闭包
        return node                             // 立即返回，状态为 InFlight
```

## Prefetch 流程

```
bch2_btree_node_prefetch(key, btree_id, level):
  1. if 节点已在 cache → return false      // 已有，无需预取
  2. bch2_btree_node_fill(sync=false)
  3. if 填充成功 → unlock_read()            // fire-and-forget
  4. return true                           // 发起了预取
```

## 集成点

### BtreeIter::init() 中的 prefetch 调用
在 `descend()` 循环中，找到下一级 key 后调用 `bch2_btree_node_prefetch`：

```rust
// 伪代码 — 在 BtreeIter::descend() 的叶查找循环中
if let Some(next_key) = bset_search(...) {
    // 异步预取下一个兄弟节点（若不在 cache 中）
    cache.bch2_btree_node_prefetch(&next_key, btree_id, level);
    // 同步获取当前节点
    let node = cache.bch2_btree_node_get(next_key, ...)?;
}
```

### get_or_load 的 wait-on-InFlight
当前 `get_or_load` 遇到缺失节点时直接调用 `load_fn`。扩展为先检查节点是否存在，若存在但 InFlight 则等待：

```rust
pub fn get_or_load<F>(&self, node_id: u64, load_fn: F) -> Arc<BtreeNode> {
    // 1. 检查 cache
    if let Some(node) = self.inner.lock()... {
        // 2. 若 InFlight → 等待 fill 完成
        wait_for_read_complete(&node);
        return node;
    }
    // 3. 缺失 → load_fn（同步，已有）
    ...
}
```

## 并发模型

- fill 的异步读使用 `std::thread::spawn` 或共享 IO 线程池
- 读完成时通过 `Condvar` 或 `AtomicBool` + `notify` 通知等待者
- volmount 当前为同步模型，暂不引入 async runtime

## bcachefs 对齐映射

| volmount | bcachefs-tools | 文件:行 |
|---------|---------------|---------|
| `bch2_btree_node_prefetch` | `bch2_btree_node_prefetch()` | cache.c:1575 |
| `bch2_btree_node_fill` | `bch2_btree_node_fill()` | cache.c:1098 |
| `set_read_in_flight` | `set_btree_node_read_in_flight()` | cache.c:1174 |
| `NodeState::InFlight` | `BTREE_NODE_READ_IN_FLIGHT` | cache.h |
| `spawn_reader` | `bch2_btree_node_read()` | cache.c:586 |
