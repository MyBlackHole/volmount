# API 差距审计设计

## 审计目标

系统性地将 volmount-core 每个核心模块的 Rust 公开 API 与 bcachefs 内核 C API 进行对比，产出每个模块的差距文档（gap analysis），供后续决策是否/如何重构。

## 参考源

| 源 | 路径 | 用途 |
|---|------|------|
| **bcachefs 内核源码** | `/home/black/Documents/bcachefs-tools/fs/` | API 参考标准。含 `bcachefs.h`、`bcachefs_format.h`、各子系统 `.h` 头文件 |
| **bcachefs 版本** | `v1.38.6-36-g499dbe7e0`（commit `499dbe7e0`） | 冻结版本，审计期间不随 upstream 变动 |
| **volmount-core** | `crates/volmount-core/src/` | 被审计方 |
| **已有设计文档** | `docs/*.design.md` | 已有的对齐分析（已包含部分差距），作为起点而非终点 |

## 审计范围（模块及顺序）

1. **btree**（含 key/transaction/node/cache/iter）
2. **snap**（snapshot）
3. **subvol**（subvolume）
4. **lock**（six）
5. **alloc**
6. **journal**
7. **volume**
8. **types**（全局类型定义）
9. **recovery**（恢复流程）

## 每个模块的审计条目

对每个模块，逐一检查以下维度：

### A. 模块结构

| 检查项 | 说明 |
|--------|------|
| 顶级模块名 | Rust mod 名是否对应 bcachefs 子系统名 |
| 子模块拆分 | 内部模块组织是否反映 bcachefs 的文件划分 |
| 公开可见性 | `pub` / `pub(crate)` 范围是否合理 |
| re-export 策略 | `lib.rs` 或 `mod.rs` 的 re-export 是否与 bcachefs include 结构一致 |

### B. 类型定义

| 检查项 | 说明 |
|--------|------|
| 核心类型名 | Rust struct/enum 名是否对应 bcachefs C struct/enum 名 |
| 字段名与顺序 | 字段名称、类型、排布是否与 bcachefs 一致（含 repr 标注） |
| 字段类型映射 | Rust 类型选择是否合理映射 C 类型（u32/u64/enum/bitflags） |
| 枚举变体 | enum 变体名和值是否对应 bcachefs 常量/枚举 |
| trait 约束 | 类型上的 `Clone`/`Copy`/`Debug`/`PartialEq`/`Eq` 等派生是否合理 |

### C. 函数签名

| 检查项 | 说明 |
|--------|------|
| 函数名 | `pub fn` 名是否对应 bcachefs C 函数名 |
| 参数顺序 | 参数顺序是否与 bcachefs 一致 |
| 参数类型 | 参数是否为对应的类型（Rust 习惯 vs bcachefs C 习惯） |
| 返回类型 | `Result<T, E>` / `Option<T>` / 直接值 的选择是否合理 |
| 错误类型 | 错误枚举变体是否覆盖 bcachefs 错误码 |
| 生命周期标注 | `'a` / `'b` 标注位置是否与所有权模型一致 |
| async/sync | 是否与 bcachefs （同步）模型一致 |

### D. 调用约定

| 检查项 | 说明 |
|--------|------|
| 可变引用 vs 所有权 | 参数 `&self` / `&mut self` / `self` 选择 |
| 错误处理 | Rust `Result` vs bcachefs `int` return + ERR_PTR |
| 可选参数 | Rust `Option<T>` vs bcachefs `NULL` |
| flag 参数 | Rust bitflags 类型 vs bcachefs `u64` flag 位 |
| 回调/闭包 | Rust `Fn`/`FnMut` vs bcachefs 函数指针 |
| 迭代器 | Rust `Iterator` trait vs bcachefs 手动遍历 |

### E. 命名规范

| 检查项 | 说明 |
|--------|------|
| 命名风格 | bcachefs 使用 `snake_case`（C），Rust 也 `snake_case` — 主要检查前缀/后缀 |
| 缩写 | bcachefs 常用缩写：`bch2_` / `bkey` / `bpos` / `btree_trans` / `sb` 等 |
| 模块前缀 | bcachefs 函数有 `bch2_` 前缀，volmount 是否使用等价前缀或模块路径 |

## 输出格式

每个模块的差距分析产出格式：

```markdown
## 模块: btree

### bcachefs 参考文件
- `fs/btree.h`
- `fs/btree_types.h`
- `fs/btree_update.h`

### volmount 对应文件
- `crates/volmount-core/src/btree/`

### 类型对齐

| Rust 类型 | C 类型 | 状态 | 差距 |
|-----------|--------|------|------|
| `Bpos` | `struct bpos` | ⚠️ 字段顺序不同 | vol: (vol_id, offset, snapshot); bcachefs: (snapshot, offset, inode) — 影响 memcmp 优化 |

### 函数对齐

| Rust fn | C fn | 状态 | 差距 |
|---------|------|------|------|
| `BtreeEngine::commit_transaction()` | `bch2_btree_trans_commit()` | ✅ 已对齐 | — |

### 总结
- **已对齐**: N 项
- **微小偏差**: M 项（命名/参数顺序等）
- **架构差异**: K 项（需用户决策是否修复）
```

## 审计执行方法

每个模块的审计步骤：
1. 读取 bcachefs 对应 `.h` 头文件（或 `.c` 中的公开函数），提取 API 列表
2. 读取 volmount-core 对应 `src/` 文件，提取所有 `pub` / `pub(crate)` API
3. 按上述维度逐项对比
4. 输出差距分析到 `design.md` 对应章节
5. 记录每个差距的：严重程度（P1/P2/P3）、重构成本估计、是否可自动化（rename/type-migrate）

## 严重程度定义

| 级别 | 定义 | 示例 |
|------|------|------|
| **P0** | 必现正确性/安全错误 | journal→btree pin 通路缺失、rewind 死循环、无校验和 |
| **P1** | 函数语义不同会导致正确性问题或性能关键路径偏差 | bpos 字段顺序/BtreeKey 编码流不兼容 |
| **P2** | 命名/结构不一致但功能等价，影响可读性和可维护性 | 函数名与 bcachefs 不同但做同一件事 |
| **P3** | 有 bcachefs 功能而 volmount 缺失，不影响现有功能 | 缺少某个辅助方法或 convenience API |

---

## 9 个模块审计汇总

### 文件清单

| 模块 | 审计文件 | 大小 |
|------|---------|------|
| 1. btree | `audit-btree.md` | 33.8KB |
| 2. snap | `audit-snap.md` | 14.8KB |
| 3. subvol | `audit-subvol.md` | 14.3KB |
| 4. lock | `audit-lock.md` | 20.6KB |
| 5. alloc | `audit-alloc.md` | 18.4KB |
| 6. journal | `audit-journal.md` | 29.1KB |
| 7. volume | `audit-volume.md` | 16.0KB |
| 8. types | `audit-types.md` | 35.8KB |
| 9. recovery | `audit-recovery.md` | 16.7KB |
| **合计** | **9 份报告** | **~199KB / 3248 行** |

### 跨模块严重度统计

| 严重度 | btree | snap | subvol | lock | alloc | journal | volume | types | recovery | **合计** |
|:------:|:-----:|:----:|:------:|:----:|:-----:|:-------:|:------:|:-----:|:--------:|:-------:|
| **P0** | — | — | — | — | — | 1 | — | — | 3 | **4** |
| **P1** | 13 | 8 | 14 | 13 | 28 | 1 | 33 | 23 | 5 | **138** |
| **P2** | 24 | 16 | 10 | 31 | 28 | 1 | 44 | 24 | 3 | **181** |
| **P3** | 32 | 23 | 9 | 21 | 30 | — | 7 | 17 | 2 | **141** |
| P4 | — | — | — | — | — | — | — | 8 | — | **8** |

### 跨模块 Top Priority 行动项

基于全部 9 份审计报告，以下是优先级最高的跨模块修复项：

#### P0 — 必须立即修复（4 项）

| # | 模块 | 问题 | 描述 |
|---|------|------|------|
| P0-1 | **journal** | btree→journal pin 通路缺失 | Btree 脏节点无法 pin journal，`last_seq_ondisk` 推进不考虑 btree |
| P0-2 | **recovery** | 无 rewind 机制 | 恢复 pass 失败后无法回滚重跑，可能死循环 |
| P0-3 | **recovery** | passes_failing 跟踪缺失 | 无法在连续失败 pass 后降级为只读 |
| P0-4 | **volume** | 无数据校验和 | BchSb/VolumeMeta 写坏不可检测，无完整性保护 |

#### P1 Top 10 — 建议优先修复（按影响面排序）

| # | 模块 | 问题 | P1 合计数/模块 |
|---|------|------|:-------------:|
| 1 | **btree** | Bpos 字段顺序与 bcachefs 不一致（vol_id 应为 inode，snapshot 应在高位） | 13 |
| 2 | **btree** | BKEY_NR_FIELDS 5 vs 6，磁盘格式不兼容 | (同 btree) |
| 3 | **snap** | 无内存 snapshot 表，每次祖先查询读 btree（~100x 慢） | 8 |
| 4 | **snap** | is_ancestor 128-bit bitmap 从未填充 | (同 snap) |
| 5 | **subvol** | 子卷创建不操作 Snapshots btree（子卷与快照未关联） | 14 |
| 6 | **alloc** | 数据类型仅 4 种（vs bcachefs 11 种），分配入口无 alloc_request 抽象 | 28 |
| 7 | **alloc** | 无 disk_reservation 系统 | (同 alloc) |
| 8 | **volume** | 无状态标志系统（started/rw/error/stopping），无 shutdown 协议 | 33 |
| 9 | **types** | BtreeId 仅覆盖 21% bcachefs btree 类型 | 23 |
| 10 | **recovery** | Pass 数量仅 5 个（vs ~50），无 fsck 级检查 | 5 |

### 对齐度最佳模块

| 模块 | 评估 | 理由 |
|------|------|------|
| **Lock (SixLock)** | 🟢 **最佳** | 三种锁类型、原子状态位布局、try→spin→sleep 模式完全对齐，设计文档标注准确 |
| **Journal** | 🟡 **中等** | JournalResState 位域布局和 CAS fastpath 正确，但缺少慢路径、pin flush、状态机 |
| **BTree** | 🟡 **中等** | 核心算法对齐（分裂合并/事务重启/路径缓存），但 Iter/Path 不分离、事务提交路径简化 |
| **Snapshot** | 🔴 **偏差大** | 无内存表/w bitmap/事务支持，创建语义不匹配 |

### 已知测试失败关联

| 测试 | 关联审计 | 根因 |
|------|---------|------|
| `snap::skip_list` | snap | `build_skip_list_from_btree()` steps 索引映射错误，skip[0]/skip[2] 互换 |
| `subvol::create_multiple` | subvol | 子卷创建无事务包裹，并发场景 ID 分配冲突 |
| `subvol::create_snapshot_subvolume` | subvol + snap | 子卷创建不创建 snapshot 节点，snapshot ID 未链接到 Snapshots btree |

### 参考源确认

- **bcachefs 版本**: v1.38.6-36-g499dbe7e0 (commit `499dbe7e0`)
- **本地路径**: `/home/black/Documents/bcachefs-tools/fs/`
- **审计期间**: 2026-06-24
