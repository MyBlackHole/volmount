# Implementation: btree-cache P2 — prefetch + async fill

## 实现清单

### Step 1: 在 `BtreeNode` 中添加 read_in_flight 标志
- **文件**: `crates/volmount-core/src/btree/node.rs`
- **内容**: 
  - `read_in_flight: AtomicBool` 字段
  - `set_read_in_flight()` / `clear_read_in_flight()` 方法
  - `wait_on_read(Condvar)` 方法
- **bcachefs 对齐**: `set_btree_node_read_in_flight()` (cache.c:1174)

### Step 2: `alloc_node_for_key` 内部方法
- **文件**: `crates/volmount-core/src/btree/cache.rs`
- **内容**:
  - 从 `BtreeKey` 分配新 `BtreeNode`
  - 设置 `key`, `level`, `btree_id`
  - 返回 `Arc<BtreeNode>`
  - 插入内部分发表（类似 `get_or_load` 的 key-based lookup）

### Step 3: `bch2_btree_node_fill` 方法
- **文件**: `crates/volmount-core/src/btree/cache.rs`
- **内容**:
  - `sync=true`: 分配 → transition(InFlight) → read → transition(Alive) → return
  - `sync=false`: 分配 → transition(InFlight) → spawn 读线程 → return InFlight 节点
- **bcachefs 对齐**: bch2_btree_node_fill() (cache.c:1098-1573)

### Step 4: `bch2_btree_node_prefetch` 方法
- **文件**: `crates/volmount-core/src/btree/cache.rs`
- **内容**:
  - 检查 cache → 有则空操作
  - 无则 `fill(sync=false)` → `unlock_read()`
- **bcachefs 对齐**: bch2_btree_node_prefetch() (cache.c:1575-1595)

### Step 5: `get_or_load` 增加 InFlight 等待
- **文件**: `crates/volmount-core/src/btree/cache.rs`
- **内容**:
  - `get_or_load` 中，若节点存在但 `read_in_flight` → `wait_on_read`
  - 不需要修改 `load_fn` 签名

### Step 6: 在 `BtreeIter` 下降中集成 prefetch
- **文件**: `crates/volmount-core/src/btree/iter.rs`
- **内容**:
  - 在 `init()` 的节点查找/下降循环中
  - 找到兄弟 key 后调用 `bch2_btree_node_prefetch`

### Step 7: 测试
- 新增测试：prefetch fire-and-forget 不阻塞
- 新增测试：fill sync 返回正确节点
- 新增测试：fill async 等待完成后可读取
- 新增测试：get_or_load 等待 InFlight 节点

### Step 8: 文档更新
- 更新 `btree-cache-coverage.md`：❓ 归零
- 更新 `bcachefs-alignment-guide.md`：P2 进度

## 验证命令

```bash
cargo test -p volmount-core
cargo clippy -p volmount-core
```

## 风险文件
- `cache.rs` (1691 行) — 核心修改
- `node.rs` (2590 行) — 加 read_in_flight
- `iter.rs` — prefetch 调用点

## 回溯点
- Step 1 完成后可独立验证
- Step 3 (fill) 完成后可通过 `sync=true` 路径独立测试
- Step 5 (get_or_load InFlight 等待) 是风险最大的一步
