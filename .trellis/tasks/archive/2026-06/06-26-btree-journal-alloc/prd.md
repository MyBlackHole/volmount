# btree-journal-alloc 功能逻辑对齐修复

## Goal

修复 volmount-core 中 btree split、journal reclaim、alloc trigger 三个子系统与 bcachefs C 源码之间的功能逻辑差异，使实现从"API 命名对齐"提升到"功能逻辑一致"。

## Background

已完成 8 核心模块的 bcachefs API 100% 命名对齐（task 06-25-bcachefs-alignment）。复查发现 3 个复杂子系统的内部逻辑与 bcachefs 存在显著差异，可能在生产中导致数据不一致、资源泄漏、性能退化。

## Requirements

### P0 — 必须修复（数据安全/死锁风险）

1. **btree split: compact_fits 检查** — 防止 compact 无效时的死循环
2. **btree split: 错误路径资源回滚** — split 失败时释放已分配的节点
3. **btree split: format-aware split point** — 防止 split 后某侧仍满
4. **alloc trigger: 事务原子性** — Alloc btree + Freespace btree 两步写入要么全成功要么全回滚
5. **alloc trigger: 版本号一致** — extent trigger 写入的 alloc entry version 与 freespace gen 同步

### P1 — 应该修复（行为差异）

6. **journal reclaim: seq 分配粒度从 per-reservation 改为 per-entry** — 对齐 bcachefs
7. **journal reclaim: reclaim 触发 btree flush** — 回收时写回脏 btree node
8. **alloc: need_discard 状态支持** — bucket 释放后先设为 NeedDiscard，TRIM 后才 Free

### P2 — 值得修复（鲁棒性）

9. **btree: 75% 主动分裂阈值** — 减少大分裂冲击
10. **journal: pin_copy/pin_drop/pin_flush** — 完整体 pin 操作
11. **alloc: freespace 重建覆盖 hole** — 扫描没有 alloc entry 的 bucket 范围
12. **journal: 错误处理增强** — stuck 检测 + 精细错误类型

## Constraints

- 不改动未发现问题模块（snap/subvol/recovery/lock/volume）
- 不改动 volmountd/volmount/volmount-nbd 的公开 API
- 每次修改后 `cargo build` + `cargo test -p volmount-core --lib` + `cargo clippy` 通过
- 不引入新的外部依赖
- 合并/优化而非增加代码量（控制行数增长不超过 20%）

## Scope

**In scope**:
- `btree/btree.rs` — split 路径的 compact_fits、错误回滚、75% 阈值
- `btree/node.rs` — find_balanced_split 的 format-aware 预测
- `btree/interior.rs` — btree_split/merge 的事务完整性
- `journal/types.rs` — seq 分配、reclaim 路径、pin 系统、slowpath
- `journal/mod.rs`, `journal/replay.rs` — 相关引用更新
- `alloc/mod.rs` — trigger 原子性、版本号、need_discard
- `alloc/btree.rs` — BchAllocEntry 类型可能需要扩展

**Out of scope**:
- 完整的 btree trans 事务系统（需要独立设计）
- 后台线程框架（journal reclaim thread 的设计需要独立 task）
- 多设备支持
- 跨模块重构

## Acceptance Criteria

- [ ] P0 全部 5 项修复完成，对应测试验证通过
- [ ] P1 至少完成 2 项（#6 和 #8 优先）
- [ ] `cargo build` 通过，0 新 warnings
- [ ] `cargo test -p volmount-core --lib` 通过（不低于 625）
- [ ] `cargo clippy --all-targets` 干净
- [ ] spec/quality-guidelines.md 中的 bcachefs 对齐原则更新
- [ ] task 归档
