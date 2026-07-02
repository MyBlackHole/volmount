# bcachefs follow-up alignment

## Goal

继续推进 volmount 对 bcachefs 的对齐工作，先把 `btree/cache.rs` 里的 `system_memory_usage_high()` 对齐到 upstream 的系统内存压力判定语义，并写回 spec。

## Confirmed Facts

- 当前仓库没有活跃 Trellis 任务，这次已新建 `07-02-bcachefs-follow-up-alignment-2` 作为新的跟进任务。
- 近期已完成并归档的对齐点覆盖了 journal dirty idx 边界、bucket_gens_init 和若干 recovery / backend 相关条目。
- spec 里仍然存在明显的未对齐区域，尤其是 `backend/btree-cache-coverage.md` 中的多个 `⚠️` 项。
- `btree/cache.rs` 中的 `system_memory_usage_high()` 仍使用本地固定阈值判断，和 bcachefs 的 `si_mem_available()` / cache footprint 判定不一致。

## Requirements

- 选定一个具体的 bcachefs 对齐点，避免只做泛化的“继续”。
- 实现必须保持和现有 volmount 语义兼容，不能回退已完成的 journal / recovery 对齐。
- 对应代码改动需要能通过定向测试或等价验证。
- 若引入新的通用约束或行为结论，需要同步到 `.trellis/spec/`。
- 这轮只处理 `system_memory_usage_high()` 这一个差异点，不扩展到完整 shrinker 或 memory management 集成。

## Acceptance Criteria

- [ ] 选定并记录一个明确的对齐目标。
- [ ] `system_memory_usage_high()` 已按 upstream 语义调整并通过验证。
- [ ] 必要的 spec 更新已完成。
- [ ] 任务可以进入 `in_progress`。

## Notes

- `system_memory_usage_high()` 采用 upstream 风格的“可用系统内存 + cache footprint”判定，而不是固定的本地节点数阈值。
- 保持当前 shrink / eviction 行为不变，仅收敛判断函数本身与相关文档。
