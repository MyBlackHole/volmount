# 剩余子系统功能逻辑审查

## Goal

对已完成 API 命名对齐但尚未做功能逻辑审查的 6 个子系统进行逐行对比审查，通过对比 bcachefs C 源码发现实现差异，记录严重度并规划修复。

## Background

已完成：
- 8 核心模块 API 100% 对齐（task 06-25-bcachefs-alignment）
- 3 高风险子系统的功能逻辑修复（task 06-26-btree-journal-alloc）
- btree split / journal reclaim / alloc trigger 已审查并修复

仍待审查的子系统（API 已对齐，内部逻辑未审）：

## Scope

### In scope

| # | 子系统 | 文件 | bcachefs 参考 |
|---|--------|------|--------------|
| 1 | **btree trans/iter** | `btree/transaction.rs`, `btree/iter.rs` | `btree/iter.c`, `btree/trans.c` |
| 2 | **btree cache/IO** | `btree/cache.rs`, `btree/io.rs`, `btree/bucket_io.rs` | `btree/io.c`, `btree/bcache.c` |
| 3 | **btree GC** | `btree/gc.rs` | `btree/gc.c` |
| 4 | **journal flush/write/read** | `journal/types.rs`（flush/write/read 路径） | `journal.c`, `journal_io.c` |
| 5 | **lock six** | `lock/six.rs` | `six.c` |
| 6 | **recovery** | `recovery/`（passes 目录 + 主模块） | `recovery.c`, `recovery_passes.c` |

每项的审查输出：差异列表（严重度 P0/P1/P2）、是否需要修复、修复方案建议。

### Out of scope
- btree key types（纯数据格式，已对齐且稳定）
- snapshot/subvol（API 已对齐，功能逻辑相对简单）
- volume/storage/NBD/HTTP/CLI（非核心或未对齐目标）

## Requirements

### 审查方式

采用 **explore agent 并行对比审查**模式（与 06-26-btree-journal-alloc 的前期审查一致）：
1. 每个子系统配 1 个 explore agent，给定 bcachefs 参考路径 + volmount 实现路径
2. 输出：按严重度分级的差异项表格
3. 关键对比维度：
   - **边界条件**：空树/满树/单节点/并发极端
   - **错误处理**：是否遗漏错误码、panic 代替 error return
   - **并发语义**：锁类型/顺序、原子操作、内存顺序
   - **行为一致性**：触发条件、阈值、回退策略

### 输出格式

```
## [子系统名] — 功能逻辑差异

### P0 (数据安全/死锁)
- [描述] — [volmount 行为] vs [bcachefs 行为] — [建议修复方案]

### P1 (行为差异)
- ...

### P2 (鲁棒性)
- ...
```

### 修复约束
- 本次只审查不修复（PRD + 设计文档）
- 修复作为后续独立 task 执行
- 审查结果记录到 `design.md`

## Acceptance Criteria

- [ ] 6 个子系统全部完成逐行对比审查
- [ ] 每个子系统输出差异项分级列表（P0/P1/P2）
- [ ] 所有 P0 项标记是否需要立即修复
- [ ] `design.md` 汇总审查结果
- [ ] spec/quality-guidelines.md 更新审查发现的约定
- [ ] task 归档
