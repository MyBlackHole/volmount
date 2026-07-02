# API 对齐 bcachefs

## Goal

将 **volmount-core 库的 Rust 内部 API**（btree、事务、快照、空间分配器、锁等核心模块）与 bcachefs 内核中对应子系统的接口约定对齐。

## 背景

Volmount 项目的 13 份设计文档均采用 "对照 bcachefs 架构" 格式，已有 bcachefs 移植成果如下：
- **SixLock** — 已移植 bcachefs six_lock（读/意向/写三态锁，三级等待）
- **BTree** — 基于 bcachefs btree 模型的 COW BTree（Bpos/BtreeKey/BtreeNode/Bset）
- **Alloc** — 基于 bcachefs bucket 的空间分配器
- **Journal** — 基于 bcachefs journal（双缓冲流水线）
- **Snapshot** — 基于 bcachefs snapshot skiplist/Δ2 快照指针
- **Subvolume** — 基于 bcachefs 子卷模型

本次任务聚焦于**这些核心模块的 Rust 函数签名、类型命名、模块结构和调用约定**是否与 bcachefs 内核 C 代码中的对应接口一致。

**注意**：用户已明确范围限定在 volmount-core 核心库的 Rust API，**不包括** HTTP API（volmountd）或 CLI（volmount）的调整。

## 已确认事实（调研结果）

### volmount-core 核心模块结构与初步评估

| 模块 | bcachefs 参考文件 | 对齐关注点 |
|------|------------------|-----------|
| `lock/six.rs` | `fs/bcachefs/six.h` | CAS 命名、try/spin/sleep 三级约定、waiting_bit 语义、lock_read/lock_intent/lock_write 签名 |
| `btree/` | `fs/bcachefs/btree.h`, btree_types.h, btree_update.h | btree_trans, btree_iter, btree_key 类型名、事务开始/提交/回滚函数、锁升级/降级 |
| `snap/snapshot.rs` | `fs/bcachefs/snapshot.h` | snapshot_id 类型、创建/删除/回滚函数签名、skiplist/skip[3] 实现 |
| `subvol/` | `fs/bcachefs/subvolume.h` | subvolume_create/destroy/list 签名 |
| `alloc/` | `fs/bcachefs/alloc_background.h`, alloc_foreground.h | bucket 状态管理、open_bucket、写入点 |
| `journal/` | `fs/bcachefs/journal.h` | journal_buf、journal_entry、commit_with_journal、close_entry 签名 |
| `volume/` | `fs/bcachefs/fs.h` | volume_create/delete/resize |
| `types.rs` | `fs/bcachefs/bcachefs.h` | 全局类型名对齐（BlockAddr, Bpos 等） |
| `recovery/` | `fs/bcachefs/recovery.h` | recovery passes 阶段顺序、接口签名 |

### 已知的 bcachefs 对齐差距（来自设计文档）

- **BTree**: key_cache、packed bpos 直接比较、Eytzinger 二分查找、快照祖先缓存、GC 离线检查
- **SixLock**: 设计文档标注为基本对齐，但需检查 bcachefs 最新版本变化
- **Snapshot**: `skip_list` 测试已知失败，有对齐差距
- **Journal**: 双缓冲流水线 vs bcachefs 最新 journal 差异
- **Alloc**: bucket 管理粒度差异

### 测试依赖

- `cargo test -p volmount-core --lib` — 492 个测试
- 已知预存失败：snap::skip_list (skip list ordered)、subvol::create_multiple、subvol::create_snapshot_subvolume
- `cargo test -p volmount-core --lib -- --ignored lock` — 6 个锁压力测试

## 审计要求

1. 以 bcachefs-tools `v1.38.6-36-g499dbe7e0`（本地 `/home/black/Documents/bcachefs-tools/`）的 `fs/` 内核源码为参考基准
2. 按 btree → snap → subvol → lock → alloc → journal → volume → types → recovery 顺序审计
3. 每个模块覆盖：类型定义、函数签名、调用约定、命名规范四个维度
4. 每个差距标注严重程度（P1/P2/P3）和重构成本估计
5. 区分"合理 Rust 化差异"（有意为之）和"偏差"（需要修复）

## Acceptance Criteria

- [ ] 9 个模块全部完成审计
- [ ] 差距分析写入 `design.md`（每个模块一章）
- [ ] 每个差距标注了 P1/P2/P3 严重等级和估算修复成本
- [ ] 产出汇总统计：对齐数 / 微小偏差数 / 架构差异数
- [ ] 用户审阅了完整差距报告并做出后续决策

## Out of Scope

- HTTP API（volmountd）的设计或重构
- CLI（volmount）子命令的调整
- volmount-nbd crate
- 任何前端/用户界面
- 对外部 IOCTL 接口的兼容性

## Open Questions

1. **对齐力度**：应该检查到哪种级别？
   - (a) 函数签名和类型命名对齐（public API surface 一致），不改算法逻辑
   - (b) 签名 + 模块组织对齐，不一致的地方记录差距
   - (c) 全量对齐：逐一对比实现，修复所有偏差
2. **模块优先级**：8 个核心模块中哪个最先检查？
3. **重构容许度**：发现命名不匹配时直接 rename 重构，还是仅记录差距文档？
4. **已知失败测试**：是否纳入对齐范围一并修复？
