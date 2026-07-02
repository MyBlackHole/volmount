# Journal 层规范

## 文件

| 文件 | 内容 | 适用场景 |
|------|------|---------|
| `reclaim.md` | Pin API 模式（UnsafeCell 内变、_seq 过渡、方法命名、测试） | 修改 reclaim.rs、types.rs 中 pin 相关代码时 |

## 质量检查

修改 journal/ 代码后，确认：

- [ ] `cargo test -p volmount-core --lib` 通过（941 passed, 0 failed, 0 ignored）
- [ ] `cargo clippy -p volmount-core` 无新 error/warning
- [ ] `cargo fmt --check` clean
- [ ] pin API 测试全部通过：`cargo test -p volmount-core --lib -- test_pin`
